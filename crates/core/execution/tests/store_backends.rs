//! The coordinator over a store that is not the run directory: a run
//! through the in-memory store leaves no file of record, exports, inspects
//! the same as a run directory's run, and resumes from what the store
//! holds. The trait contract itself is `testkit::run_store::conformance`;
//! these are the run-level checks over both in-tree backends.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use execution::events::{replay_run, verify_export};
use execution::inspect::inspect_run;
use execution::{
    Access, Coordinator, CoordinatorError, CoordinatorEvent, CoordinatorOptions, ExecutionId,
    InvocationId, LeaseState, MemoryRunStore, OwnerId, ResourceStore, RunKey, RunStore as _,
    SandboxAllocationKey, host, read_coordinator_log, read_execution_log,
};
use executor::WorkspaceId;
use ir::{GraphBuilder, Outcome, RunStatus, ScopeId, StepEvent};
use runtime::steps::{Registry, StepCtx, StepRunner};
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::run_store::{conformance, stale_owner_conformance};
use testkit::{RunDir, add_script};
use tokio::sync::Notify;
use tokio::time::timeout;

fn two_steps() -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let first = add_script(&mut b, "first", scope, "echo one");
    let second = add_script(&mut b, "second", scope, "echo two");
    b.link(first, second);
    b.build()
}

/// The files the run-directory store writes: none may appear under a run
/// that lives in another store.
fn files_of_record(run_dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for name in [
        execution::RUN_FILE,
        execution::COORDINATOR_FILE,
        execution::RESOURCES_FILE,
    ] {
        if run_dir.join(name).exists() {
            found.push(name.to_owned());
        }
    }
    if run_dir.join(execution::GRAPHS_DIR).exists() {
        found.push(execution::GRAPHS_DIR.to_owned());
    }
    if let Ok(executions) = fs::read_dir(run_dir.join(execution::EXECUTIONS_DIR)) {
        for entry in executions.flatten() {
            if entry.path().join(execution::EVENTS_FILE).exists() {
                found.push(format!(
                    "{}/{}",
                    entry.path().display(),
                    execution::EVENTS_FILE
                ));
            }
        }
    }
    found
}

/// A run through the in-memory store: no file of record under the run
/// directory, the export check holds over the store, and the inspection
/// equals a run directory's for the same run, field for field except the
/// locator.
#[tokio::test]
async fn a_run_over_the_memory_store_leaves_no_file_of_record_and_inspects_the_same() {
    let key = RunKey::new("memory-run");
    let memory_dir = RunDir::new("store-memory");
    let memory = Arc::new(MemoryRunStore::new());
    let mut options = RunOptions::new(memory_dir.path());
    options.run_key = Some(key.clone());
    let rt = Runtime::standard().store(memory.clone()).options(options);
    let report = host::run(&rt, two_steps())
        .await
        .expect("the run completes");
    assert_eq!(report.status, RunStatus::Success);
    assert_eq!(
        files_of_record(memory_dir.path()),
        Vec::<String>::new(),
        "the run directory holds no record of the run"
    );

    let logs = rt
        .open_run(memory_dir.path(), Access::Read)
        .await
        .expect("the run opens in its store");
    verify_export(&*logs)
        .await
        .expect("the stream exports the store");
    let events = replay_run(&*logs).await.expect("the store projects");
    assert!(!events.is_empty());
    let from_memory = inspect_run(&*logs).await.expect("inspects");
    assert!(from_memory.complete, "{:?}", from_memory.incomplete);
    assert_eq!(from_memory.run_key, key);

    // The same run through a run directory.
    let dir = RunDir::new("store-run-dir");
    let mut options = RunOptions::new(dir.path());
    options.run_key = Some(key);
    let rt = Runtime::standard().options(options);
    let report = host::run(&rt, two_steps())
        .await
        .expect("the run completes");
    assert_eq!(report.status, RunStatus::Success);
    let from_dir = inspect_run(&*testkit::read_run_dir(dir.path()).await)
        .await
        .expect("inspects");
    let mut from_memory = from_memory;
    from_memory.locator.clone_from(&from_dir.locator);
    assert_eq!(from_memory, from_dir, "field for field, except the locator");
}

const PARKING: ir::StepKindId = ir::StepKindId::new_static("parking");

/// Sends one acknowledged event, then, when told to park, announces the
/// acknowledgement and waits for a crash that never lets it finish.
struct ParkingStep {
    acked: Arc<Notify>,
    park:  bool,
}

impl ir::StepKind for ParkingStep {
    fn id(&self) -> ir::StepKindId {
        PARKING
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "parking"
    }
}

