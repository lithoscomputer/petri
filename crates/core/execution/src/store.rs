//! The coordinator's side of the store seam: the lifecycle log and the
//! graph registry over a [`RunLogs`] handle.
//!
//! The backend stores opaque records and blobs. Everything Petri decides
//! stays here: the run format version, checked on the run declaration
//! before any record is read; the coordinator state machine, which checks
//! an event before its record is appended and applies it once the record
//! is durable; and the graph registry, which digests, decodes and validates
//! every registered graph.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use driver::RecordingClock;
use ir::Graph;
use serde::de::Error as _;
use serde_json::Value;
use store::{Access, LogId, Record, RunDirStore, RunKey, RunLogs};

use crate::{
    COORDINATOR_FORMAT_VERSION, CoordinatorEvent, CoordinatorRecord, CoordinatorState, GraphDigest,
    InvocationId, StateError,
};

/// Why the coordinator's durable record could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The backend refused or failed.
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error("the {log} log holds a record at seq {seq} this build cannot decode: {source}")]
    BadRecord {
        log:    LogId,
        seq:    u64,
        #[source]
        source: serde_json::Error,
    },
    #[error("run format {found} is unsupported; expected {expected}")]
    UnsupportedFormat { found: u32, expected: u32 },
    #[error(transparent)]
    State(#[from] StateError),
    #[error("could not encode durable state: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("registered graph {0} is missing")]
    MissingGraph(GraphDigest),
    #[error("registered graph {expected} hashes to {found}")]
    GraphDigest {
        expected: GraphDigest,
        found:    GraphDigest,
    },
    #[error("registered graph {digest} is not a graph: {source}")]
    BadGraph {
        digest: GraphDigest,
        #[source]
        source: serde_json::Error,
    },
    #[error("registered graph {digest} failed validation: {message}")]
    InvalidGraph {
        digest:  GraphDigest,
        message: String,
    },
}

/// The coordinator log and graph registry of one run, open for writing.
pub struct CoordinatorStore {
    logs:     Arc<dyn RunLogs>,
    key:      RunKey,
    state:    CoordinatorState,
    next_seq: u64,
    /// Decoded and validated graphs by digest, so repeated loads (restarts of
    /// one invocation, resume-time resource checks) parse and validate once.
    graphs:   BTreeMap<GraphDigest, Arc<Graph>>,
    /// The records `create` appended before an observer could attach.
    opening:  Vec<CoordinatorRecord>,
    /// What each record's `recorded_at` reads.
    clock:    RecordingClock,
}

impl CoordinatorStore {
    /// Start a run's log: the run declaration is its first record.
    pub async fn create(
        logs: Arc<dyn RunLogs>,
        key: RunKey,
        middleware_chain: Vec<engine::MiddlewareKey>,
    ) -> Result<Self, StoreError> {
        Self::create_with_clock(
            logs,
            key,
            middleware_chain,
            RecordingClock::default(),
            false,
        )
        .await
    }

    /// [`CoordinatorStore::create`], stamping every record with `clock`.
    pub async fn create_with_clock(
        logs: Arc<dyn RunLogs>,
        key: RunKey,
        middleware_chain: Vec<engine::MiddlewareKey>,
        clock: RecordingClock,
        required_finalization: bool,
    ) -> Result<Self, StoreError> {
        let mut store = Self {
            logs,
            key: key.clone(),
            state: CoordinatorState::default(),
            next_seq: 0,
            graphs: BTreeMap::new(),
            opening: Vec::new(),
            clock,
        };
        let started = store
            .append(CoordinatorEvent::RunStarted {
                format_version: COORDINATOR_FORMAT_VERSION,
                key,
                root: InvocationId::ROOT,
                middleware_chain,
                required_finalization,
                forked_from: None,
            })
            .await?;
        store.opening.push(started);
        Ok(store)
    }

