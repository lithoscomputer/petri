//! The durable record store a run lives in.
//!
//! Everything durable about a run is a log or a blob. The coordinator log,
//! one engine log per execution and the sandbox resource log are append-only
//! logs of records; registered graphs are content-addressed blobs. A backend
//! implements two traits with five methods ([`RunStore::open`];
//! [`RunLogs::append`], [`RunLogs::read`], [`RunLogs::put_blob`],
//! [`RunLogs::get_blob`]) and stores what it is given: a backend never sees
//! an observer, a projection, a decoded record, a version number or a digest
//! to verify. Decoding, version checks and digest checks stay on Petri's
//! side of the seam. A sixth method, [`RunLogs::read_from`], has a default
//! over `read`; a backend that can key on `seq` overrides it so a reader
//! that holds a prefix of a log is handed the rest without the prefix.
//!
//! The stored unit is the record ([`Record`]): the line a public event
//! carries under `record`, `{seq, origin, recorded_at, body}`, with `seq` and
//! `recorded_at` lifted out so a backend can key and index without reading
//! into the JSON. What a backend hands back must equal what it was given as
//! a JSON value.
//!
//! Two backends ship here: [`RunDirStore`], today's run directory, and
//! [`MemoryRunStore`] for tests. A host with a database of its own
//! implements the traits once over it.
//!
//! # Contract
//!
//! - `open` with [`Access::Create`] fails with [`StoreError::Exists`] when the
//!   key exists; `Create` and [`Access::Write`] take the run's exclusive writer
//!   lease for the caller's [`OwnerId`]; [`Access::Read`] takes no lease and
//!   never blocks a writer.
//! - The lease is idempotent per owner: a retry with the owner that holds it
//!   gets a handle to the same lease. Another live owner is refused with
//!   [`StoreError::Leased`]. A lease ends when its owner drops the handle, when
//!   the backend's own liveness signal says the owner is gone (the file lock
//!   ends with the process), or when an operator releases it. It never ends by
//!   timeout.
//! - Releasing a live owner's lease does not stop the owner. Its writes are
//!   refused from then on, but until its next write fails it may still start
//!   processes, run hooks, and finish provider calls it began, beside the new
//!   owner. A host releases a lease, or lets its liveness signal end one, only
//!   when the owner is known to be gone.
//! - `append` returns once the records are durable: past the point where a
//!   process crash can lose them. Appends to one log are ordered; a backend
//!   never reorders within a log. `(log, seq)` is unique: the same record again
//!   at a taken seq is accepted without a second append (a lost-reply retry is
//!   safe), a different record at a taken seq is [`StoreError::Conflict`], and
//!   so is a seq past the log's end. A handle whose owner no longer holds the
//!   lease gets [`StoreError::StaleOwner`]; a `Read` handle gets
//!   [`StoreError::ReadOnly`].
//! - `read` hands back every record of one log in seq order, and an empty list
//!   for a log nothing was appended to. A run-directory backend drops a torn
//!   tail before it answers, and reports nothing. `read_from` hands back the
//!   records of one log at or past a seq, in seq order: what `read` would, with
//!   the records before that seq left out.
//! - Blobs are content-addressed by SHA-256 ([`Digest::of`]); a blob write is
//!   idempotent by construction.

use std::error::Error;
use std::fmt::Write as _;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fmt, io, process};

pub use ir::ExecutionId;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use smol_str::SmolStr;

pub mod jsonl;
mod memory;
mod run_dir;

pub use memory::MemoryRunStore;
pub use run_dir::{
    COORDINATOR_FILE, EVENTS_FILE, EXECUTIONS_DIR, GRAPHS_DIR, RESOURCES_FILE, RUN_FILE,
    RunDirLogs, RunDirStore, execution_relative_dir,
};

/// Host-provided run identity: the one name a run has in its store and on
/// its sandbox provider (every sandbox of the run is labelled with it). A
/// host passes its own (Fabro's run ULID); Petri mints one when the host
/// gives none.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunKey(SmolStr);

impl RunKey {
    pub fn new(key: impl Into<SmolStr>) -> Self {
        Self(key.into())
    }

    /// A fresh key: unique across processes and time.
    pub fn mint() -> Self {
        Self(SmolStr::new(fresh_id()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RunKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "RunKey({:?})", self.0)
    }
}

impl fmt::Display for RunKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The coordinator instance holding a run's writer lease, minted per start.
/// A retry with the same owner recovers the same lease; another owner is
/// refused while the first is live.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OwnerId(SmolStr);

impl OwnerId {
    pub fn new(owner: impl Into<SmolStr>) -> Self {
        Self(owner.into())
    }

