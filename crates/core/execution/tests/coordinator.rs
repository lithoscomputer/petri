use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use driver::lifecycle::{ExecutionHooks, HookContext, Note, RunFinished};
use execution::{
    Access, CallSite, Coordinator, CoordinatorEvent, CoordinatorInvocationClient,
    CoordinatorOptions, CoordinatorStore, ExecutionId, FoldEvent, GraphDigest,
    InvocationClient as _, InvocationId, InvocationRequest, Middleware, MiddlewareError, OwnerId,
    RouteCall, RouteNext, RunDirStore, RunStore as _, SandboxBinding, SandboxMode, SecretBindings,
    StoredEngineRecord, read_coordinator_log,
};
use executor::Retention;
use ir::{
    EdgeTransition, GraphBuilder, Outcome, ResultProjection, RunStatus, Scope, ScopeId, Status,
    StepRef,
};
use runtime::engine::{
    DEFAULT_MAX_EXECUTIONS, EngineExit, EngineStart, EntryPoint, Event, MiddlewareKey,
    RouteDecision,
};
use runtime::steps::{Step, StepCtx};
use runtime::store::RunKey;
use runtime::{RunOptions, Runtime};
use serde::Deserialize;
use testkit::RunDir;
use tokio::sync::{Barrier, Notify};
use tokio::time::timeout;

#[tokio::test]
async fn one_invocation_and_execution_use_the_coordinator_layout() {
    let directory = RunDir::new("coordinator-simple");
    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let run_runtime = runtime.prepare_run(directory.path());
    let mut coordinator =
        Coordinator::create(run_runtime, Vec::new(), CoordinatorOptions::default())
            .await
            .expect("the coordinator starts");

    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    builder.add_step("only", scope, "noop");
    let graph = builder.build();
    let digest = coordinator
        .register_graph(&graph)
        .await
        .expect("graph registers");
    let result = coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("the root invocation runs");

    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(result.final_execution.raw(), 0);
    assert!(directory.path().join("run.json").is_file());
    assert!(directory.path().join("coordinator.jsonl").is_file());
    assert!(
        directory
            .path()
            .join("graphs")
            .join(format!("{digest}.json"))
            .is_file()
    );
    assert!(
        directory
            .path()
            .join("executions/0000000000000000/events.jsonl")
            .is_file()
    );
    assert_eq!(coordinator.store().state().root, Some(InvocationId::ROOT));
    coordinator.finish().await;
}

#[tokio::test]
async fn a_declared_execution_with_an_empty_log_starts_from_its_declaration() {
    let directory = RunDir::new("coordinator-empty-execution");
    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    builder.add_step("only", scope, "noop");
    let graph = builder.build();
    let context = BTreeMap::from([("input".into(), serde_json::json!("kept"))]);

    let digest = {
        let key = RunKey::new("test");
        let logs = RunDirStore::new(directory.path())
            .open(&key, Access::Create {
                owner: OwnerId::mint(),
            })
            .await
            .expect("the run is created");
        let mut store = CoordinatorStore::create(logs, key, Vec::new())
            .await
            .expect("store");
        let (digest, _) = store.register_graph(&graph).await.expect("graph registers");
        store
            .append(CoordinatorEvent::InvocationDeclared {
                invocation:      InvocationId::ROOT,
                call:            None,
                graph:           digest,
                context:         context.clone(),
                secret_bindings: SecretBindings::None,
                sandbox:         SandboxBinding::Isolated,
                admission:       None,
            })
            .await
            .expect("invocation declaration persists");
        store
            .append(CoordinatorEvent::ExecutionDeclared {
                execution:        ExecutionId::new(0),
                invocation:       InvocationId::ROOT,
                predecessor:      None,
                start:            EngineStart {
                    entry:           EntryPoint::GraphEntries,
                    context:         context.clone(),
                    prior_firings:   BTreeMap::new(),
                    execution_index: 0,
                    max_executions:  DEFAULT_MAX_EXECUTIONS,
                },
                middleware_state: BTreeMap::new(),
            })
            .await
            .expect("execution declaration persists");
        digest
    };

    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator resumes");
    let result = coordinator
        .run_root(digest, context)
        .await
        .expect("the declared execution starts");

    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(result.context["input"], serde_json::json!("kept"));
    coordinator.finish().await;
}

