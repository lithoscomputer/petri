//! The run-directory backend: one directory per run, one JSONL file per
//! log, one file per blob, and the run's lease as a file lock.
//!
//! Layout under the run directory:
//!
//! - `run.json`: the store's own index, `{"key": <run key>}`, and the file the
//!   writer lease locks.
//! - `coordinator.jsonl`: the coordinator log, one record per line.
//! - `resources.jsonl`: the sandbox resource log, one record per line.
//! - `graphs/<digest>.json`: every blob, byte-exact, named by its digest.
//! - `executions/<execution>/events.jsonl`: one engine log per execution.
//!
//! A line is the record's JSON value, so the coordinator and engine logs
//! hold exactly the lines a public event carries under `record`. A torn
//! final line (EOF before its newline) is dropped: a writer truncates it
//! before it appends, a reader leaves the file alone.
//!
//! Durability: every append is written and flushed before it returns, so a
//! process crash cannot lose it. The coordinator and resource logs are also
//! synced to disk per append, as their records are few and decide the run;
//! an engine log is synced when the handle closes, since its records are
//! many and replay regenerates what a power loss takes from its tail.
//!
//! A failed append ends the handle's writing: part of a line, or a whole
//! line whose sync failed, may be in the file past what the writer counted,
//! and a later append would join or repeat it. Every later write through
//! the handle is refused with the first failure, so the process ends, and
//! the next writer recovers from the files: it truncates a partial line and
//! counts a line that landed, so a retry of that record is stored once.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::{JoinError, spawn_blocking};

use crate::jsonl::clean_lines;
use crate::{
    Access, Digest, ExecutionId, LogId, OwnerId, Record, RunKey, RunLogs, RunStore, StoreError,
};

pub const RUN_FILE: &str = "run.json";
pub const COORDINATOR_FILE: &str = "coordinator.jsonl";
pub const RESOURCES_FILE: &str = "resources.jsonl";
pub const GRAPHS_DIR: &str = "graphs";
pub const EXECUTIONS_DIR: &str = "executions";
pub const EVENTS_FILE: &str = "events.jsonl";

/// An execution's directory relative to the run directory: the one spelling
/// of `executions/<execution>` the store and the host's execution work
/// directory share.
pub fn execution_relative_dir(execution: ExecutionId) -> PathBuf {
    Path::new(EXECUTIONS_DIR).join(format!("{:016x}", execution.raw()))
}

/// The store's own index under the run directory: the run's key, and the
/// owner holding the writer lease while one does, so a refused open can
/// name the holder.
#[derive(Serialize, Deserialize)]
struct RunFile {
    key:   RunKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<OwnerId>,
}

/// A store of one run, the one under `run_dir`.
pub struct RunDirStore {
    root:  PathBuf,
    /// The live writer handle, so a retry by its owner shares the lease and
    /// another owner in this process is refused without touching the lock.
    lease: Mutex<Option<(OwnerId, Weak<RunDirLogs>)>>,
}

impl RunDirStore {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            root:  run_dir.into(),
            lease: Mutex::new(None),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The key of the run stored here, read from `run.json`; `None` when
    /// the directory holds no run.
    pub fn stored_key(&self) -> Result<Option<RunKey>, StoreError> {
        Ok(self.run_file()?.map(|run| run.key))
    }

    fn run_file(&self) -> Result<Option<RunFile>, StoreError> {
        let path = self.root.join(RUN_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(self.io("read run.json", source)),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| StoreError::backend(self.locator(), "read run.json", source))
    }

    /// Open the run stored here, whatever its key: what a command given a
    /// run directory does. A directory with no run is [`StoreError::NotFound`].
    pub async fn open_stored(&self, access: Access) -> Result<Arc<dyn RunLogs>, StoreError> {
        let key = self.stored_key()?.ok_or_else(|| {
            self.io(
                "read run.json",
                io::Error::new(io::ErrorKind::NotFound, "the directory holds no run"),
            )
        })?;
        self.open(&key, access).await
    }