#[async_trait::async_trait]
impl StepRunner for ParkingStep {
    async fn run(&self, mut ctx: StepCtx) -> Outcome {
        ctx.logs
            .send_acked(StepEvent::Custom(json!({ "kind": "durable-test" })))
            .await
            .expect("acknowledged");
        if self.park {
            self.acked.notify_one();
            let _ = ctx.control.recv().await;
        }
        Outcome::success(json!("done"))
    }
}

fn parking_runtime(
    store: &Arc<MemoryRunStore>,
    dir: &RunDir,
    key: &RunKey,
    acked: &Arc<Notify>,
    park: bool,
) -> Runtime {
    let mut registry = Registry::new();
    registry.register_runner(Arc::new(ParkingStep {
        acked: acked.clone(),
        park,
    }));
    let mut options = RunOptions::new(dir.path());
    options.run_key = Some(key.clone());
    Runtime::standard()
        .steps(registry)
        .store(store.clone())
        .options(options)
}

/// A crash mid-execution and a resume over the same in-memory store: the
/// acknowledged record is what the store holds when the coordinator dies,
/// the resume continues the engine log after it, and the finished run
/// exports.
#[tokio::test]
async fn a_resume_over_the_memory_store_continues_the_stored_engine_log() {
    let dir = RunDir::new("store-memory-resume");
    let store = Arc::new(MemoryRunStore::new());
    let key = RunKey::new("memory-resume");
    let acked = Arc::new(Notify::new());
    let mut b = GraphBuilder::new();
    b.add_step("work", ScopeId::new(0), PARKING);
    let graph = b.build();

    let crashing = parking_runtime(&store, &dir, &key, &acked, true);
    let run = tokio::spawn({
        let graph = graph.clone();
        async move { host::run(&crashing, graph).await }
    });
    timeout(Duration::from_secs(10), acked.notified())
        .await
        .expect("the acknowledgement arrives");
    // The crash: the coordinator, its writer and the parked step go away.
    run.abort();
    let _ = run.await;

    let resuming = parking_runtime(&store, &dir, &key, &acked, false);
    let stored = {
        let logs = resuming
            .open_run(dir.path(), Access::Read)
            .await
            .expect("the crashed run reads");
        read_execution_log(&*logs, ExecutionId::new(0))
            .await
            .expect("the stored log decodes")
            .log
    };
    assert!(
        stored.events().any(|event| matches!(
            event,
            engine::Event::StepProgressRecorded { ev: StepEvent::Custom(value), .. }
                if value["kind"] == "durable-test"
        )),
        "the acknowledged record is in the store the crash left behind"
    );
    let report = host::resume(&resuming).await.expect("the run resumes");
    assert_eq!(report.status, RunStatus::Success);
    let logs = resuming
        .open_run(dir.path(), Access::Read)
        .await
        .expect("the finished run reads");
    let complete = read_execution_log(&*logs, ExecutionId::new(0))
        .await
        .expect("the stored log decodes");
    assert_eq!(
        complete.log, report.state.log,
        "the store holds the resumed log"
    );
    assert!(complete.log.len() > stored.len(), "the resume continued it");
    assert_eq!(
        &complete.log.records()[..stored.len()],
        stored.records(),
        "the stored prefix is untouched"
    );
    verify_export(&*logs)
        .await
        .expect("the resumed run exports");
}

/// The resource log: one record per transition, and the latest record per
/// lease is the state a reopened store sees.
#[tokio::test]
async fn the_latest_resource_record_per_lease_wins_on_open() {
    let store = MemoryRunStore::new();
    let key = RunKey::new("resources");
    let logs = store
        .open(&key, Access::Create {
            owner: execution::OwnerId::mint(),
        })
        .await
        .expect("creates");
    let mut resources = ResourceStore::load(&logs).await.expect("loads");
    let allocation = SandboxAllocationKey {
        invocation: InvocationId::ROOT,
        scope:      engine::ScopeIdentity::Declared(ScopeId::new(0)),
    };
    let lease = resources
        .ensure_record(
            allocation.clone(),
            "docker",
            WorkspaceId::new("scope-0"),
            ir::RuntimeSpec::default(),
            None,
        )
        .await
        .expect("reserves")
        .lease;
    resources
        .update(lease, |record| {
            record.state = LeaseState::Live;
            record.resource_id = Some("box-1".into());
        })
        .await
        .expect("live");
    resources
        .update(lease, |record| record.state = LeaseState::Stopped)
        .await
        .expect("stopped");
    let stored = logs
        .read(&execution::LogId::Resources)
        .await
        .expect("reads");
    assert_eq!(stored.len(), 3, "one record per transition");

    let reopened = ResourceStore::load(&logs).await.expect("reloads");
    let record = reopened.resolve(lease).expect("the lease");
    assert_eq!(record.state, LeaseState::Stopped);
    assert_eq!(record.resource_id.as_deref(), Some("box-1"));
    assert_eq!(reopened.records().count(), 1, "the latest record per lease");
    // The same allocation resolves to the same lease, not a new one, and
    // appends nothing.
    let mut reopened = reopened;
    let again = reopened
        .ensure_record(
            allocation,
            "docker",
            WorkspaceId::new("scope-0"),
            ir::RuntimeSpec::default(),
            None,
        )
        .await
        .expect("the allocation is known")
        .lease;
    assert_eq!(again, lease);
    assert_eq!(
        logs.read(&execution::LogId::Resources)
            .await
            .expect("reads")
            .len(),
        3
    );
}

