//! The execution-scoped hook context: every `ExecutionHooks` callback is told
//! which run, invocation and execution it belongs to, so one hooks object
//! serves every execution of a run and a host still tells a child apart from
//! its sibling, and keys an external effect on an identity that survives a
//! crash.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use driver::lifecycle::{
    ExecutionHooks, HookContext, Note, RunFinished, ScopeReleased, Transition, TransitionError,
    TransitionReport,
};
use engine::{DecisionId, Event};
use execution::{
    CallSite, Coordinator, CoordinatorEvent, CoordinatorInvocationClient, CoordinatorOptions,
    EVENTS_FILE, ExecutionId, GraphDigest, InvocationClient as _, InvocationId, InvocationRequest,
    RunKey, SandboxMode, SecretBindings, StoredEngineRecord, execution_relative_dir,
    read_coordinator_log,
};
use executor::Retention;
use ir::{Attempt, GraphBuilder, Outcome, RunStatus, ScopeId, Status, StepRef};
use runtime::steps::{Step, StepCtx};
use runtime::{RunOptions, Runtime};
use serde::Deserialize;
use testkit::RunDir;

/// The effect kind the host produces at every transition.
const ROUTE_EFFECT: &str = "route";

/// One external effect's identity: the key a host deduplicates on.
type EffectKey = (RunKey, ExecutionId, DecisionId, &'static str);

/// A host adapter that performs one external effect per routing decision
/// and keeps what every callback was told. The effect set outlives a
/// coordinator, the way a host's durable record does.
#[derive(Default)]
struct RecordingHost {
    effects:     Mutex<BTreeSet<EffectKey>>,
    transitions: Mutex<Vec<(HookContext, DecisionId, String)>>,
    finished:    Mutex<Vec<HookContext>>,
    released:    Mutex<Vec<(HookContext, ScopeId)>>,
}

impl RecordingHost {
    fn effects(&self) -> BTreeSet<EffectKey> {
        lock(&self.effects).clone()
    }

    fn transitions(&self) -> Vec<(HookContext, DecisionId, String)> {
        lock(&self.transitions).clone()
    }

    fn finished(&self) -> Vec<HookContext> {
        lock(&self.finished).clone()
    }

    fn released(&self) -> Vec<(HookContext, ScopeId)> {
        lock(&self.released).clone()
    }

    /// Forget the callbacks seen so far; the effects stay.
    fn clear_calls(&self) {
        lock(&self.transitions).clear();
        lock(&self.finished).clear();
        lock(&self.released).clear();
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

#[async_trait::async_trait]
impl ExecutionHooks for RecordingHost {
    async fn transition(
        &self,
        context: &HookContext,
        transition: Transition,
    ) -> Result<TransitionReport, TransitionError> {
        lock(&self.transitions).push((
            context.clone(),
            transition.decision,
            transition.view.node_name().to_owned(),
        ));
        lock(&self.effects).insert((
            context.run_key.clone(),
            context.execution,
            transition.decision,
            ROUTE_EFFECT,
        ));
        Ok(TransitionReport::default())
    }

    async fn run_finished(&self, context: &HookContext, _finished: RunFinished) -> Vec<Note> {
        lock(&self.finished).push(context.clone());
        Vec::new()
    }

    async fn scope_released(&self, context: &HookContext, released: ScopeReleased) -> Vec<Note> {
        lock(&self.released).push((context.clone(), released.scope));
        Vec::new()
    }
}

#[derive(Deserialize)]
struct InvokeConfig {
    graph:   GraphDigest,
    #[serde(default)]
    inherit: bool,
}

/// Calls a registered graph as a nested invocation under the slot `child`.
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

fn runtime(directory: &RunDir, host: &Arc<RecordingHost>) -> Runtime {
    // A truncated log models a crash, not a confirmed deletion: keep the
    // workspaces so a resume finds them.
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::Always;
    Runtime::standard()
        .step(InvokeStep)
        .hooks(host.clone())
        .options(options)
}

/// A one-node child graph, registered with `coordinator`.
async fn register_child(coordinator: &mut Coordinator) -> GraphDigest {
    let mut child = GraphBuilder::new();
    child.add_step("work", ScopeId::new(0), "noop");
    coordinator
        .register_graph(&child.build())
        .await
        .expect("child registers")
}

fn invoke(child: GraphDigest, inherit: bool) -> StepRef {
    StepRef::new(
        InvokeStep::NAME,
        serde_json::json!({ "graph": child, "inherit": inherit }),
    )
}

#[tokio::test]
async fn sibling_children_with_the_same_local_ids_get_distinct_contexts() {
    let directory = RunDir::new("hook-context-siblings");
    let host = Arc::new(RecordingHost::default());
    let run = runtime(&directory, &host).prepare_run(directory.path());
    let key = run.run_key().clone();
    let mut coordinator = Coordinator::create(run, Vec::new(), CoordinatorOptions::default())
        .await
        .expect("the coordinator starts");

    let child = register_child(&mut coordinator).await;
    let mut parent = GraphBuilder::new();
    parent.add_node("left", ScopeId::new(0), invoke(child, false));
    parent.add_node("right", ScopeId::new(0), invoke(child, false));
    let parent = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent registers");
    let result = coordinator
        .run_root(parent, BTreeMap::new())
        .await
        .expect("the invocation tree runs");
    assert_eq!(result.status, RunStatus::Success);
    let root = result.final_execution;
    coordinator.finish().await;

    // The same workflow ran twice, so both children routed the same firing
    // under the same decision id. Only the context tells them apart.
    let children: Vec<_> = host
        .transitions()
        .into_iter()
        .filter(|(_, _, node)| node == "work")
        .collect();
    assert_eq!(children.len(), 2, "one routing decision per child");
    let (first, first_decision, _) = &children[0];
    let (second, second_decision, _) = &children[1];
    assert_eq!(first_decision, second_decision, "identical local ids");
    assert_ne!(first.execution, second.execution);
    assert_ne!(first.invocation, second.invocation);
    assert_ne!(first, second);
    let mut parents = Vec::new();
    for context in [first, second] {
        assert_eq!(context.run_key, key);
        let parent = context.parent.as_ref().expect("a child names its parent");
        assert_eq!(parent.execution, root);
        assert_eq!(parent.attempt, Attempt::FIRST);
        assert_eq!(parent.slot, "child");
        parents.push(parent.firing);
    }
    assert_ne!(
        parents[0], parents[1],
        "each child was called by its own firing"
    );

    // A host adapter keyed on the operation identity performs two effects.
    let child_effects = host
        .effects()
        .into_iter()
        .filter(|(_, execution, _, _)| *execution != root)
        .count();
    assert_eq!(child_effects, 2);
}

#[tokio::test]
async fn a_reissued_child_routing_decision_produces_one_effect() {
    let directory = RunDir::new("hook-context-reissue");
    let host = Arc::new(RecordingHost::default());
    let mut coordinator = Coordinator::create(
        runtime(&directory, &host).prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");

    let child = register_child(&mut coordinator).await;
    let mut parent = GraphBuilder::new();
    parent.add_node("invoke", ScopeId::new(0), invoke(child, true));
    let parent = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent registers");
    let result = coordinator
        .run_root(parent, BTreeMap::new())
        .await
        .expect("the invocation tree runs");
    assert_eq!(result.status, RunStatus::Success);
    let root = result.final_execution;
    drop(coordinator);

    let child_execution = ExecutionId::new(1);
    let effects_before = host.effects();
    assert_eq!(
        effects_before
            .iter()
            .filter(|(_, execution, _, _)| *execution == child_execution)
            .count(),
        1,
        "the child routed once"
    );

    // Model a crash while the child was routing: the coordinator log ends
    // with the child's declaration, the parent's log with its call still
    // running, and the child's log with its finish recorded and its routing
    // decision pending.
    truncate_coordinator_log(&directory, |body, declared| {
        if matches!(body, CoordinatorEvent::ExecutionDeclared { .. }) {
            *declared += 1;
        }
        *declared == 2
    })
    .await;
    truncate_engine_log(&directory, root, |body| {
        matches!(body, Event::StepStarted { .. })
    });
    truncate_engine_log(&directory, child_execution, |body| {
        matches!(body, Event::StepFinished { .. })
    });

    host.clear_calls();
    let mut coordinator = Coordinator::resume(
        runtime(&directory, &host).prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator resumes");
    let result = coordinator
        .run_root(parent, BTreeMap::new())
        .await
        .expect("the resumed tree runs");
    assert_eq!(result.status, RunStatus::Success);
    coordinator.finish().await;

    // The child's routing was asked for again, under the same context and
    // decision id, so the host's effect key matched and nothing new happened.
    let reissued: Vec<_> = host
        .transitions()
        .into_iter()
        .filter(|(context, _, _)| context.execution == child_execution)
        .collect();
    assert_eq!(
        reissued.len(),
        1,
        "the resume reissued the child's decision"
    );
    assert_eq!(
        host.effects(),
        effects_before,
        "the reissued decision produced no second effect"
    );
}

#[tokio::test]
async fn the_run_level_points_carry_the_owning_executions_context() {
    let directory = RunDir::new("hook-context-run-level");
    let host = Arc::new(RecordingHost::default());
    let run = runtime(&directory, &host).prepare_run(directory.path());
    let key = run.run_key().clone();
    let mut coordinator = Coordinator::create(run, Vec::new(), CoordinatorOptions::default())
        .await
        .expect("the coordinator starts");

    let child = register_child(&mut coordinator).await;
    let mut parent = GraphBuilder::new();
    parent.add_node("invoke", ScopeId::new(0), invoke(child, false));
    let parent = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent registers");
    let result = coordinator
        .run_root(parent, BTreeMap::new())
        .await
        .expect("the invocation tree runs");
    assert_eq!(result.status, RunStatus::Success);
    coordinator.finish().await;

    let root = HookContext::new(key, InvocationId::ROOT, result.final_execution);
    assert_eq!(
        host.finished(),
        vec![root.clone()],
        "the run owner reports the end"
    );

    let released = host.released();
    assert_eq!(
        released.len(),
        2,
        "the root's scope and the child's own scope"
    );
    assert!(released.iter().any(|(context, _)| *context == root));
    let child = released
        .iter()
        .find(|(context, _)| context.parent.is_some())
        .map(|(context, _)| context)
        .expect("the child's release names the child");
    assert_eq!(child.run_key, root.run_key);
    assert_eq!(child.invocation, InvocationId::new(1));
    assert_eq!(
        child.parent.as_ref().map(|parent| parent.execution),
        Some(root.execution)
    );
}

#[tokio::test]
async fn a_resumed_execution_runs_the_release_point_a_crash_cut_off() {
    let directory = RunDir::new("hook-context-cut-off-release");
    let host = Arc::new(RecordingHost::default());
    let run = runtime(&directory, &host).prepare_run(directory.path());
    let key = run.run_key().clone();
    let mut coordinator = Coordinator::create(run, Vec::new(), CoordinatorOptions::default())
        .await
        .expect("the coordinator starts");

    let mut graph = GraphBuilder::new();
    let second_scope = graph.add_scope(ir::Scope::new(ScopeId::new(0)));
    let first = graph.add_step("first", ScopeId::new(0), "noop");
    let second = graph.add_step("second", second_scope, "noop");
    graph.link(first, second);
    let graph = coordinator
        .register_graph(&graph.build())
        .await
        .expect("the graph registers");
    let result = coordinator
        .run_root(graph, BTreeMap::new())
        .await
        .expect("the run finishes");
    assert_eq!(result.status, RunStatus::Success);
    let root = result.final_execution;
    drop(coordinator);
    assert_eq!(host.released().len(), 2, "one release point per scope");

    // Model a crash while the second step ran: the first scope's point had
    // run, but the driver hands its notes over only at its end, so they
    // died with it. The resumed driver never holds the first scope.
    truncate_coordinator_log(&directory, |body, _| {
        matches!(body, CoordinatorEvent::ExecutionDeclared { .. })
    })
    .await;
    let started = Cell::new(0);
    truncate_engine_log(&directory, root, |body| {
        if matches!(body, Event::StepStarted { .. }) {
            started.set(started.get() + 1);
        }
        started.get() == 2
    });

    host.clear_calls();
    let mut coordinator = Coordinator::resume(
        runtime(&directory, &host).prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator resumes");
    let result = coordinator
        .run_root(graph, BTreeMap::new())
        .await
        .expect("the resumed run finishes");
    assert_eq!(result.status, RunStatus::Success);
    coordinator.finish().await;

    let context = HookContext::new(key, InvocationId::ROOT, root);
    let released: BTreeSet<ScopeId> = host
        .released()
        .into_iter()
        .map(|(released, scope)| {
            assert_eq!(released, context);
            scope
        })
        .collect();
    assert_eq!(
        released,
        BTreeSet::from([ScopeId::new(0), second_scope]),
        "the resumed driver ran the first scope's point again, and the second's as it released it"
    );
}

/// Cut the coordinator log after the first record `keep_through` accepts.
async fn truncate_coordinator_log(
    directory: &RunDir,
    mut keep_through: impl FnMut(&CoordinatorEvent, &mut u32) -> bool,
) {
    let decoded = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .expect("coordinator log decodes");
    let mut prefix = Vec::new();
    let mut declared = 0;
    for record in decoded {
        serde_json::to_writer(&mut prefix, &record).expect("record encodes");
        prefix.push(b'\n');
        if keep_through(&record.body, &mut declared) {
            break;
        }
    }
    fs::write(directory.path().join("coordinator.jsonl"), prefix).expect("coordinator prefix");
}

/// Cut an execution's engine log after the first record `last` accepts.
fn truncate_engine_log(directory: &RunDir, execution: ExecutionId, last: impl Fn(&Event) -> bool) {
    let path = directory
        .path()
        .join(execution_relative_dir(execution))
        .join(EVENTS_FILE);
    let events = fs::read_to_string(&path).expect("engine log");
    let mut lines = events.lines();
    let mut prefix = format!("{}\n", lines.next().expect("engine header"));
    let mut cut = false;
    for line in lines {
        let record: StoredEngineRecord = serde_json::from_str(line).expect("engine record");
        prefix.push_str(line);
        prefix.push('\n');
        if last(&record.body) {
            cut = true;
            break;
        }
    }
    assert!(cut, "execution {execution} has the record to cut after");
    fs::write(&path, prefix).expect("engine prefix");
}
