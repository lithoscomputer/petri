use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use driver::lifecycle::{ExecutionHooks, HookContext, Note, RunFinished, ScopeReleased};
use execution::controls::ControlService;
use execution::events::replay_run;
use execution::inspect::inspect_run;
use execution::{
    Coordinator, CoordinatorError, CoordinatorEvent, CoordinatorOptions, CoordinatorRecord,
    CoordinatorState, GraphDigest, InvocationId, StateError, read_coordinator_log,
};
use ir::{FinalizationFailure, GraphBuilder, Outcome, RunStatus, ScopeId, StepRef};
use runtime::{RunOptions, Runtime};
use steps::{Step, StepCtx};
use testkit::RunDir;
use tokio::sync::Notify;
use tokio::time::timeout;

struct Finalizer {
    entered:      Notify,
    proceed:      Notify,
    failure:      Option<FinalizationFailure>,
    calls:        AtomicUsize,
    observations: AtomicUsize,
    releases:     AtomicUsize,
}

impl Finalizer {
    fn new(failure: Option<FinalizationFailure>) -> Self {
        Self {
            entered: Notify::new(),
            proceed: Notify::new(),
            failure,
            calls: AtomicUsize::new(0),
            observations: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ExecutionHooks for Finalizer {
    fn requires_run_finalization(&self) -> bool {
        true
    }

    async fn finalize_run(
        &self,
        _context: &HookContext,
        _finished: RunFinished,
    ) -> Result<(), FinalizationFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.proceed.notified().await;
        self.failure.clone().map_or(Ok(()), Err)
    }

    async fn run_finished(&self, _context: &HookContext, _finished: RunFinished) -> Vec<Note> {
        self.observations.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }

    async fn scope_released(&self, _context: &HookContext, _released: ScopeReleased) -> Vec<Note> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }
}

struct CancelReady(Arc<Notify>);

struct ResultStep;

#[async_trait::async_trait]
impl Step for ResultStep {
    const NAME: &'static str = "test/finalization-result";
    type Config = RunStatus;