    /// Continue a stored run: read the log back, check its format, replay
    /// the state and verify every registered graph.
    pub async fn resume(logs: Arc<dyn RunLogs>, key: RunKey) -> Result<Self, StoreError> {
        let records = read_coordinator_log(&*logs).await?;
        let state = CoordinatorState::replay(&records)?;
        if state.root != Some(InvocationId::ROOT) {
            return Err(StateError::InvalidRootInvocation.into());
        }
        let graphs = load_graph_registry(&*logs, &state).await?;
        let next_seq = records.len() as u64;
        Ok(Self {
            logs,
            key,
            state,
            next_seq,
            graphs,
            opening: Vec::new(),
            clock: RecordingClock::default(),
        })
    }

    /// Stamp every record appended from now on with `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: RecordingClock) -> Self {
        self.clock = clock;
        self
    }

    /// The store handle the log lives in.
    pub fn logs(&self) -> &Arc<dyn RunLogs> {
        &self.logs
    }

    /// Where the run lives, for messages.
    pub fn locator(&self) -> String {
        self.logs.locator()
    }

    pub fn key(&self) -> &RunKey {
        &self.key
    }

    pub fn state(&self) -> &CoordinatorState {
        &self.state
    }

    /// The records `create` appended before anyone could observe them: the
    /// run's own start. A resumed store opened with none.
    pub fn opening_records(&self) -> &[CoordinatorRecord] {
        &self.opening
    }

    /// Validate, append, then apply. A rejected event is a
    /// [`StoreError::State`] and changes nothing; a failed append changes
    /// nothing; the state holds a record only once it is durable. The check
    /// borrows the state and the apply mutates it in place, so an append
    /// costs the record, not a copy of every declared invocation.
    pub async fn append(
        &mut self,
        event: CoordinatorEvent,
    ) -> Result<CoordinatorRecord, StoreError> {
        self.state.check(&event)?;
        let record = CoordinatorRecord::external(self.next_seq, self.clock.now(), event);
        let stored = encode_record(&record)?;
        self.logs.append(&LogId::Coordinator, &[stored]).await?;
        // Nothing touched the state since `check` accepted the event.
        self.state.apply_checked(&record.body);
        self.next_seq += 1;
        Ok(record)
    }

    /// Store and register a graph. The record is the `GraphRegistered`
    /// append when the graph was new, `None` when it was already registered.
    pub async fn register_graph(
        &mut self,
        graph: &Graph,
    ) -> Result<(GraphDigest, Option<CoordinatorRecord>), StoreError> {
        let bytes = ir::encode_graph(graph).map_err(StoreError::Encode)?;
        self.register_graph_bytes(&bytes).await
    }

    pub(crate) async fn register_graph_bytes(
        &mut self,
        bytes: &[u8],
    ) -> Result<(GraphDigest, Option<CoordinatorRecord>), StoreError> {
        let digest = GraphDigest::of(bytes);
        if self.state.graphs.contains(&digest) {
            let existing = self.graph_bytes(digest).await?;
            if existing != bytes {
                return Err(StoreError::GraphDigest {
                    expected: digest,
                    found:    GraphDigest::of(&existing),
                });
            }
            return Ok((digest, None));
        }
        let stored = self.logs.put_blob(bytes).await?;
        if stored != digest {
            return Err(StoreError::GraphDigest {
                expected: digest,
                found:    stored,
            });
        }
        let record = self
            .append(CoordinatorEvent::GraphRegistered { digest })
            .await?;
        Ok((digest, Some(record)))
    }

    /// A registered graph, decoded and validated once and cached by digest.
    pub async fn load_graph(&mut self, digest: GraphDigest) -> Result<Arc<Graph>, StoreError> {
        if !self.state.graphs.contains(&digest) {
            return Err(StoreError::MissingGraph(digest));
        }
        if let Some(graph) = self.graphs.get(&digest) {
            return Ok(graph.clone());
        }
        let graph = Arc::new(decode_graph(digest, &self.graph_bytes(digest).await?)?);
        self.graphs.insert(digest, graph.clone());
        Ok(graph)
    }

    /// A registered graph's exact bytes.
    pub async fn graph_bytes(&self, digest: GraphDigest) -> Result<Vec<u8>, StoreError> {
        graph_bytes(&*self.logs, digest).await
    }
}

/// The coordinator log of a run, read and decoded under this build's rules:
/// the run declaration's format version is checked before any record is
/// decoded, so an old run is refused as an old run, not as a bad record.
pub async fn read_coordinator_log(
    logs: &dyn RunLogs,
) -> Result<Vec<CoordinatorRecord>, StoreError> {
    let stored = logs.read(&LogId::Coordinator).await?;
    decode_coordinator_records(&stored)
}