#[tokio::test]
async fn resume_folds_a_final_outcome_before_reissuing_pending_routing() {
    let directory = RunDir::new("coordinator-pending-routing-fold");
    let middleware: Vec<Arc<dyn Middleware>> = vec![Arc::new(RequireFoldBeforeRoute)];
    // This fixture truncates a completed run's logs to model a crash. Keep
    // its workspace so the fixture does not also model confirmed deletion.
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::Always;
    let runtime = Runtime::standard().options(options);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        middleware.clone(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");

    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    let first = builder.add_step("first", scope, "noop");
    let second = builder.add_step("second", scope, "noop");
    builder.link(first, second);
    let digest = coordinator
        .register_graph(&builder.build())
        .await
        .expect("graph registers");
    let result = coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("the first run completes");
    assert_eq!(result.status, RunStatus::Success);
    drop(coordinator);

    let coordinator_path = directory.path().join("coordinator.jsonl");
    let decoded = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("coordinator log decodes");
    let mut coordinator_prefix = Vec::new();
    for record in decoded {
        let declared = matches!(record.body, CoordinatorEvent::ExecutionDeclared { .. });
        serde_json::to_writer(&mut coordinator_prefix, &record).expect("record encodes");
        coordinator_prefix.push(b'\n');
        if declared {
            break;
        }
    }
    fs::write(&coordinator_path, coordinator_prefix).expect("coordinator prefix");

    let events_path = directory
        .path()
        .join("executions/0000000000000000/events.jsonl");
    let events = fs::read_to_string(&events_path).expect("engine log");
    let mut lines = events.lines();
    let mut engine_prefix = format!("{}\n", lines.next().expect("engine header"));
    for line in lines {
        let record: StoredEngineRecord = serde_json::from_str(line).expect("engine record");
        engine_prefix.push_str(line);
        engine_prefix.push('\n');
        if matches!(record.body, Event::StepFinished { .. }) {
            break;
        }
    }
    fs::write(&events_path, engine_prefix).expect("engine prefix");

    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        middleware,
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator resumes");
    let result = coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("pending routing resumes");

    assert_eq!(result.status, RunStatus::Success);
    coordinator.finish().await;
}

/// A crash after the root's result is stored but before the run's end is:
/// the resumed run replays the finished root and still ends the run, once.
#[tokio::test]
async fn a_crash_between_the_roots_end_and_the_runs_end_still_ends_the_run() {
    let directory = RunDir::new("coordinator-run-finished-on-resume");
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::Always;
    let runtime = Runtime::standard().options(options);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");
    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    builder.add_step("only", scope, "noop");
    let digest = coordinator
        .register_graph(&builder.build())
        .await
        .expect("graph registers");
    coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("the first run completes");
    drop(coordinator);

    // The crash: the log ends at the root's result.
    let decoded = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("coordinator log decodes");
    let mut prefix = Vec::new();
    for record in decoded {
        let root_finished = matches!(
            record.body,
            CoordinatorEvent::InvocationFinished { invocation, .. } if invocation == InvocationId::ROOT
        );
        serde_json::to_writer(&mut prefix, &record).expect("record encodes");
        prefix.push(b'\n');
        if root_finished {
            break;
        }
    }
    fs::write(directory.path().join("coordinator.jsonl"), prefix).expect("coordinator prefix");

    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator resumes");
    let result = coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("the finished root replays");
    let ends: Vec<RunStatus> = read_coordinator_log(&**coordinator.store().logs())
        .await
        .expect("coordinator log decodes")
        .into_iter()
        .filter_map(|record| match record.body {
            CoordinatorEvent::RunFinished { status, .. } => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(
        ends,
        vec![result.status],
        "the run ends once, as its root did"
    );
    // The crash cut off the root's `scope.released` too; its lease was
    // settled, and the resumed run records that before the run's end.
    assert_release_before_end(&coordinator).await;
    coordinator.finish().await;
}

/// The log's one `scope.released`, then `run.finished` as its last record.
async fn assert_release_before_end(coordinator: &Coordinator) {
    let kinds: Vec<&str> = read_coordinator_log(&**coordinator.store().logs())
        .await
        .expect("coordinator log decodes")
        .into_iter()
        .filter_map(|record| match record.body {
            CoordinatorEvent::ScopeReleased { .. } => Some("scope.released"),
            CoordinatorEvent::RunFinished { .. } => Some("run.finished"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["scope.released", "run.finished"]);
}

/// A crash after the root's result is stored but before its lease was
/// released: the resumed run releases it at the run's end, and records the
/// release before `run.finished`, which stays the last record.
#[tokio::test]
async fn a_crash_before_the_roots_release_still_records_it_before_the_runs_end() {
    let directory = RunDir::new("coordinator-release-on-resume");
    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");
    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    builder.add_step("only", scope, "noop");
    let digest = coordinator
        .register_graph(&builder.build())
        .await
        .expect("graph registers");
    coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("the first run completes");
    drop(coordinator);

    // The crash: the coordinator log ends at the root's result, and the
    // resource log at the lease's sandbox going live.
    let decoded = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("coordinator log decodes");
    let mut prefix = Vec::new();
    for record in decoded {
        let root_finished = matches!(
            record.body,
            CoordinatorEvent::InvocationFinished { invocation, .. } if invocation == InvocationId::ROOT
        );
        serde_json::to_writer(&mut prefix, &record).expect("record encodes");
        prefix.push(b'\n');
        if root_finished {
            break;
        }
    }
    fs::write(directory.path().join("coordinator.jsonl"), prefix).expect("coordinator prefix");
    let resources = fs::read_to_string(directory.path().join("resources.jsonl"))
        .expect("the resource log reads");
    let mut kept = String::new();
    for line in resources.lines() {
        kept.push_str(line);
        kept.push('\n');
        let record: serde_json::Value = serde_json::from_str(line).expect("a record parses");
        if record["body"]["state"] == "live" {
            break;
        }
    }
    fs::write(directory.path().join("resources.jsonl"), kept).expect("resource prefix");

    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator resumes");
    coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("the finished root replays");
    assert_release_before_end(&coordinator).await;
    coordinator.finish().await;
}

#[derive(Deserialize)]
struct InvokeConfig {
    graph:              GraphDigest,
    #[serde(default)]
    inherit:            bool,
    #[serde(default)]
    detach_after_start: bool,
}

struct InvokeStep;

#[async_trait::async_trait]
impl Step for InvokeStep {
    const NAME: &'static str = "test/invoke";
    type Config = InvokeConfig;

    async fn run(&self, config: InvokeConfig, ctx: StepCtx) -> Outcome {
        let client = match ctx.require_capability::<CoordinatorInvocationClient>() {
            Ok(client) => client,
            Err(error) => return error.into(),
        };
        let mut handle = match client
            .start_or_attach(InvocationRequest {
                site:      CallSite {
                    firing:  ctx.firing,
                    attempt: ctx.attempt,
                    slot:    "child".into(),
                },
                graph:     config.graph,
                context:   BTreeMap::new(),
                secrets:   SecretBindings::None,
                sandbox:   if config.inherit {
                    SandboxMode::Inherit { scope: ctx.scope }
                } else {
                    SandboxMode::Isolated
                },
                admission: None,
            })
            .await
        {
            Ok(handle) => handle,
            Err(error) => return Outcome::failure(error.to_string()),
        };
        if config.detach_after_start {
            let started = match ctx.require_capability::<TestStarted>() {
                Ok(started) => started,
                Err(error) => return error.into(),
            };
            started.0.notified().await;
            return Outcome::success(serde_json::Value::Null);
        }
        let result = handle.result().await;
        match result.status {
            RunStatus::Success => Outcome::success(result.output),
            RunStatus::Failed => Outcome::new(
                Status::Failure(
                    result
                        .failure
                        .unwrap_or_else(|| ir::FailureInfo::new("nested invocation failed")),
                ),
                result.output,
            ),
            RunStatus::Cancelled => Outcome::cancelled(),
        }
    }
}

#[derive(Clone)]
struct TestBarrier(Arc<Barrier>);

struct BarrierStep;

#[async_trait::async_trait]
impl Step for BarrierStep {
    const NAME: &'static str = "test/barrier";
    type Config = ();

    async fn run(&self, (): (), ctx: StepCtx) -> Outcome {
        let barrier = match ctx.require_capability::<TestBarrier>() {
            Ok(barrier) => barrier,
            Err(error) => return error.into(),
        };
        barrier.0.wait().await;
        Outcome::success(serde_json::Value::Null)
    }
}

#[derive(Default)]
struct SiblingOrder {
    first_started:  Notify,
    second_started: Notify,
    release_second: Notify,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SiblingPhase {
    First,
    Second,
    AwaitFirst,
    ReleaseSecond,
}

struct OrderedSiblingStep;

#[async_trait::async_trait]
impl Step for OrderedSiblingStep {
    const NAME: &'static str = "test/ordered-sibling";
    type Config = SiblingPhase;

    async fn run(&self, phase: SiblingPhase, mut ctx: StepCtx) -> Outcome {
        let order = match ctx.require_capability::<Arc<SiblingOrder>>() {
            Ok(order) => order,
            Err(error) => return error.into(),
        };
        match phase {
            SiblingPhase::First => {
                order.first_started.notify_one();
                order.second_started.notified().await;
            }
            SiblingPhase::Second => {
                order.second_started.notify_one();
                tokio::select! {
                    () = order.release_second.notified() => {}
                    _ = ctx.control.recv() => return Outcome::cancelled(),
                }
            }
            SiblingPhase::AwaitFirst => order.first_started.notified().await,
            SiblingPhase::ReleaseSecond => order.release_second.notify_one(),
        }
        Outcome::success(serde_json::Value::Null)
    }
}

#[derive(Clone)]
struct TestStarted(Arc<Notify>);

struct WaitForCancelStep;

#[async_trait::async_trait]
impl Step for WaitForCancelStep {
    const NAME: &'static str = "test/wait-for-cancel";
    type Config = ();

    async fn run(&self, (): (), mut ctx: StepCtx) -> Outcome {
        let started = match ctx.require_capability::<TestStarted>() {
            Ok(started) => started,
            Err(error) => return error.into(),
        };
        started.0.notify_one();
        match ctx.control.recv().await {
            Some(ir::Control::Cancel | ir::Control::Kill) | None => Outcome::cancelled(),
            Some(ir::Control::Deliver(_)) => Outcome::failure("unexpected control"),
            Some(_) => Outcome::failure("unknown control"),
        }
    }
}

struct RequireFoldBeforeRoute;

#[async_trait::async_trait]
impl Middleware for RequireFoldBeforeRoute {
    fn key(&self) -> MiddlewareKey {
        MiddlewareKey::new("require-final-outcome-fold")
    }

    fn state_version(&self) -> u32 {
        1
    }

    fn initial_state(&self) -> serde_json::Value {
        serde_json::json!(0)
    }

    fn fold(
        &self,
        state: &mut serde_json::Value,
        event: &FoldEvent<'_>,
    ) -> Result<(), MiddlewareError> {
        if matches!(event, FoldEvent::FinalOutcome { .. }) {
            *state = serde_json::json!(state.as_u64().unwrap_or(0) + 1);
        }
        Ok(())
    }

    async fn route(
        &self,
        call: RouteCall,
        next: RouteNext<'_>,
    ) -> Result<RouteDecision, MiddlewareError> {
        if call.state == serde_json::json!(1) {
            next.run().await
        } else {
            Ok(RouteDecision::Block {
                reason: "final outcome was not folded before routing".into(),
            })
        }
    }
}

#[tokio::test]
async fn a_step_can_run_a_registered_nested_invocation() {
    let directory = RunDir::new("coordinator-nested");
    let runtime = Runtime::standard()
        .step(InvokeStep)
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");

    let mut child = GraphBuilder::bare();
    let child_scope = child.add_scope(Scope::new(ScopeId::new(0)));
    let result_node = child.add_node(
        "result",
        child_scope,
        StepRef::new("noop", serde_json::json!({ "from": "child" })),
    );
    child.graph_mut().result = ResultProjection::NodeOutput(result_node);
    let child = child.build();
    let child_digest = coordinator
        .register_graph(&child)
        .await
        .expect("child registers");

    let mut parent = GraphBuilder::bare();
    let parent_scope = parent.add_scope(Scope::new(ScopeId::new(0)));
    parent.add_node(
        "invoke",
        parent_scope,
        StepRef::new(
            "test/invoke",
            serde_json::json!({ "graph": child_digest, "inherit": true }),
        ),
    );
    let parent = parent.build();
    let parent_digest = coordinator
        .register_graph(&parent)
        .await
        .expect("parent registers");

    let result = coordinator
        .run_root(parent_digest, BTreeMap::new())
        .await
        .expect("the invocation tree runs");

    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(coordinator.store().state().invocations.len(), 2);
    let child = &coordinator.store().state().invocations[&InvocationId::new(1)];
    assert_eq!(child.declaration.graph, child_digest);
    assert!(matches!(
        child.declaration.sandbox,
        execution::SandboxBinding::Inherited { .. }
    ));
    assert_eq!(
        child.result.as_ref().expect("child result").output,
        serde_json::json!({ "from": "child" })
    );
    coordinator.finish().await;
}

#[tokio::test]
async fn an_inherited_child_cannot_declare_a_different_container() {
    let directory = RunDir::new("coordinator-inherited-container-mismatch");
    let runtime = Runtime::standard()
        .step(InvokeStep)
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    let mut child = GraphBuilder::bare();
    let mut child_scope = Scope::new(ScopeId::new(0));
    child_scope.runtime = ir::RuntimeSpec::container("alpine:3.20");
    let child_scope = child.add_scope(child_scope);
    child.add_step("must-not-run", child_scope, "noop");
    let child = coordinator.register_graph(&child.build()).await.unwrap();
    let mut parent = GraphBuilder::new();
    parent.add_node(
        "invoke",
        ScopeId::new(0),
        StepRef::new(
            InvokeStep::NAME,
            serde_json::json!({"graph": child, "inherit": true}),
        ),
    );
    let parent = coordinator.register_graph(&parent.build()).await.unwrap();
    let result = coordinator.run_root(parent, BTreeMap::new()).await.unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(
        coordinator.store().state().invocations.len(),
        1,
        "the invalid child is refused before declaration"
    );
    let events = fs::read_to_string(
        directory
            .path()
            .join("executions/0000000000000000/events.jsonl"),
    )
    .unwrap();
    assert!(events.contains("declares a different container"));
    coordinator.finish().await;
}

#[tokio::test]
async fn sibling_nested_invocations_run_in_parallel() {
    let directory = RunDir::new("coordinator-parallel-nested");
    let runtime = Runtime::standard()
        .step(InvokeStep)
        .step(BarrierStep)
        .capability(TestBarrier(Arc::new(Barrier::new(2))))
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");

    let mut child = GraphBuilder::bare();
    let child_scope = child.add_scope(Scope::new(ScopeId::new(0)));
    child.add_step("barrier", child_scope, "test/barrier");
    let child_digest = coordinator
        .register_graph(&child.build())
        .await
        .expect("child registers");

    let mut parent = GraphBuilder::bare();
    let parent_scope = parent.add_scope(Scope::new(ScopeId::new(0)));
    for name in ["left", "right"] {
        parent.add_node(
            name,
            parent_scope,
            StepRef::new("test/invoke", serde_json::json!({ "graph": child_digest })),
        );
    }
    let parent_digest = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent registers");

    let result = timeout(
        Duration::from_secs(5),
        coordinator.run_root(parent_digest, BTreeMap::new()),
    )
    .await
    .expect("the sibling invocations reached the barrier together")
    .expect("the invocation tree runs");

    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(coordinator.store().state().invocations.len(), 3);
    coordinator.finish().await;
}

#[tokio::test]
async fn a_sibling_result_reaches_the_parent_while_another_sibling_is_running() {
    let directory = RunDir::new("coordinator-sibling-results");
    let runtime = Runtime::standard()
        .step(InvokeStep)
        .step(OrderedSiblingStep)
        .capability(Arc::new(SiblingOrder::default()))
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");

    let mut children = Vec::new();
    for phase in ["first", "second"] {
        let mut child = GraphBuilder::new();
        child.add_node(
            phase,
            ScopeId::new(0),
            StepRef::new(OrderedSiblingStep::NAME, serde_json::json!(phase)),
        );
        children.push(
            coordinator
                .register_graph(&child.build())
                .await
                .expect("child registers"),
        );
    }
    let mut parent = GraphBuilder::new();
    let first = parent.add_node(
        "first",
        ScopeId::new(0),
        StepRef::new(InvokeStep::NAME, serde_json::json!({"graph": children[0]})),
    );
    let await_first = parent.add_node(
        "await-first",
        ScopeId::new(0),
        StepRef::new(OrderedSiblingStep::NAME, serde_json::json!("await_first")),
    );
    let second = parent.add_node(
        "second",
        ScopeId::new(0),
        StepRef::new(InvokeStep::NAME, serde_json::json!({"graph": children[1]})),
    );
    let release_second = parent.add_node(
        "release-second",
        ScopeId::new(0),
        StepRef::new(
            OrderedSiblingStep::NAME,
            serde_json::json!("release_second"),
        ),
    );
    // The second call begins inside the first child's execution. It can only
    // finish after the first result reaches the parent and releases it.
    parent.link(await_first, second);
    parent.link(first, release_second);
    let parent = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent registers");
    let result = timeout(
        Duration::from_secs(5),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the parent receives the first result while the second child waits")
    .expect("the invocation tree runs");

    assert_eq!(result.status, RunStatus::Success);
    let invocations = &coordinator.store().state().invocations;
    assert_eq!(invocations.len(), 3);
    for invocation in invocations.values() {
        assert!(
            !invocation.cancelled,
            "sibling completion is not cancellation"
        );
        assert_eq!(
            invocation.result.as_ref().expect("finished").status,
            RunStatus::Success
        );
    }
    coordinator.finish().await;
}

#[tokio::test]
async fn cancelling_the_root_cancels_an_active_nested_invocation() {
    let directory = RunDir::new("coordinator-cancel-nested");
    let started = Arc::new(Notify::new());
    let runtime = Runtime::standard()
        .step(InvokeStep)
        .step(WaitForCancelStep)
        .capability(TestStarted(started.clone()))
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");

    let mut child = GraphBuilder::bare();
    let child_scope = child.add_scope(Scope::new(ScopeId::new(0)));
    child.add_step("wait", child_scope, "test/wait-for-cancel");
    let child_digest = coordinator
        .register_graph(&child.build())
        .await
        .expect("child registers");

    let mut parent = GraphBuilder::bare();
    let parent_scope = parent.add_scope(Scope::new(ScopeId::new(0)));
    parent.add_node(
        "invoke",
        parent_scope,
        StepRef::new("test/invoke", serde_json::json!({ "graph": child_digest })),
    );
    let parent_digest = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent registers");
    let control = coordinator.handle();

    let mut running = Box::pin(coordinator.run_root(parent_digest, BTreeMap::new()));
    tokio::select! {
        () = started.notified() => control.cancel_root(),
        result = &mut running => panic!("the run finished before cancellation: {result:?}"),
    }
    let result = running.await.expect("the cancelled tree settles");

    assert_eq!(result.status, RunStatus::Cancelled);
    assert!(
        coordinator
            .store()
            .state()
            .invocations
            .values()
            .all(|invocation| invocation.cancelled)
    );
    coordinator.finish().await;
}

#[tokio::test]
async fn completing_a_parent_cancels_each_descendant_once() {
    let directory = RunDir::new("coordinator-descendant-cancel");
    let runtime = Runtime::standard()
        .step(InvokeStep)
        .step(WaitForCancelStep)
        .capability(TestStarted(Arc::new(Notify::new())))
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");
    let mut grandchild = GraphBuilder::new();
    grandchild.add_step("wait", ScopeId::new(0), WaitForCancelStep::NAME);
    let grandchild = coordinator
        .register_graph(&grandchild.build())
        .await
        .expect("grandchild registers");
    let mut child = GraphBuilder::new();
    child.add_node(
        "invoke",
        ScopeId::new(0),
        StepRef::new(InvokeStep::NAME, serde_json::json!({"graph": grandchild})),
    );
    let child = coordinator
        .register_graph(&child.build())
        .await
        .expect("child registers");
    let mut parent = GraphBuilder::new();
    parent.add_node(
        "detach",
        ScopeId::new(0),
        StepRef::new(
            InvokeStep::NAME,
            serde_json::json!({"graph": child, "detach_after_start": true}),
        ),
    );
    let parent = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent registers");
    timeout(
        Duration::from_secs(5),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the descendants settle")
    .expect("the run succeeds");
    for id in [1, 2] {
        let invocation = &coordinator.store().state().invocations[&InvocationId::new(id)];
        let execution = invocation
            .result
            .as_ref()
            .expect("descendant settled")
            .final_execution;
        let path = coordinator.execution_dir(execution).join("events.jsonl");
        let log = execution::read_engine_log(&path)
            .expect("descendant engine log")
            .log;
        assert_eq!(
            log.events()
                .filter(|event| matches!(
                    event, Event::CancelRequested { target: engine::CancelTarget::Scope(scope) } if *scope == ir::CancelScopeId::ROOT
                ))
                .count(),
            1,
            "each descendant receives one polite cancellation"
        );
        assert!(
            !log.events()
                .any(|event| matches!(event, Event::KillRequested { .. })),
            "finishing ancestors must not escalate cancellation"
        );
    }
    coordinator.finish().await;
}

#[tokio::test]
async fn a_terminal_parent_replay_settles_its_unfinished_declared_child() {
    assert_terminal_parent_recovers_child(false).await;
}

#[tokio::test]
async fn recovering_a_cancelled_child_does_not_escalate_its_cancellation() {
    assert_terminal_parent_recovers_child(true).await;
}

async fn assert_terminal_parent_recovers_child(cancel_recorded: bool) {
    let directory = RunDir::new("coordinator-terminal-parent-recovery");
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::Always;
    let runtime = Runtime::standard()
        .step(InvokeStep)
        .step(WaitForCancelStep)
        .capability(TestStarted(Arc::new(Notify::new())))
        .options(options);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");
    let mut child = GraphBuilder::new();
    child.add_step("wait", ScopeId::new(0), WaitForCancelStep::NAME);
    let child = coordinator
        .register_graph(&child.build())
        .await
        .expect("child registers");
    let mut parent = GraphBuilder::new();
    parent.add_node(
        "detach",
        ScopeId::new(0),
        StepRef::new(
            InvokeStep::NAME,
            serde_json::json!({"graph": child, "detach_after_start": true}),
        ),
    );
    let parent = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent registers");
    timeout(
        Duration::from_secs(5),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the parent settles its child")
    .expect("the initial run succeeds");
    coordinator.finish().await;

    // Model a crash after the parent driver finished, before its coordinator
    // cancelled the child. The terminal parent log cannot reissue the call.
    let lifecycle_path = directory.path().join("coordinator.jsonl");
    let lifecycle = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("coordinator log decodes");
    let mut prefix = Vec::new();
    for record in lifecycle {
        let cancellation = matches!(
            record.body,
            CoordinatorEvent::InvocationCancelRequested { .. }
        );
        if cancellation && !cancel_recorded {
            break;
        }
        serde_json::to_writer(&mut prefix, &record).expect("record encodes");
        prefix.push(b'\n');
        if cancellation {
            break;
        }
    }
    fs::write(&lifecycle_path, prefix).expect("coordinator prefix");
    let child_events = directory
        .path()
        .join("executions/0000000000000001/events.jsonl");
    let events = fs::read_to_string(&child_events).expect("child engine log");
    let mut lines = events.lines();
    let mut prefix = format!("{}\n", lines.next().expect("engine header"));
    for line in lines {
        let record: StoredEngineRecord = serde_json::from_str(line).expect("engine record");
        let cancellation = matches!(record.body, Event::CancelRequested { .. });
        if cancellation && !cancel_recorded {
            break;
        }
        prefix.push_str(line);
        prefix.push('\n');
        if cancellation {
            break;
        }
    }
    fs::write(&child_events, prefix).expect("child engine prefix");

    let mut coordinator = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator resumes");
    let result = timeout(
        Duration::from_secs(5),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the terminal replay settles its durable child")
    .expect("the replay succeeds");
    assert_eq!(result.status, RunStatus::Success);
    let child = &coordinator.store().state().invocations[&InvocationId::new(1)];
    assert!(child.cancelled);
    assert_eq!(
        child.result.as_ref().expect("child settled").status,
        RunStatus::Cancelled
    );
    let log = execution::read_engine_log(&child_events)
        .expect("recovered child log")
        .log;
    assert_eq!(
        log.events()
            .filter(|event| matches!(event, Event::CancelRequested { .. }))
            .count(),
        1
    );
    assert!(
        !log.events()
            .any(|event| matches!(event, Event::KillRequested { .. })),
        "recovery preserves polite cancellation"
    );
    let report = coordinator.take_root_report().expect("root report");
    assert_eq!(
        report.state.history().len(),
        1,
        "the parent call was not reissued"
    );
    coordinator.finish().await;
}

/// Notes the run's end, so a record can land between a cancel and its
/// cascade.
struct NotingHooks;

#[async_trait::async_trait]
impl ExecutionHooks for NotingHooks {
    async fn run_finished(&self, _context: &HookContext, _finished: RunFinished) -> Vec<Note> {
        vec![Note::new("test.run", serde_json::json!({}))]
    }
}

/// A crash can cut a cancel's cascade short: the root's cancel is
/// recorded, its child's is not. The resumed coordinator records the
/// child's cancel before anything else, so the root cannot reach its end,
/// and run its end hooks, while its child is still uncancelled.
#[tokio::test]
async fn a_resumed_run_finishes_a_cancel_a_crash_cut_short() {
    let directory = RunDir::new("coordinator-cut-cancel");
    let started = Arc::new(Notify::new());
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::Always;
    let runtime = Runtime::standard()
        .step(InvokeStep)
        .step(WaitForCancelStep)
        .capability(TestStarted(started.clone()))
        .hooks(Arc::new(NotingHooks))
        .options(options);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");
    let mut child = GraphBuilder::new();
    child.add_step("wait", ScopeId::new(0), WaitForCancelStep::NAME);
    let child = coordinator
        .register_graph(&child.build())
        .await
        .expect("child registers");
    let mut parent = GraphBuilder::new();
    parent.add_node(
        "invoke",
        ScopeId::new(0),
        StepRef::new(InvokeStep::NAME, serde_json::json!({ "graph": child })),
    );
    let parent = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent registers");
    let control = coordinator.handle();
    let mut running = Box::pin(coordinator.run_root(parent, BTreeMap::new()));
    tokio::select! {
        () = started.notified() => control.cancel_root(),
        result = &mut running => panic!("the run finished before cancellation: {result:?}"),
    }
    let result = timeout(Duration::from_secs(5), running)
        .await
        .expect("the cancelled tree settles")
        .expect("the run ends");
    assert_eq!(result.status, RunStatus::Cancelled);
    coordinator.finish().await;

    // Model a crash between the root's cancel and its child's: the
    // coordinator log ends with the root's cancel, and neither engine log
    // holds the cancel yet.
    let lifecycle = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("coordinator log decodes");
    let mut prefix = Vec::new();
    let mut kept = 0;
    for record in lifecycle {
        serde_json::to_writer(&mut prefix, &record).expect("record encodes");
        prefix.push(b'\n');
        kept += 1;
        if matches!(
            record.body,
            CoordinatorEvent::InvocationCancelRequested { invocation, .. }
                if invocation == InvocationId::ROOT
        ) {
            break;
        }
    }
    fs::write(directory.path().join("coordinator.jsonl"), prefix).expect("coordinator prefix");
    for execution in ["0000000000000000", "0000000000000001"] {
        let path = directory
            .path()
            .join(format!("executions/{execution}/events.jsonl"));
        let events = fs::read_to_string(&path).expect("engine log");
        let mut lines = events.lines();
        let mut prefix = format!("{}\n", lines.next().expect("engine header"));
        for line in lines {
            let record: StoredEngineRecord = serde_json::from_str(line).expect("engine record");
            if matches!(record.body, Event::CancelRequested { .. }) {
                break;
            }
            prefix.push_str(line);
            prefix.push('\n');
        }
        fs::write(&path, prefix).expect("engine prefix");
    }

    let mut coordinator = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator resumes");
    let result = timeout(
        Duration::from_secs(5),
        coordinator.run_root(parent, BTreeMap::new()),
    )
    .await
    .expect("the resumed tree settles")
    .expect("the resumed run ends");
    assert_eq!(result.status, RunStatus::Cancelled);
    coordinator.finish().await;

    let records = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("coordinator log decodes");
    let first = records.get(kept).map(|record| &record.body);
    assert!(
        matches!(
            first,
            Some(CoordinatorEvent::InvocationCancelRequested { invocation, .. })
                if *invocation == InvocationId::new(1)
        ),
        "the cut cascade is the resumed lifetime's first record: {first:?}"
    );
}

#[tokio::test]
async fn a_restart_declares_a_successor_in_the_same_invocation() {
    let directory = RunDir::new("coordinator-restart");
    let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");

    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    let start = builder.add_step("start", scope, "noop");
    let target = builder.add_step("target", scope, "noop");
    builder.mark_entry(start);
    builder.link(start, target);
    builder.node_mut(start).routing.groups[0].arms[0].transition = EdgeTransition::Restart;
    let graph = builder.build();
    let digest = coordinator
        .register_graph(&graph)
        .await
        .expect("graph registers");

    let result = coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("the successor runs");

    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(result.final_execution.raw(), 1);
    let root = &coordinator.store().state().invocations[&InvocationId::ROOT];
    assert_eq!(root.executions.len(), 2);
    assert!(matches!(
        coordinator.store().state().executions[&root.executions[0]].exit,
        Some(EngineExit::Restart { .. })
    ));
    coordinator.finish().await;
}