    fn locator(&self) -> String {
        self.root.display().to_string()
    }

    fn io(&self, action: &'static str, source: io::Error) -> StoreError {
        StoreError::io(self.locator(), action, source)
    }

    fn check_key(&self, key: &RunKey) -> Result<(), StoreError> {
        match self.stored_key()? {
            Some(stored) if stored == *key => Ok(()),
            _ => Err(StoreError::NotFound {
                key:     key.clone(),
                locator: self.locator(),
            }),
        }
    }

    /// Take the writer lease: the lock on `run.json`, then the holder's
    /// owner written into it so a refused open can name who holds the run.
    fn acquire_lease(
        &self,
        file: &mut File,
        key: &RunKey,
        owner: &OwnerId,
    ) -> Result<(), StoreError> {
        file.try_lock().map_err(|source| match source {
            fs::TryLockError::WouldBlock => StoreError::Leased {
                locator: self.locator(),
                owner:   self
                    .run_file()
                    .ok()
                    .flatten()
                    .and_then(|run| run.owner)
                    .unwrap_or_else(|| OwnerId::new("another process")),
            },
            fs::TryLockError::Error(source) => self.io("lock run.json", source),
        })?;
        write_run_file(file, &RunFile {
            key:   key.clone(),
            owner: Some(owner.clone()),
        })
        .map_err(|source| self.io("write run.json", source))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[async_trait::async_trait]
impl RunStore for RunDirStore {
    async fn open(&self, key: &RunKey, access: Access) -> Result<Arc<dyn RunLogs>, StoreError> {
        let Some(owner) = access.owner() else {
            self.check_key(key)?;
            return Ok(Arc::new(RunDirLogs {
                key:   key.clone(),
                owner: None,
                state: Arc::new(WriterState::new(self.root.clone())),
                lock:  Mutex::new(None),
            }));
        };
        let mut lease = lock(&self.lease);
        if let Some((holder, handle)) = lease.as_ref()
            && let Some(handle) = handle.upgrade()
        {
            if holder == owner
                && access
                    == (Access::Write {
                        owner: owner.clone(),
                    })
            {
                return Ok(handle);
            }
            return Err(StoreError::Leased {
                locator: self.locator(),
                owner:   holder.clone(),
            });
        }
        let path = self.root.join(RUN_FILE);
        let lock_file = match &access {
            Access::Create { .. } => {
                fs::create_dir_all(&self.root).map_err(|source| self.io("create", source))?;
                fs::create_dir_all(self.root.join(GRAPHS_DIR))
                    .map_err(|source| self.io("create graphs/", source))?;
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|source| {
                        if source.kind() == io::ErrorKind::AlreadyExists {
                            StoreError::Exists {
                                key:     key.clone(),
                                locator: self.locator(),
                            }
                        } else {
                            self.io("create run.json", source)
                        }
                    })?;
                self.acquire_lease(&mut file, key, owner)?;
                file
            }
            Access::Write { .. } => {
                self.check_key(key)?;
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|source| {
                        if source.kind() == io::ErrorKind::NotFound {
                            StoreError::NotFound {
                                key:     key.clone(),
                                locator: self.locator(),
                            }
                        } else {
                            self.io("open run.json", source)
                        }
                    })?;
                self.acquire_lease(&mut file, key, owner)?;
                file
            }
            Access::Read => unreachable!("a read open names no owner"),
        };
        let handle = Arc::new(RunDirLogs {
            key:   key.clone(),
            owner: Some(owner.clone()),
            state: Arc::new(WriterState::new(self.root.clone())),
            lock:  Mutex::new(Some(lock_file)),
        });
        *lease = Some((owner.clone(), Arc::downgrade(&handle)));
        Ok(handle)
    }
}

/// One log's open append handle and head.
struct LogHead {
    file: File,
    /// The seq the next record takes: the count of complete lines.
    next: u64,
}