/// Decode stored coordinator records. See [`read_coordinator_log`].
pub fn decode_coordinator_records(stored: &[Record]) -> Result<Vec<CoordinatorRecord>, StoreError> {
    if let Some(first) = stored.first() {
        let found = first
            .record
            .get("body")
            .and_then(|body| body.get("format_version"))
            .and_then(Value::as_u64)
            .and_then(|found| u32::try_from(found).ok())
            .unwrap_or(0);
        if found != COORDINATOR_FORMAT_VERSION {
            return Err(StoreError::UnsupportedFormat {
                found,
                expected: COORDINATOR_FORMAT_VERSION,
            });
        }
    }
    stored.iter().map(decode_coordinator_record).collect()
}

/// Decode one stored coordinator record, past the format check the log's
/// first record carries.
pub(crate) fn decode_coordinator_record(record: &Record) -> Result<CoordinatorRecord, StoreError> {
    record.decode().map_err(|source| StoreError::BadRecord {
        log: LogId::Coordinator,
        seq: record.seq,
        source,
    })
}

/// Encode a coordinator record as the store keeps it.
pub fn encode_record(record: &CoordinatorRecord) -> Result<Record, StoreError> {
    Record::encode(record).map_err(|error| match error {
        store::EncodeError::Encode(source) => StoreError::Encode(source),
        store::EncodeError::Shape(shape) => StoreError::Encode(serde_json::Error::custom(shape)),
    })
}

/// A registered graph's bytes from the store.
pub(crate) async fn graph_bytes(
    logs: &dyn RunLogs,
    digest: GraphDigest,
) -> Result<Vec<u8>, StoreError> {
    logs.get_blob(digest)
        .await?
        .ok_or(StoreError::MissingGraph(digest))
}

/// Load and verify every registered graph: the bytes hash to their digest,
/// decode, and validate. Seeds the store's cache so the first `load_graph`
/// does not repeat the work.
pub(crate) async fn load_graph_registry(
    logs: &dyn RunLogs,
    state: &CoordinatorState,
) -> Result<BTreeMap<GraphDigest, Arc<Graph>>, StoreError> {
    let mut graphs = BTreeMap::new();
    for digest in &state.graphs {
        let bytes = graph_bytes(logs, *digest).await?;
        graphs.insert(*digest, Arc::new(decode_graph(*digest, &bytes)?));
    }
    Ok(graphs)
}

pub(crate) fn decode_graph(digest: GraphDigest, bytes: &[u8]) -> Result<Graph, StoreError> {
    let found = GraphDigest::of(bytes);
    if found != digest {
        return Err(StoreError::GraphDigest {
            expected: digest,
            found,
        });
    }
    let graph: Graph =
        serde_json::from_slice(bytes).map_err(|source| StoreError::BadGraph { digest, source })?;
    if let Err(errors) = ir::validate(&graph) {
        return Err(StoreError::InvalidGraph {
            digest,
            message: errors[0].to_string(),
        });
    }
    Ok(graph)
}

/// Open the run stored under `run_dir`: the one-line form of "a
/// [`RunDirStore`] at this path, then this access" every command given a
/// run directory uses.
pub async fn open_run_dir(run_dir: &Path, access: Access) -> Result<Arc<dyn RunLogs>, StoreError> {
    Ok(RunDirStore::new(run_dir).open_stored(access).await?)
}

#[cfg(test)]
mod tests {
    use store::{Access, MemoryRunStore, OwnerId, RunKey, RunStore};

    use super::{CoordinatorStore, StoreError, read_coordinator_log};
    use crate::{CoordinatorEvent, CoordinatorState, ExecutionId, GraphDigest, StateError};

    async fn fresh() -> CoordinatorStore {
        let store = MemoryRunStore::new();
        let key = RunKey::new("store-test");
        let logs = store
            .open(&key, Access::Create {
                owner: OwnerId::mint(),
            })
            .await
            .expect("creates");
        CoordinatorStore::create(logs, key, Vec::new())
            .await
            .expect("store")
    }

