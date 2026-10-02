//! A failed write to the run's store ends the lifetime: nothing after it is
//! recorded, no firing fails for it, and the next lifetime resumes from what
//! the store holds. The lease records are the case that matters most: the
//! lease layer once turned a failed lease write into an acquire failure, so
//! a store glitch failed the workflow.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use engine::Event;
use execution::{
    CoordinatorError, CoordinatorEvent, ExecutionId, host, read_coordinator_log, read_execution_log,
};
use ir::{GraphBuilder, RunStatus, ScopeId};
use runtime::{RunOptions, Runtime};
use store::{Access, Digest, LogId, MemoryRunStore, Record, RunKey, RunLogs, RunStore, StoreError};
use testkit::{RunDir, add_script};

/// The in-memory store, failing one append to `log`: the one at `fail_at`,
/// counting from 0. A lost reply stores the record, then fails.
struct FailingStore {
    inner:      Arc<MemoryRunStore>,
    log:        LogId,
    fail_at:    usize,
    lost_reply: bool,
    appended:   Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RunStore for FailingStore {
    async fn open(&self, key: &RunKey, access: Access) -> Result<Arc<dyn RunLogs>, StoreError> {
        Ok(Arc::new(FailingLogs {
            inner:      self.inner.open(key, access).await?,
            log:        self.log,
            fail_at:    self.fail_at,
            lost_reply: self.lost_reply,
            appended:   Arc::clone(&self.appended),
        }))
    }
}

struct FailingLogs {
    inner:      Arc<dyn RunLogs>,
    log:        LogId,
    fail_at:    usize,
    lost_reply: bool,
    appended:   Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl RunLogs for FailingLogs {
    fn locator(&self) -> String {
        self.inner.locator()
    }

    async fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        let fails =
            *log == self.log && self.appended.fetch_add(1, Ordering::SeqCst) == self.fail_at;
        if fails && !self.lost_reply {
            return Err(StoreError::backend(
                self.locator(),
                "append",
                "the disk is full",
            ));
        }
        self.inner.append(log, records).await?;
        if fails {
            return Err(StoreError::backend(
                self.locator(),
                "append",
                "the reply was lost",
            ));
        }
        Ok(())
    }

    async fn read(&self, log: &LogId) -> Result<Vec<Record>, StoreError> {
        self.inner.read(log).await
    }

    async fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        self.inner.put_blob(bytes).await
    }

    async fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get_blob(digest).await
    }
}

fn one_step() -> ir::Graph {
    let mut b = GraphBuilder::new();
    add_script(&mut b, "work", ScopeId::new(0), "echo one");
    b.build()
}

/// A write to `log` fails at its `fail_at`th append: the lifetime ends with
/// the store's failure, nothing fails for it, and a resume finishes the run.
async fn a_failed_write_ends_the_lifetime(log: LogId, fail_at: usize, lost_reply: bool) {
    let key = RunKey::new("failed-write");
    let dir = RunDir::new("store-failure-write");
    let memory = Arc::new(MemoryRunStore::new());
    let store = Arc::new(FailingStore {
        inner: Arc::clone(&memory),
        log,
        fail_at,
        lost_reply,
        appended: Arc::new(AtomicUsize::new(0)),
    });
    let mut options = RunOptions::new(dir.path());
    options.run_key = Some(key.clone());
    let rt = Runtime::standard().store(store).options(options);

    let first = host::run(&rt, one_step()).await;
    let failure = Mutex::new(None);
    match first {
        Err(host::HostError::Coordinator(CoordinatorError::StoreFailed(message))) => {
            *failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(message);
        }
        other => panic!(
            "the lifetime ends with the store's failure, got {:?}",
            other.map(|report| report.status)
        ),
    }

    // Nothing failed for the store: no scope failure, no failed step, no
    // invocation result.
    let logs = memory.open(&key, Access::Read).await.expect("reads");
    let records = read_coordinator_log(&*logs).await.expect("decodes");
    assert!(
        !records
            .iter()
            .any(|record| matches!(record.body, CoordinatorEvent::InvocationFinished { .. })),
        "the run did not end"
    );
    let engine = read_execution_log(&*logs, ExecutionId::new(0))
        .await
        .expect("the engine log reads");
    assert!(
        !engine.log.events().any(|event| match event {
            Event::ScopeFailed { .. } => true,
            Event::StepFinished { outcome, .. } => outcome.status.failure_info().is_some(),
            _ => false,
        }),
        "no firing failed for the store: {:?}",
        failure.lock().unwrap_or_else(PoisonError::into_inner)
    );

    // The next lifetime resumes from what the store holds and succeeds.
    let report = host::resume(&rt).await;
    assert!(
        matches!(&report, Ok(report) if report.status == RunStatus::Success),
        "the resumed run succeeds: {:?}",
        report.map(|report| report.status)
    );
}

/// The lease's reservation, the first resource record.
#[tokio::test]
async fn a_lease_write_that_fails_ends_the_lifetime_and_the_run_resumes() {
    a_failed_write_ends_the_lifetime(LogId::Resources, 0, false).await;
}

#[tokio::test]
async fn a_lease_write_whose_reply_is_lost_ends_the_lifetime_and_the_run_resumes() {
    a_failed_write_ends_the_lifetime(LogId::Resources, 0, true).await;
}

/// The coordinator's own append after the run's creation (`run.started`,
/// the graph, the root's declarations): the lifetime ends with the store's
/// failure too, not the record's.
#[tokio::test]
async fn a_coordinator_write_that_fails_ends_the_lifetime_and_the_run_resumes() {
    a_failed_write_ends_the_lifetime(LogId::Coordinator, 4, false).await;
}