/// What the file operations need: the root, and each opened log's head.
struct WriterState {
    root:   PathBuf,
    heads:  Mutex<BTreeMap<LogId, Arc<Mutex<LogHead>>>>,
    /// The first append that failed: this handle writes nothing more.
    failed: Mutex<Option<String>>,
}

impl WriterState {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            heads: Mutex::new(BTreeMap::new()),
            failed: Mutex::new(None),
        }
    }

    /// Refuse a write once an append has failed through this handle.
    fn refuse_after_failure(&self, action: &'static str) -> Result<(), StoreError> {
        match lock(&self.failed).as_ref() {
            Some(first) => Err(StoreError::backend(
                self.locator(),
                action,
                format!("an earlier append failed, and this handle writes nothing more: {first}"),
            )),
            None => Ok(()),
        }
    }

    fn locator(&self) -> String {
        self.root.display().to_string()
    }

    fn io(&self, action: &'static str, source: io::Error) -> StoreError {
        StoreError::io(self.locator(), action, source)
    }

    fn log_path(&self, log: &LogId) -> PathBuf {
        match log {
            LogId::Coordinator => self.root.join(COORDINATOR_FILE),
            LogId::Resources => self.root.join(RESOURCES_FILE),
            LogId::Execution(execution) => self
                .root
                .join(execution_relative_dir(*execution))
                .join(EVENTS_FILE),
        }
    }

    fn blob_path(&self, digest: Digest) -> PathBuf {
        self.root.join(GRAPHS_DIR).join(format!("{digest}.json"))
    }

    /// The log's head, opening the file on first use: a torn tail is
    /// truncated, the complete lines counted, and the file opened to append.
    fn head(&self, log: &LogId) -> Result<Arc<Mutex<LogHead>>, StoreError> {
        let mut heads = lock(&self.heads);
        if let Some(head) = heads.get(log) {
            return Ok(head.clone());
        }
        let path = self.log_path(log);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| self.io("create", source))?;
        }
        let next = match fs::read(&path) {
            Ok(bytes) => {
                let lines = clean_lines(&bytes);
                let (clean_len, torn) = (lines.clean_len, lines.torn);
                let count = lines.count() as u64;
                if torn {
                    let file = OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .map_err(|source| self.io("open", source))?;
                    file.set_len(clean_len as u64)
                        .and_then(|()| file.sync_data())
                        .map_err(|source| self.io("truncate", source))?;
                }
                count
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => 0,
            Err(source) => return Err(self.io("read", source)),
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| self.io("open", source))?;
        let head = Arc::new(Mutex::new(LogHead { file, next }));
        heads.insert(*log, head.clone());
        Ok(head)
    }

    /// Append, or refuse after an earlier failure. A conflict writes
    /// nothing, so it is not a failure of the handle.
    fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        self.refuse_after_failure("append")?;
        let appended = self.append_records(log, records);
        if let Err(error) = &appended
            && !matches!(error, StoreError::Conflict { .. })
        {
            lock(&self.failed).get_or_insert_with(|| error.to_string());
        }
        appended
    }

    fn append_records(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        let head = self.head(log)?;
        let mut head = lock(&head);
        let stored = |seq: u64| self.read_log(log).ok()?.into_iter().find(|r| r.seq == seq);
        let fresh = crate::admit(log, head.next, records, stored)?;
        if fresh.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        for record in &fresh {
            serde_json::to_writer(&mut bytes, &record.record)
                .map_err(|source| StoreError::backend(self.locator(), "encode", source))?;
            bytes.push(b'\n');
        }
        head.file
            .write_all(&bytes)
            .and_then(|()| head.file.flush())
            .and_then(|()| match log {
                LogId::Coordinator | LogId::Resources => head.file.sync_data(),
                LogId::Execution(_) => Ok(()),
            })
            .map_err(|source| self.io("append", source))?;
        head.next += fresh.len() as u64;
        Ok(())
    }

    fn read_log(&self, log: &LogId) -> Result<Vec<Record>, StoreError> {
        let path = self.log_path(log);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(self.io("read", source)),
        };
        let mut records = Vec::new();
        for line in clean_lines(&bytes) {
            let value: Value = serde_json::from_slice(line).map_err(|source| {
                StoreError::backend(
                    path.display().to_string(),
                    "read",
                    format!("line {} is not JSON: {source}", records.len() + 1),
                )
            })?;
            let record = Record::from_value(value).map_err(|source| {
                StoreError::backend(path.display().to_string(), "read", source)
            })?;
            records.push(record);
        }
        Ok(records)
    }

    fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        self.refuse_after_failure("put blob")?;
        let digest = Digest::of(bytes);
        let path = self.blob_path(digest);
        if path.exists() {
            // Content-addressed: the same bytes are already there.
            return Ok(digest);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| self.io("create", source))?;
        }
        write_atomically(&path, bytes).map_err(|(action, source)| self.io(action, source))?;
        Ok(digest)
    }

    fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError> {
        match fs::read(self.blob_path(digest)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(self.io("read", source)),
        }
    }
}