    /// A fresh owner: unique across processes and time.
    pub fn mint() -> Self {
        Self(SmolStr::new(fresh_id()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OwnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "OwnerId({:?})", self.0)
    }
}

impl fmt::Display for OwnerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Unique across processes and time: the wall clock, the process, a
/// counter, and random bytes so two hosts never mint the same id.
fn fresh_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut random = [0_u8; 6];
    // A failed OS random source leaves zeros; the clock, process and
    // counter still tell ids apart within one host.
    let _ = getrandom::fill(&mut random);
    let mut id = format!(
        "{nanos:x}-{}-{}-",
        process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    for byte in random {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// Which log of a run a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "log", rename_all = "snake_case")]
pub enum LogId {
    /// The coordinator's lifecycle log.
    Coordinator,
    /// The sandbox resource log: one record per lease transition.
    Resources,
    /// One execution's engine log.
    Execution(ExecutionId),
}

impl fmt::Display for LogId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coordinator => formatter.write_str("coordinator"),
            Self::Resources => formatter.write_str("resources"),
            Self::Execution(execution) => write!(formatter, "execution {execution}"),
        }
    }
}

/// One stored line. `record` is exactly the value a public event carries
/// under `record`: `{seq, origin, recorded_at, body}` for the coordinator
/// and engine logs, `{seq, recorded_at, body}` for the resource log. `seq`
/// and `recorded_at` are lifted out of it so a backend can key and index
/// without reading into the JSON; they are stored, never read into.
#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub seq:         u64,
    /// Milliseconds since the Unix epoch when Petri appended the record.
    pub recorded_at: u64,
    pub record:      Value,
}

/// A stored value that is not a record: it lacks `seq` or `recorded_at`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a stored line is not a record: it lacks a numeric `{field}`")]
pub struct BadRecord {
    pub field: &'static str,
}

impl Record {
    /// A record from its stored value, lifting `seq` and `recorded_at`.
    pub fn from_value(record: Value) -> Result<Self, BadRecord> {
        let lift = |field: &'static str| {
            record
                .get(field)
                .and_then(Value::as_u64)
                .ok_or(BadRecord { field })
        };
        let seq = lift("seq")?;
        let recorded_at = lift("recorded_at")?;
        Ok(Self {
            seq,
            recorded_at,
            record,
        })
    }

    /// A record from a serializable line: what Petri's own record types
    /// store.
    pub fn encode<T: Serialize>(line: &T) -> Result<Self, EncodeError> {
        let record = serde_json::to_value(line).map_err(EncodeError::Encode)?;
        Self::from_value(record).map_err(EncodeError::Shape)
    }

    /// The record read into Petri's own type.
    pub fn decode<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(&self.record)
    }
}

/// Why a line could not become a [`Record`].
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("could not encode a record: {0}")]
    Encode(#[source] serde_json::Error),
    #[error(transparent)]
    Shape(BadRecord),
}

/// SHA-256 of a blob's exact bytes: how registered graphs are addressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(ir::graph_digest_bytes(bytes))
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        ir::digest_hex(&self.0)
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a digest must be 64 lowercase hexadecimal characters")]
pub struct DigestParseError;

impl FromStr for Digest {
    type Err = DigestParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 64 {
            return Err(DigestParseError);
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_digit(pair[0]).ok_or(DigestParseError)?;
            let low = hex_digit(pair[1]).ok_or(DigestParseError)?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

const fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl Serialize for Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(D::Error::custom)
    }
}

/// How a run is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// A fresh run: fails if the key exists. Takes the writer lease.
    Create { owner: OwnerId },
    /// A stored run, to continue: takes the writer lease.
    Write { owner: OwnerId },
    /// A stored run, to read: no lease, never blocks a writer.
    Read,
}

impl Access {
    /// The owner a writer open names; a read names none.
    pub fn owner(&self) -> Option<&OwnerId> {
        match self {
            Self::Create { owner } | Self::Write { owner } => Some(owner),
            Self::Read => None,
        }
    }
}