    /// The state, `next_seq`, and the stored log: everything an append may
    /// change.
    async fn snapshot(store: &CoordinatorStore) -> (Vec<u8>, u64, Vec<store::Record>) {
        let state = serde_json::to_vec(store.state()).expect("the state encodes");
        let log = store
            .logs()
            .read(&store::LogId::Coordinator)
            .await
            .expect("the log reads");
        (state, store.next_seq, log)
    }

    async fn replayed(store: &CoordinatorStore) -> CoordinatorState {
        let records = read_coordinator_log(&**store.logs())
            .await
            .expect("the log decodes");
        CoordinatorState::replay(&records).expect("the log replays")
    }

    #[tokio::test]
    async fn a_rejected_event_leaves_the_state_and_the_log_untouched() {
        let mut store = fresh().await;
        let digest = GraphDigest::from_bytes([7; 32]);
        store
            .append(CoordinatorEvent::GraphRegistered { digest })
            .await
            .expect("the first registration");
        let before = snapshot(&store).await;

        let duplicate = store
            .append(CoordinatorEvent::GraphRegistered { digest })
            .await
            .expect_err("a duplicate graph is rejected");
        assert!(matches!(
            duplicate,
            StoreError::State(StateError::DuplicateGraph(found)) if found == digest
        ));
        let unknown = store
            .append(CoordinatorEvent::ExecutionFinished {
                execution: ExecutionId::new(9),
                exit:      engine::EngineExit::Terminal {
                    status: ir::RunStatus::Success,
                },
            })
            .await
            .expect_err("an unknown execution is rejected");
        assert!(matches!(
            unknown,
            StoreError::State(StateError::UnknownExecution(found)) if found == ExecutionId::new(9)
        ));

        assert_eq!(
            snapshot(&store).await,
            before,
            "a rejected event changes nothing"
        );
        let record = store
            .append(CoordinatorEvent::GraphRegistered {
                digest: GraphDigest::from_bytes([8; 32]),
            })
            .await
            .expect("the store still accepts events");
        assert_eq!(record.seq, 2, "rejected events take no sequence number");
        assert_eq!(replayed(&store).await, *store.state());
    }

    /// An append the backend refuses leaves the state as it was: the state
    /// holds a record only once it is durable.
    #[tokio::test]
    async fn a_failed_append_leaves_the_state_unchanged_and_is_reported() {
        let backing = MemoryRunStore::new();
        let key = RunKey::new("store-refused");
        let logs = backing
            .open(&key, Access::Create {
                owner: OwnerId::new("first"),
            })
            .await
            .expect("creates");
        let mut store = CoordinatorStore::create(logs, key.clone(), Vec::new())
            .await
            .expect("store");
        let before = snapshot(&store).await;

        // The lease moves: every append of this handle is refused.
        backing.release(&key);
        let taken = backing
            .open(&key, Access::Write {
                owner: OwnerId::new("second"),
            })
            .await
            .expect("another owner takes the run");
        let digest = GraphDigest::from_bytes([7; 32]);
        let failure = store
            .append(CoordinatorEvent::GraphRegistered { digest })
            .await
            .expect_err("a stale owner's append fails");
        assert!(
            matches!(failure, StoreError::Store(store::StoreError::StaleOwner)),
            "{failure}"
        );
        assert_eq!(
            snapshot(&store).await,
            before,
            "a failed append changes nothing"
        );
        assert!(
            !store.state().graphs.contains(&digest),
            "the graph is not in the state before it is durable"
        );
        drop(taken);
    }

    /// The run declaration pins the format: a log of another version is
    /// refused before any record is decoded.
    #[tokio::test]
    async fn a_log_of_another_format_is_refused_by_version() {
        let store = fresh().await;
        let mut stored = store
            .logs()
            .read(&store::LogId::Coordinator)
            .await
            .expect("reads");
        stored[0].record["body"]["format_version"] = serde_json::json!(1);
        let error = super::decode_coordinator_records(&stored).expect_err("refused");
        assert!(
            matches!(error, StoreError::UnsupportedFormat {
                found:    1,
                expected: crate::COORDINATOR_FORMAT_VERSION,
            }),
            "{error}"
        );
    }
}