impl Drop for WriterState {
    fn drop(&mut self) {
        // The engine logs' one sync, when the run's handle closes.
        for head in lock(&self.heads).values() {
            let _ = lock(head).file.sync_data();
        }
    }
}

/// Publish `bytes` at `path`: a temp write with fsync, a rename, and a
/// parent-directory sync, so a crash leaves the file whole or absent.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), (&'static str, io::Error)> {
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|source| ("create", source))?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_data())
        .map_err(|source| ("write", source))?;
    fs::rename(&temporary, path).map_err(|source| ("rename", source))?;
    if let Some(parent) = path.parent() {
        File::open(parent)
            .and_then(|directory| directory.sync_data())
            .map_err(|source| ("sync", source))?;
    }
    Ok(())
}

/// One run directory, opened.
pub struct RunDirLogs {
    key:   RunKey,
    owner: Option<OwnerId>,
    state: Arc<WriterState>,
    /// The lease: `run.json`, locked for the handle's life.
    lock:  Mutex<Option<File>>,
}

impl Drop for RunDirLogs {
    fn drop(&mut self) {
        // Clear the holder before the lock ends with the file. Best effort:
        // a crash leaves the old owner's name, which the next holder
        // overwrites when it takes the lease.
        if let Some(mut file) = lock(&self.lock).take() {
            let _ = write_run_file(&mut file, &RunFile {
                key:   self.key.clone(),
                owner: None,
            });
        }
    }
}

/// Rewrite `run.json` in place, whole, and sync it.
fn write_run_file(file: &mut File, run: &RunFile) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(run).map_err(io::Error::other)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_data()
}

impl RunDirLogs {
    /// The run's key.
    pub fn key(&self) -> &RunKey {
        &self.key
    }

    /// The run directory.
    pub fn root(&self) -> &Path {
        &self.state.root
    }

    fn check_writer(&self) -> Result<(), StoreError> {
        // The file lock ends with the process, so a handle that holds it is
        // never stale: the only refusal is a reader asked to write.
        self.owner.as_ref().map(|_| ()).ok_or(StoreError::ReadOnly)
    }

    fn joined<T>(&self, result: Result<T, JoinError>) -> Result<T, StoreError> {
        result.map_err(|source| StoreError::backend(self.locator(), "run", source))
    }
}

#[async_trait::async_trait]
impl RunLogs for RunDirLogs {
    fn locator(&self) -> String {
        self.state.locator()
    }

