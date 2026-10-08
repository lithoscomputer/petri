//! The coordinator's observers and the store writer: how every execution's
//! records reach the run's store, and how a host observer sees them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::{fmt, fs, io};

use driver::{EventObserver, ObserveError};
use engine::{EngineState, Event, EventLog, EventOrigin, EventRecord, InvalidRecords, LOG_VERSION};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use store::jsonl::clean_lines;
use store::{LogId, Record, RunLogs};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::{CoordinatorRecord, CoordinatorState, ExecutionId};

#[async_trait::async_trait]
pub trait ExecutionObserver: Send + Sync {
    /// One execution's appended record, with the driver's recording time
    /// (`recorded_at`, milliseconds since the Unix epoch) and the post-apply
    /// state; see `driver::EventObserver::on_record`.
    fn on_engine_record(
        &self,
        execution: ExecutionId,
        record: &EventRecord,
        recorded_at: u64,
        state: &EngineState,
    );

    fn on_lifecycle(&self, record: &CoordinatorRecord);

    /// The replayed coordinator state an observer attaches to on resume, so
    /// state it keeps from coordinator records (a pause, the live executions)
    /// starts where the log left it. Records already stored are not
    /// redelivered; only records appended from here on reach `on_lifecycle`.
    /// Not called on a fresh run.
    fn on_resumed(&self, state: &CoordinatorState) {
        let _ = state;
    }

    /// Resolve once every record of `execution` this observer has been
    /// handed through `seq` is in its durable storage: the per-execution
    /// form of [`EventObserver::durable`], awaited by the driver for a
    /// step's acknowledged progress send. The default answers at once.
    async fn durable(&self, execution: ExecutionId, seq: u64) -> Result<(), ObserveError> {
        let _ = (execution, seq);
        Ok(())
    }

    /// Awaited after an execution's last record, before its report: the
    /// per-execution form of [`EventObserver::finish`]. A failure lands in
    /// the execution report's observer errors.
    async fn finish(&self, execution: ExecutionId) -> Result<(), ObserveError> {
        let _ = execution;
        Ok(())
    }
}

pub struct AddressedObserver {
    execution: ExecutionId,
    observer:  Arc<dyn ExecutionObserver>,
}

impl AddressedObserver {
    pub fn new(execution: ExecutionId, observer: Arc<dyn ExecutionObserver>) -> Self {
        Self {
            execution,
            observer,
        }
    }
}

#[async_trait::async_trait]
impl EventObserver for AddressedObserver {
    fn on_record(&self, record: &EventRecord, recorded_at: u64, state: &EngineState) {
        self.observer
            .on_engine_record(self.execution, record, recorded_at, state);
    }

    async fn durable(&self, seq: u64) -> Result<(), ObserveError> {
        self.observer.durable(self.execution, seq).await
    }

    async fn finish(&self) -> Result<(), ObserveError> {
        self.observer.finish(self.execution).await
    }
}

/// One execution's engine log, decoded: the records and each one's
/// recording time, by seq.
#[derive(Debug)]
pub struct DecodedEngineLog {
    pub log:         EventLog,
    /// Each record's recording time, by seq: when the driver appended it,
    /// milliseconds since the Unix epoch.
    pub recorded_at: Vec<u64>,
}

/// An engine log decoded from a JSONL file's bytes: [`DecodedEngineLog`]
/// with what the framing adds, the clean prefix and whether a torn line
/// followed it.
#[derive(Debug)]
pub struct DecodedEngineFile {
    pub log:         EventLog,
    pub recorded_at: Vec<u64>,
    pub clean_len:   usize,
    pub torn:        bool,
}