/// The trait's own store-level conformance, over the in-memory backend a
/// host copies: see `testkit::run_store`.
#[tokio::test]
async fn the_memory_store_passes_the_store_conformance() {
    conformance(|| Arc::new(MemoryRunStore::new())).await;
    let store = MemoryRunStore::new();
    stale_owner_conformance(&store, |key| store.release(key)).await;
    let _ = store.open(&RunKey::new("noop"), Access::Read).await;
}

/// A run whose creation a crash cut short: `run.started` and its graph are
/// stored, and no invocation. A resume refuses it as never started;
/// starting it again under the same key finishes the creation and runs it,
/// once. A run that started is not started again.
#[tokio::test]
async fn a_run_cut_short_at_its_creation_is_started_again_under_its_key() {
    let key = RunKey::new("cut-short");
    let dir = RunDir::new("store-cut-short");
    let memory = Arc::new(MemoryRunStore::new());
    let mut options = RunOptions::new(dir.path());
    options.run_key = Some(key.clone());
    let rt = Runtime::standard().store(memory.clone()).options(options);
    let mut coordinator = Coordinator::create(
        rt.prepare_run(dir.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("creates");
    coordinator
        .register_graph(&two_steps())
        .await
        .expect("registers");
    drop(coordinator);

    let resumed = host::resume(&rt).await;
    assert!(
        matches!(resumed, Err(host::HostError::NotStarted)),
        "nothing to resume: {:?}",
        resumed.map(|report| report.status)
    );
    let report = host::run(&rt, two_steps())
        .await
        .expect("starts again under its key");
    assert_eq!(report.status, RunStatus::Success);

    let logs = memory.open(&key, Access::Read).await.expect("reads");
    let records = read_coordinator_log(&*logs).await.expect("decodes");
    let count = |matches: fn(&CoordinatorEvent) -> bool| {
        records
            .iter()
            .filter(|record| matches(&record.body))
            .count()
    };
    assert_eq!(
        count(|event| matches!(event, CoordinatorEvent::RunStarted { .. })),
        1
    );
    assert_eq!(
        count(
            |event| matches!(event, CoordinatorEvent::InvocationDeclared { invocation, .. } if *invocation == InvocationId::ROOT)
        ),
        1
    );
    assert_eq!(
        count(|event| matches!(event, CoordinatorEvent::RunFinished { .. })),
        1
    );

    let again = host::run(&rt, two_steps()).await;
    assert!(
        matches!(
            again,
            Err(host::HostError::Coordinator(CoordinatorError::Open(
                store::StoreError::Exists { .. }
            )))
        ),
        "a run that started is not started again: {:?}",
        again.map(|report| report.status)
    );
}

/// A crash before `run.started` itself: the key is stored with an empty
/// log. A resume refuses it as never started, and a start under the key
/// takes it over.
#[tokio::test]
async fn a_run_cut_short_before_its_start_record_is_started_again() {
    let key = RunKey::new("cut-before-start");
    let dir = RunDir::new("store-cut-before-start");
    let memory = Arc::new(MemoryRunStore::new());
    drop(
        memory
            .open(&key, Access::Create {
                owner: OwnerId::new("crashed"),
            })
            .await
            .expect("creates the key"),
    );
    let mut options = RunOptions::new(dir.path());
    options.run_key = Some(key.clone());
    let rt = Runtime::standard().store(memory.clone()).options(options);

    let resumed = host::resume(&rt).await;
    assert!(
        matches!(resumed, Err(host::HostError::NotStarted)),
        "nothing to resume: {:?}",
        resumed.map(|report| report.status)
    );
    let report = host::run(&rt, two_steps())
        .await
        .expect("starts under its key");
    assert_eq!(report.status, RunStatus::Success);
}