    async fn run(&self, status: RunStatus, mut ctx: StepCtx) -> Outcome {
        match status {
            RunStatus::Success => Outcome::success(serde_json::Value::Null),
            RunStatus::Failed => Outcome::failure("execution failed"),
            RunStatus::Cancelled => {
                let ready = match ctx.require_capability::<CancelReady>() {
                    Ok(ready) => ready,
                    Err(error) => return error.into(),
                };
                ready.0.notify_one();
                let _ = ctx.control.recv().await;
                Outcome::cancelled()
            }
        }
    }
}

/// A coordinator for a new run of `graph`, and the graph's digest.
async fn start(
    runtime: &Runtime,
    directory: &RunDir,
    graph: GraphBuilder,
) -> (Coordinator, GraphDigest) {
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("create");
    let digest = coordinator
        .register_graph(&graph.build())
        .await
        .expect("register");
    (coordinator, digest)
}

/// A committed run's state refuses a second end, and the state before it
/// refuses an end that does not match execution and finalization.
fn assert_run_finish_is_validated(
    records: &[CoordinatorRecord],
    committed: &CoordinatorState,
    expected: RunStatus,
    failure: Option<FinalizationFailure>,
) {
    let mut before_finish =
        CoordinatorState::replay(&records[..records.len() - 1]).expect("replay before completion");
    let unchanged = before_finish.clone();
    assert_eq!(
        before_finish.apply(&CoordinatorEvent::RunFinished {
            status:               if expected == RunStatus::Success {
                RunStatus::Failed
            } else {
                RunStatus::Success
            },
            finalization_failure: failure,
        }),
        Err(StateError::RunStatusMismatch)
    );
    assert_eq!(before_finish, unchanged);
    let mut immutable = committed.clone();
    assert_eq!(
        immutable.apply(&CoordinatorEvent::RunFinished {
            status:               RunStatus::Success,
            finalization_failure: None,
        }),
        Err(StateError::DuplicateRunFinish)
    );
    assert_eq!(&immutable, committed);
}

async fn complete(execution_status: RunStatus, failure: Option<FinalizationFailure>) {
    let directory = RunDir::new("required-finalization");
    let hooks = Arc::new(Finalizer::new(failure.clone()));
    let cancel_ready = Arc::new(Notify::new());
    let runtime = Runtime::standard()
        .capability(CancelReady(cancel_ready.clone()))
        .step(ResultStep)
        .hooks(ControlService::new().hooks(Some(hooks.clone())))
        .options(RunOptions::new(directory.path()));
    let mut graph = GraphBuilder::new();
    graph.add_node(
        "result",
        ScopeId::new(0),
        StepRef::new(
            ResultStep::NAME,
            serde_json::to_value(execution_status).expect("status encodes"),
        ),
    );
    let (mut coordinator, digest) = start(&runtime, &directory, graph).await;
    let logs = coordinator.store().logs().clone();
    let handle = coordinator.handle();
    let mut running = Box::pin(coordinator.run_root(digest, BTreeMap::new()));
    if execution_status == RunStatus::Cancelled {
        tokio::select! {
            () = cancel_ready.notified() => handle.cancel_root(),
            result = &mut running => panic!("finished before cancellation: {result:?}"),
        }
    }
    tokio::select! {
        () = hooks.entered.notified() => {},
        result = &mut running => panic!("finished before finalization: {result:?}"),
    }
    let pending_records = read_coordinator_log(&*logs).await.expect("read pending");
    let pending = CoordinatorState::replay(&pending_records).expect("replay pending");
    assert!(pending.required_finalization);
    assert_eq!(pending.run_status, None);
    let inspection = inspect_run(&*logs).await.expect("inspect pending");
    assert!(!inspection.complete);
    assert_eq!(inspection.status, None);
    assert_eq!(hooks.observations.load(Ordering::SeqCst), 0);
    assert_eq!(
        hooks.releases.load(Ordering::SeqCst),
        0,
        "release must wait"
    );
    hooks.proceed.notify_one();
    let result = timeout(Duration::from_secs(5), running)
        .await
        .expect("settles")
        .expect("run");
    assert_eq!(
        result.status, execution_status,
        "invocation is execution evidence"
    );
    let expected = ir::finalized_status(execution_status, failure.as_ref());
    let report = coordinator.take_root_report().expect("report");
    assert_eq!(report.status, expected);
    assert_eq!(report.finalization_failure, failure);
    assert_eq!(report.state.folded_status(), execution_status);
    assert_eq!(hooks.observations.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.releases.load(Ordering::SeqCst), 1);
    let committed = coordinator.store().state().clone();
    assert_eq!(committed.run_status, Some(expected));
    assert_eq!(committed.finalization_failure, failure);
    assert_eq!(
        committed.invocations[&InvocationId::ROOT]
            .result
            .as_ref()
            .expect("root")
            .status,
        execution_status
    );
    drop(logs);
    coordinator.finish().await;
    let logs = testkit::read_run_dir(directory.path()).await;
    let records = read_coordinator_log(&*logs).await.expect("records");
    assert_eq!(
        CoordinatorState::replay(&records).expect("replay"),
        committed
    );
    let inspection = inspect_run(&*logs).await.expect("inspect");
    assert!(inspection.complete, "{:?}", inspection.incomplete);
    assert_eq!(inspection.status, Some(expected.to_string()));
    assert_eq!(inspection.finalization_failure, failure);
    let events = replay_run(&*logs).await.expect("public replay");
    assert!(events.iter().any(|event| matches!(event.coordinator(),
        Some(CoordinatorEvent::RunFinished { status, finalization_failure })
            if *status == expected && *finalization_failure == failure)));
    assert_run_finish_is_validated(&records, &committed, expected, failure.clone());
    // The callback would block if called again: committed completion must
    // return the original result without rerunning it.
    let mut resumed = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("resume committed");
    timeout(
        Duration::from_secs(5),
        resumed.run_root(digest, BTreeMap::new()),
    )
    .await
    .expect("committed completion does not finalize again")
    .expect("reconstruct");
    let report = resumed.take_root_report().expect("resumed report");
    assert_eq!(report.status, expected);
    assert_eq!(report.finalization_failure, failure);
    assert_eq!(hooks.calls.load(Ordering::SeqCst), 1);
    resumed.finish().await;
}

#[tokio::test]
async fn rejected_finalization_is_durable_failure_with_details() {
    complete(
        RunStatus::Success,
        Some(FinalizationFailure::new("publish_failed", "push rejected")),
    )
    .await;
}

#[tokio::test]
async fn successful_execution_and_finalization_succeed() {
    complete(RunStatus::Success, None).await;
}

#[tokio::test]
async fn successful_finalization_cannot_upgrade_failed_execution() {
    complete(RunStatus::Failed, None).await;
}

#[tokio::test]
async fn cancellation_keeps_precedence_and_releases_resources() {
    complete(
        RunStatus::Cancelled,
        Some(FinalizationFailure::new(
            "cleanup_failed",
            "host work failed",
        )),
    )
    .await;
}

#[tokio::test]
async fn interrupted_completion_requires_the_finalizer_on_resume() {
    let directory = RunDir::new("finalization-interrupted");
    let hooks = Arc::new(Finalizer::new(None));
    hooks.proceed.notify_one();
    let runtime = Runtime::standard()
        .hooks(hooks.clone())
        .options(RunOptions::new(directory.path()));
    let mut graph = GraphBuilder::new();
    graph.add_step("only", ScopeId::new(0), "noop");
    let (mut coordinator, digest) = start(&runtime, &directory, graph).await;
    coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("run");
    coordinator.finish().await;
    // The workflow and its result survived, but the completion commit did
    // not. Recovery cannot promote that narrower result into run success.
    let decoded = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("coordinator log decodes");
    let mut prefix = Vec::new();
    for record in decoded {
        if matches!(record.body, CoordinatorEvent::RunFinished { .. }) {
            continue;
        }
        serde_json::to_writer(&mut prefix, &record).expect("record encodes");
        prefix.push(b'\n');
    }
    fs::write(directory.path().join("coordinator.jsonl"), prefix).expect("interrupted log");
    let no_finalizer = Runtime::standard().options(RunOptions::new(directory.path()));
    let Err(error) = Coordinator::resume(
        no_finalizer.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    else {
        panic!("missing required finalizer was accepted");
    };
    assert!(matches!(
        error,
        CoordinatorError::FinalizationRequirementMismatch
    ));
    let failure = FinalizationFailure::new(
        "publication_unverified",
        "resources unavailable after interruption",
    );
    let recovered_hooks = Arc::new(Finalizer::new(Some(failure.clone())));
    let recovered_runtime = Runtime::standard()
        .hooks(recovered_hooks.clone())
        .options(RunOptions::new(directory.path()));
    let mut resumed = Coordinator::resume(
        recovered_runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("resume with finalizer");
    let logs = resumed.store().logs().clone();
    let mut running = Box::pin(resumed.run_root(digest, BTreeMap::new()));
    tokio::select! {
        () = recovered_hooks.entered.notified() => {},
        result = &mut running => panic!("invented completion: {result:?}"),
    }
    assert_eq!(
        CoordinatorState::replay(&read_coordinator_log(&*logs).await.expect("read"))
            .expect("replay")
            .run_status,
        None
    );
    recovered_hooks.proceed.notify_one();
    running.await.expect("recovered run");
    assert_eq!(resumed.store().state().run_status, Some(RunStatus::Failed));
    assert_eq!(resumed.store().state().finalization_failure, Some(failure));
    resumed.finish().await;
}

#[tokio::test]
async fn failed_execution_retains_a_rejected_finalization_detail() {
    complete(
        RunStatus::Failed,
        Some(FinalizationFailure::new(
            "finalize_failed",
            "host work rejected",
        )),
    )
    .await;
}