/// Why stored engine records could not become an [`EventLog`], with no
/// log identity attached; callers that read one execution's log wrap this
/// in [`EngineLogError`].
///
/// The torn-line rule of the file framing is strict: a final line is *torn*
/// only when EOF arrives before its terminating newline, and only then is it
/// dropped. A newline-terminated line that fails to decode refuses the load:
/// corruption or tampering must not be silently accepted as a crash prefix.
#[derive(Debug, thiserror::Error)]
pub enum EngineLogDecodeError {
    #[error("line {line} is not an event record")]
    BadRecord {
        line:   usize,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Invalid(#[from] InvalidRecords),
}

/// An execution's engine log that could not be read or decoded.
#[derive(Debug, thiserror::Error)]
pub enum EngineLogError {
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error("execution {execution} log: {source}")]
    Decode {
        execution: ExecutionId,
        #[source]
        source:    EngineLogDecodeError,
    },
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{path}`: {source}")]
    DecodeFile {
        path:   PathBuf,
        #[source]
        source: EngineLogDecodeError,
    },
}

/// One stored engine record: the core's record with the driver's recording
/// time beside it, `{"seq", "origin", "recorded_at", "body"}`. `body` is the
/// engine event as the engine serializes it, tagged by `event`. The public
/// event stream carries this same line, unchanged, as a record's `record`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredEngineRecord {
    pub seq:         u64,
    pub origin:      EventOrigin,
    /// Milliseconds since the Unix epoch when the driver appended the record.
    pub recorded_at: u64,
    pub body:        Event,
}

impl StoredEngineRecord {
    pub fn new(record: &EventRecord, recorded_at: u64) -> Self {
        Self {
            seq: record.seq,
            origin: record.origin,
            recorded_at,
            body: record.event.clone(),
        }
    }

    pub fn into_parts(self) -> (EventRecord, u64) {
        (
            EventRecord {
                seq:    self.seq,
                origin: self.origin,
                event:  self.body,
            },
            self.recorded_at,
        )
    }
}

/// [`StoredEngineRecord`] for writing, over a borrowed record.
#[derive(Serialize)]
struct StoredRecordRef<'a> {
    seq:         u64,
    origin:      EventOrigin,
    recorded_at: u64,
    body:        &'a Event,
}

/// The stored form of one engine record.
pub fn encode_engine_record(
    record: &EventRecord,
    recorded_at: u64,
) -> Result<Record, serde_json::Error> {
    let value = serde_json::to_value(StoredRecordRef {
        seq: record.seq,
        origin: record.origin,
        recorded_at,
        body: &record.event,
    })?;
    Record::from_value(value).map_err(serde_json::Error::custom)
}

/// Decode the stored records of one engine log into an [`EventLog`] with
/// its recording times. The log version is the run format's: a run this
/// build opens stores logs of [`LOG_VERSION`].
pub fn decode_engine_records(stored: &[Record]) -> Result<DecodedEngineLog, EngineLogDecodeError> {
    let mut records = Vec::with_capacity(stored.len());
    let mut recorded_at = Vec::with_capacity(stored.len());
    for (index, line) in stored.iter().enumerate() {
        let stored: StoredEngineRecord =
            line.decode()
                .map_err(|source| EngineLogDecodeError::BadRecord {
                    line: index + 1,
                    source,
                })?;
        let (record, at) = stored.into_parts();
        records.push(record);
        recorded_at.push(at);
    }
    let log = EventLog::try_from_records(LOG_VERSION, records)?;
    Ok(DecodedEngineLog { log, recorded_at })
}

/// Read and decode one execution's engine log from the store. An execution
/// nothing was appended for decodes to an empty log.
pub async fn read_execution_log(
    logs: &dyn RunLogs,
    execution: ExecutionId,
) -> Result<DecodedEngineLog, EngineLogError> {
    let stored = logs.read(&LogId::Execution(execution)).await?;
    decode_engine_records(&stored).map_err(|source| EngineLogError::Decode { execution, source })
}

/// Decode `events.jsonl` bytes: one record per line, under the strict
/// torn-line rule. The file framing of the run directory, for a host or a
/// test that reads a log file directly.
pub fn decode_engine_log(bytes: &[u8]) -> Result<DecodedEngineFile, EngineLogDecodeError> {
    let lines = clean_lines(bytes);
    let (clean_len, torn) = (lines.clean_len, lines.torn);
    let mut stored = Vec::new();
    for (index, line) in lines.enumerate() {
        let bad = |source: serde_json::Error| EngineLogDecodeError::BadRecord {
            line: index + 1,
            source,
        };
        let value: serde_json::Value = serde_json::from_slice(line).map_err(bad)?;
        let record =
            Record::from_value(value).map_err(|shape| bad(serde_json::Error::custom(shape)))?;
        stored.push(record);
    }
    let decoded = decode_engine_records(&stored)?;
    Ok(DecodedEngineFile {
        log: decoded.log,
        recorded_at: decoded.recorded_at,
        clean_len,
        torn,
    })
}

/// Render a log in the `events.jsonl` framing, produced in one piece.
/// `recorded_at` is each record's recording time, by seq.
///
/// # Panics
///
/// When `recorded_at` does not have one time per record.
pub fn encode_engine_log(log: &EventLog, recorded_at: &[u64]) -> Vec<u8> {
    assert_eq!(
        recorded_at.len(),
        log.len(),
        "one recording time per record"
    );
    let mut out = Vec::new();
    for (record, recorded_at) in log.records().iter().zip(recorded_at) {
        let line = serde_json::to_vec(&StoredRecordRef {
            seq:         record.seq,
            origin:      record.origin,
            recorded_at: *recorded_at,
            body:        &record.event,
        })
        .expect("a record always encodes");
        out.extend(line);
        out.push(b'\n');
    }
    out
}

/// Read and decode one execution's `events.jsonl` file.
pub fn read_engine_log(path: &Path) -> Result<DecodedEngineFile, EngineLogError> {
    let bytes = fs::read(path).map_err(|source| EngineLogError::Io {
        action: "read",
        path: path.to_path_buf(),
        source,
    })?;
    decode_engine_log(&bytes).map_err(|source| EngineLogError::DecodeFile {
        path: path.to_path_buf(),
        source,
    })
}

/// The first write the run's store failed in this lifetime. Every writer of
/// the run trips it: the engine logs' [`StoreWriter`] and the resource
/// store. The coordinator ends the lifetime when it trips, and the next
/// lifetime resumes from what the store holds.
#[derive(Debug, Default)]
pub(crate) struct StoreFailure {
    first:   OnceLock<String>,
    tripped: Notify,
}

impl StoreFailure {
    /// Record a failed write; only the first is kept.
    pub(crate) fn trip(&self, message: String) {
        if self.first.set(message).is_ok() {
            self.tripped.notify_waiters();
        }
    }

    /// The first failed write, once there is one.
    pub(crate) fn get(&self) -> Option<&str> {
        self.first.get().map(String::as_str)
    }

    /// Resolve once a write has failed.
    pub(crate) async fn wait(&self) -> &str {
        loop {
            let tripped = self.tripped.notified();
            tokio::pin!(tripped);
            tripped.as_mut().enable();
            if let Some(first) = self.get() {
                return first;
            }
            tripped.await;
        }
    }
}

enum WriterMessage {
    Records(LogId, Vec<Record>),
    /// Answer once every record queued before this message is stored: the
    /// writer is one task over one FIFO queue, so reaching the marker means
    /// the earlier records reached the store or an append failed.
    Durable(oneshot::Sender<Result<(), ObserveError>>),
}

/// The one store writer of a run: every execution's records go through it
/// to the run's [`RunLogs`], in batches per log. A dedicated task owns the
/// appends, so the backend's latency never blocks a driver's event loop.
/// The queue is unbounded because the observer contract is lossless; its
/// length cannot exceed the run's finite logs.
///
/// The writer holds the run's store handle weakly: the coordinator owns
/// the handle and the lease with it, and a writer that outlives its
/// coordinator (a crash test drops one mid-run) stores nothing more.
///
/// A `durable` marker is the acknowledgement a step's `send_acked` waits
/// for: every record queued before it is stored, past the point where a
/// process crash can lose it. The first failed append is answered there and
/// by every later marker; later records are not stored, and the run stops
/// at that record.
pub struct StoreWriter {
    locator: String,
    tx:      mpsc::UnboundedSender<WriterMessage>,
    task:    Mutex<Option<JoinHandle<()>>>,
    failure: Arc<StoreFailure>,
}

impl fmt::Debug for StoreWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoreWriter")
            .field("locator", &self.locator)
            .finish_non_exhaustive()
    }
}

impl StoreWriter {
    /// Start the writer over an opened run.
    pub fn start(logs: &Arc<dyn RunLogs>) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let locator = logs.locator();
        let failure = Arc::new(StoreFailure::default());
        let task = tokio::spawn(write_records(
            Arc::downgrade(logs),
            rx,
            Arc::clone(&failure),
        ));
        Arc::new(Self {
            locator,
            tx,
            task: Mutex::new(Some(task)),
            failure,
        })
    }

    /// The run's store failure this writer trips, for the run's other
    /// writers to share.
    pub(crate) fn failure(&self) -> Arc<StoreFailure> {
        Arc::clone(&self.failure)
    }

    /// Queue records for one log. Never waits.
    pub fn push(&self, log: LogId, records: Vec<Record>) {
        let _ = self.tx.send(WriterMessage::Records(log, records));
    }

    /// Resolve once every record queued before this call is stored.
    pub async fn flush(&self) -> Result<(), ObserveError> {
        let (reply, done) = oneshot::channel();
        self.tx
            .send(WriterMessage::Durable(reply))
            .map_err(|_| self.dead())?;
        done.await.unwrap_or_else(|_| Err(self.dead()))
    }

    /// Store what is queued, then stop the writer task. Call once, after
    /// the run's last driver has finished.
    pub async fn shutdown(&self) -> Result<(), ObserveError> {
        let result = self.flush().await;
        let task = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        result
    }

    /// The writer task is gone: its queue closed before it answered.
    fn dead(&self) -> ObserveError {
        ObserveError::new(
            "run store",
            format!("the writer for {} stopped", self.locator),
        )
    }
}

impl Drop for StoreWriter {
    fn drop(&mut self) {
        if let Some(task) = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

/// The writer task: drain what is queued, append per log in order, answer
/// each marker once everything before it is stored.
async fn write_records(
    logs: Weak<dyn RunLogs>,
    mut rx: mpsc::UnboundedReceiver<WriterMessage>,
    tripped: Arc<StoreFailure>,
) {
    let mut failure: Option<String> = None;
    let mut pending: Vec<(LogId, Vec<Record>)> = Vec::new();
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while let Ok(message) = rx.try_recv() {
            batch.push(message);
        }
        for message in batch {
            match message {
                WriterMessage::Records(log, records) => {
                    if failure.is_some() {
                        continue;
                    }
                    match pending.last_mut() {
                        Some((last, queued)) if *last == log => queued.extend(records),
                        _ => pending.push((log, records)),
                    }
                }
                WriterMessage::Durable(reply) => {
                    append_pending(&logs, &mut pending, &mut failure, &tripped).await;
                    let _ = reply.send(match &failure {
                        Some(message) => Err(ObserveError::new("run store", message.clone())),
                        None => Ok(()),
                    });
                }
            }
        }
        append_pending(&logs, &mut pending, &mut failure, &tripped).await;
    }
}

async fn append_pending(
    logs: &Weak<dyn RunLogs>,
    pending: &mut Vec<(LogId, Vec<Record>)>,
    failure: &mut Option<String>,
    tripped: &StoreFailure,
) {
    for (log, records) in pending.drain(..) {
        if failure.is_some() {
            break;
        }
        let Some(logs) = logs.upgrade() else {
            *failure = Some("the run's store handle is gone".to_owned());
            break;
        };
        if let Err(error) = logs.append(&log, &records).await {
            let message = format!("could not append to the {log} log: {error}");
            tripped.trip(message.clone());
            *failure = Some(message);
        }
    }
}

/// One execution's engine log as the driver's observer: the records it is
/// handed go to the run's [`StoreWriter`] under the execution's log, and
/// `durable` and `finish` resolve on the store's acknowledgement.
///
/// `high_water` is the count of records already stored: a resumed driver
/// redelivers the stored prefix, and records below the mark are skipped.
pub struct ExecutionLogWriter {
    writer:     Arc<StoreWriter>,
    execution:  ExecutionId,
    high_water: u64,
}

impl ExecutionLogWriter {
    pub fn new(writer: Arc<StoreWriter>, execution: ExecutionId, high_water: u64) -> Self {
        Self {
            writer,
            execution,
            high_water,
        }
    }

    /// Resolve once every record queued before this call is stored: the
    /// fence a lease reservation waits for.
    pub async fn flush(&self) -> Result<(), ObserveError> {
        self.writer.flush().await
    }
}

#[async_trait::async_trait]
impl EventObserver for ExecutionLogWriter {
    fn on_record(&self, record: &EventRecord, recorded_at: u64, _state: &EngineState) {
        if record.seq < self.high_water {
            return;
        }
        match encode_engine_record(record, recorded_at) {
            Ok(stored) => self
                .writer
                .push(LogId::Execution(self.execution), vec![stored]),
            // A record that does not encode is a Petri bug; the log is then
            // short of it, which replay verification reports.
            Err(error) => tracing::error!(
                execution = self.execution.raw(),
                seq = record.seq,
                %error,
                "an engine record does not encode"
            ),
        }
    }

    async fn durable(&self, _seq: u64) -> Result<(), ObserveError> {
        // The queue is FIFO: every record handed over before this call is
        // ahead of the marker, whatever its seq.
        self.writer.flush().await
    }

    async fn finish(&self) -> Result<(), ObserveError> {
        self.writer.flush().await
    }
}

#[cfg(test)]
mod tests {
    use ir::{CancelScopeId, Graph};
    use store::{Access, MemoryRunStore, OwnerId, RunKey, RunStore};

    use super::*;

    fn record(seq: u64) -> EventRecord {
        EventRecord {
            seq,
            origin: EventOrigin::External,
            event: Event::cancel_scope(CancelScopeId::ROOT),
        }
    }

    async fn opened() -> (MemoryRunStore, RunKey, Arc<dyn RunLogs>) {
        let store = MemoryRunStore::new();
        let key = RunKey::new("writer");
        let logs = store
            .open(&key, Access::Create {
                owner: OwnerId::new("owner"),
            })
            .await
            .expect("creates");
        (store, key, logs)
    }

    /// `durable` answers once the record is in the store: the records read
    /// back decode to the record before the acknowledgement is used for
    /// anything.
    #[tokio::test]
    async fn durable_answers_once_the_record_is_in_the_store() {
        let (_store, _key, logs) = opened().await;
        let writer = StoreWriter::start(&logs);
        let log = ExecutionLogWriter::new(writer.clone(), ExecutionId::new(0), 0);
        let state = EngineState::new(Graph::new());
        log.on_record(&record(0), 1_000, &state);
        log.on_record(&record(1), 1_250, &state);
        log.durable(1).await.expect("both records stored");

        let decoded = read_execution_log(&*logs, ExecutionId::new(0))
            .await
            .expect("decodes");
        assert_eq!(decoded.log.records(), &[record(0), record(1)]);
        assert_eq!(
            decoded.recorded_at,
            vec![1_000, 1_250],
            "each record's recording time is read back beside it"
        );
        log.finish().await.expect("finished");
        writer.shutdown().await.expect("stopped");
    }

    /// The file framing writes the same lines the store keeps, times
    /// included, and reads them back.
    #[test]
    fn encoding_a_log_round_trips_its_recording_times() {
        let log = EventLog::try_from_records(LOG_VERSION, vec![record(0), record(1)])
            .expect("a valid log");
        let bytes = encode_engine_log(&log, &[7, 9]);
        let decoded = decode_engine_log(&bytes).expect("decodes");
        assert_eq!(decoded.log, log);
        assert_eq!(decoded.recorded_at, vec![7, 9]);
        assert!(!decoded.torn);
    }

    /// An append that fails is the acknowledgement's error (the step hears
    /// it) and every later marker reports it again for the run's report.
    #[tokio::test]
    async fn a_failed_append_is_the_durable_answer_and_the_finish_report() {
        let (store, key, logs) = opened().await;
        let writer = StoreWriter::start(&logs);
        // The lease moves: every append of this handle is refused.
        store.release(&key);
        let taken = store
            .open(&key, Access::Write {
                owner: OwnerId::new("other"),
            })
            .await
            .expect("another owner takes the run");
        let log = ExecutionLogWriter::new(writer.clone(), ExecutionId::new(0), 0);
        log.on_record(&record(0), 1_000, &EngineState::new(Graph::new()));

        let error = log.durable(0).await.expect_err("the append failed");
        assert_eq!(error.observer, "run store");
        let error = log.finish().await.expect_err("reported again at finish");
        assert_eq!(error.observer, "run store");
        assert!(
            taken
                .read(&LogId::Execution(ExecutionId::new(0)))
                .await
                .expect("reads")
                .is_empty(),
            "nothing reached the store"
        );
        writer
            .shutdown()
            .await
            .expect_err("reported at shutdown too");
    }

    /// Records below the high-water mark are already stored and are
    /// skipped on redelivery.
    #[tokio::test]
    async fn a_resumed_writer_skips_the_stored_prefix() {
        let (_store, _key, logs) = opened().await;
        let writer = StoreWriter::start(&logs);
        let state = EngineState::new(Graph::new());
        let first = ExecutionLogWriter::new(writer.clone(), ExecutionId::new(0), 0);
        first.on_record(&record(0), 1, &state);
        first.finish().await.expect("stored");
        let resumed = ExecutionLogWriter::new(writer.clone(), ExecutionId::new(0), 1);
        resumed.on_record(&record(0), 5, &state);
        resumed.on_record(&record(1), 6, &state);
        resumed.finish().await.expect("stored");
        let decoded = read_execution_log(&*logs, ExecutionId::new(0))
            .await
            .expect("decodes");
        assert_eq!(decoded.recorded_at, vec![1, 6]);
        writer.shutdown().await.expect("stopped");
    }
}