    async fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        self.check_writer()?;
        let state = self.state.clone();
        let log = *log;
        let records = records.to_vec();
        self.joined(spawn_blocking(move || state.append(&log, &records)).await)?
    }

    async fn read(&self, log: &LogId) -> Result<Vec<Record>, StoreError> {
        let state = self.state.clone();
        let log = *log;
        self.joined(spawn_blocking(move || state.read_log(&log)).await)?
    }

    async fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        self.check_writer()?;
        let state = self.state.clone();
        let bytes = bytes.to_vec();
        self.joined(spawn_blocking(move || state.put_blob(&bytes)).await)?
    }

    async fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError> {
        let state = self.state.clone();
        self.joined(spawn_blocking(move || state.get_blob(digest)).await)?
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::io::Write as _;

    use serde_json::json;
    use testkit::RunDir;

    use super::{WriterState, lock};
    use crate::{LogId, Record};

    fn record(seq: u64) -> Record {
        Record::from_value(json!({
            "seq": seq,
            "origin": "external",
            "recorded_at": 1_000 + seq,
            "body": { "event": "event" },
        }))
        .expect("a test record has seq and recorded_at")
    }

    /// Write `bytes` past what the writer counted, as a write that failed
    /// partway, or whose sync failed, leaves them.
    fn leave_bytes(state: &WriterState, log: &LogId, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(state.log_path(log))
            .and_then(|mut file| file.write_all(bytes))
            .expect("writes past the writer's count");
    }

    /// Make the log's next write fail, as a full disk or a failed sync
    /// does: its cached handle turns read-only.
    fn fail_writes(state: &WriterState, log: &LogId) {
        let head = state.head(log).expect("the log is open");
        lock(&head).file = File::open(state.log_path(log)).expect("reopens read-only");
    }

    #[test]
    fn a_failed_append_refuses_every_later_write_and_the_next_writer_recovers() {
        let dir = RunDir::new("store-failed-append");
        let state = WriterState::new(dir.path().to_path_buf());
        state
            .append(&LogId::Coordinator, &[record(0)])
            .expect("appends");
        leave_bytes(&state, &LogId::Coordinator, b"{\"seq\":1,\"origin\":\"ext");
        fail_writes(&state, &LogId::Coordinator);

        state
            .append(&LogId::Coordinator, &[record(1)])
            .expect_err("the write fails");
        for refused in [
            state.append(&LogId::Coordinator, &[record(1)]).map(drop),
            state.append(&LogId::Resources, &[record(0)]).map(drop),
            state.put_blob(b"graph").map(drop),
        ] {
            let error = refused.expect_err("the handle writes nothing more");
            assert!(
                error.to_string().contains("an earlier append failed"),
                "{error}"
            );
        }
        assert!(
            !state.log_path(&LogId::Resources).exists(),
            "the refused append touched no file"
        );

        // The next process: the partial line is truncated, the log reads
        // whole, and the record the failure cut off is appended once.
        let next = WriterState::new(dir.path().to_path_buf());
        next.append(&LogId::Coordinator, &[record(1)])
            .expect("appends after the partial line");
        assert_eq!(next.read_log(&LogId::Coordinator).expect("reads"), vec![
            record(0),
            record(1)
        ]);
    }

    #[test]
    fn a_record_that_landed_before_a_failed_sync_is_stored_once() {
        let dir = RunDir::new("store-failed-sync");
        let state = WriterState::new(dir.path().to_path_buf());
        state
            .append(&LogId::Coordinator, &[record(0)])
            .expect("appends");
        let mut line = serde_json::to_vec(&record(1).record).expect("encodes");
        line.push(b'\n');
        leave_bytes(&state, &LogId::Coordinator, &line);
        fail_writes(&state, &LogId::Coordinator);
        state
            .append(&LogId::Coordinator, &[record(1)])
            .expect_err("the sync fails");
        state
            .append(&LogId::Coordinator, &[record(1)])
            .expect_err("the retry is refused in this process");

        // The next process counts the line that landed: the retry of the
        // same record is accepted without a second copy.
        let next = WriterState::new(dir.path().to_path_buf());
        next.append(&LogId::Coordinator, &[record(1)])
            .expect("the retry is accepted");
        assert_eq!(next.read_log(&LogId::Coordinator).expect("reads"), vec![
            record(0),
            record(1)
        ]);
        assert_eq!(
            fs::read_to_string(next.log_path(&LogId::Coordinator))
                .expect("the log")
                .lines()
                .count(),
            2
        );
    }
}