/// What a backend could not do.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// `Create` named a key the store already holds.
    #[error("run `{key}` already exists in {locator}")]
    Exists { key: RunKey, locator: String },
    /// `Write` or `Read` named a key the store does not hold.
    #[error("run `{key}` is not in {locator}")]
    NotFound { key: RunKey, locator: String },
    /// Another live owner holds the run's writer lease.
    #[error("run {locator} is already in use by owner {owner}")]
    Leased { locator: String, owner: OwnerId },
    /// The handle's owner no longer holds the lease: a later owner took the
    /// run, and this one stops at its next write.
    #[error("the run's lease moved to another owner; this handle is stale")]
    StaleOwner,
    /// A `Read` handle was asked to write.
    #[error("the run was opened for reading")]
    ReadOnly,
    /// A different record at a taken seq, or a seq past the log's end.
    #[error(
        "{log} log: a different record is already stored at seq {seq}, or seq {seq} skips ahead"
    )]
    Conflict { log: LogId, seq: u64 },
    /// The backend itself failed: an I/O error, a database error, a line it
    /// stores that is not a record.
    #[error("could not {action} in {locator}: {cause}")]
    Backend {
        locator: String,
        action:  &'static str,
        #[source]
        cause:   Box<dyn Error + Send + Sync>,
    },
}

impl StoreError {
    /// A backend failure with its cause.
    pub fn backend(
        locator: impl Into<String>,
        action: &'static str,
        cause: impl Into<Box<dyn Error + Send + Sync>>,
    ) -> Self {
        Self::Backend {
            locator: locator.into(),
            action,
            cause: cause.into(),
        }
    }

    /// A backend I/O failure.
    pub fn io(locator: impl Into<String>, action: &'static str, source: io::Error) -> Self {
        Self::backend(locator, action, source)
    }
}

/// A store of runs. Petri ships [`RunDirStore`] and [`MemoryRunStore`]; a
/// host implements it over its own database or over the wire.
#[async_trait::async_trait]
pub trait RunStore: Send + Sync {
    /// Open one run. See the crate docs for the lease rules each access
    /// mode follows.
    async fn open(&self, key: &RunKey, access: Access) -> Result<Arc<dyn RunLogs>, StoreError>;
}

/// One run: its append-only logs and its content-addressed blobs. The
/// handle is the owner token: a writer handle carries the lease it was
/// opened with, and a reader handle refuses every mutation.
#[async_trait::async_trait]
pub trait RunLogs: Send + Sync {
    /// Where the run lives, for messages: a path, or a database and an id.
    fn locator(&self) -> String;

    /// Append records to one log at the sequences they carry, returning
    /// once they are durable. See the crate docs for the `(log, seq)` rule.
    async fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError>;

    /// Every record of one log in seq order; empty if the log does not
    /// exist.
    async fn read(&self, log: &LogId) -> Result<Vec<Record>, StoreError>;

    /// The records of one log at or past `seq`, in seq order: what
    /// [`read`](Self::read) hands back with the records before `seq` left
    /// out; empty if the log does not exist or ends before `seq`. The
    /// default reads the log whole and drops the prefix; a backend that can
    /// key on `seq` answers without reading it.
    async fn read_from(&self, log: &LogId, seq: u64) -> Result<Vec<Record>, StoreError> {
        let mut records = self.read(log).await?;
        records.retain(|record| record.seq >= seq);
        Ok(records)
    }

    /// Store a blob by content, idempotently, and hand back its digest.
    async fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError>;

    /// The blob with this digest, if the run holds one.
    async fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError>;
}

/// Check that a batch of appends is at or past the log's head and answer
/// which records are new. The shared half of the `(log, seq)` rule: a
/// record at a taken seq must equal the stored one, a record past the head
/// must be the next one, and a batch must be contiguous.
///
/// `stored` is the record already at a seq, for the retry comparison.
pub(crate) fn admit<'a>(
    log: &LogId,
    head: u64,
    records: &'a [Record],
    stored: impl Fn(u64) -> Option<Record>,
) -> Result<Vec<&'a Record>, StoreError> {
    let mut next = head;
    let mut fresh = Vec::new();
    for record in records {
        if record.seq < next {
            match stored(record.seq) {
                Some(existing) if existing == *record => continue,
                _ => {
                    return Err(StoreError::Conflict {
                        log: *log,
                        seq: record.seq,
                    });
                }
            }
        }
        if record.seq != next {
            return Err(StoreError::Conflict {
                log: *log,
                seq: record.seq,
            });
        }
        fresh.push(record);
        next += 1;
    }
    Ok(fresh)
}
