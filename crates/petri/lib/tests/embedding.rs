//! The embedding boundary, proven on a Fabro workflow: first with no adapters,
//! then with fake adapters that pause and release admission, skip and block
//! attempts, consume the public event stream, prepare results, override
//! routes, and delay or fail advancement. The fake host reconstructs the
//! run from public events alone.
//!
//! Every scenario runs through the standalone host (`petri::host`), so the
//! events it consumes are the ones an embedding host would consume, and the
//! run dir it leaves behind is the one `events::replay_run` rebuilds from.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::future::pending;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use petri::attractor::{
    AGENT_KIND, BranchStep, CommandStep, FanInStep, ForkStep, HumanStep, StageStep, StubStep,
    WAIT_KIND, WORKFLOW_KIND,
};
use petri::driver::lifecycle::{
    AdmitAttempt, AttemptDecision, ExecutionHooks, HookContext, Note, PrepareError, PrepareResult,
    Prepared, RESULT_PREPARED_KIND, Recorded, ResultAdjustment, RouteOverride, TRANSITION_KIND,
    Transition, TransitionError, TransitionReport,
};
use petri::driver::{BranchRole, FiringView};
use petri::engine::{Admission, DecisionId, Event, Intervention, RouteDecision};
use petri::execution::events::{
    CollectingSink, Derived, EventId, EventProjector, EventSource, Parsed, ProjectionReceipt,
    ProjectorOptions, RunEvent, RunEventSink, SinkError, ViewEvent, WaitState, replay_run_dir,
};
use petri::execution::hooks::{
    HOOK_NOTE_KIND, HookAdapter, HookDecision, HookPoint, HookReport, HookRequest, HookRun,
    HookService, HookServiceHandle,
};
use petri::execution::host::{self, HostRun};
use petri::execution::{
    CoordinatorEvent, ExecutionObserver, InterviewDispatcher, InterviewReply, InterviewRequest,
    Interviewer,
};
use petri::executor::Retention;
use petri::frontend::fabro::Fabro;
use petri::frontend::{CompileInputs, Lowered};
use petri::ir::{
    Attempt, EdgeId, Exhaustion, Graph, Outcome, RunStatus, Status, UnderlyingFailure, Value,
};
use petri::steps::Answer;
use petri::{RunOptions, Runtime, driver};
use serde_json::json;
use testkit::RunDir;
use tokio::sync::Notify;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;

// ── The workflow ───────────────────────────────────────────────────────────

/// A Fabro workflow with every baseline shape: a start, a command, an agent
/// that asks for a retry, a fan-out to two branches and a fan-in, a human
/// gate with two ways out, and an exit.
const WORKFLOW: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="echo prepared > prepared.txt; echo prepared"]
    flaky [shape=box, prompt="try", retry_policy="aggressive"]
    fan [shape=component]
    left [shape=parallelogram, script="echo left"]
    right [shape=parallelogram, script="echo right"]
    join [shape=tripleoctagon]
    gate [shape=hexagon, label="Ship?", question_type="yes_no"]
    ship [shape=parallelogram, script="echo shipped > shipped.txt; echo shipped"]
    hold [shape=parallelogram, script="echo held > held.txt; echo held"]
    start -> prepare
    prepare -> flaky
    flaky -> fan
    fan -> left
    fan -> right
    left -> join
    right -> join
    join -> gate
    gate -> ship [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    ship -> exit
    hold -> exit
}"#;

/// A node with an explicit recovery edge, for the completion preparation
/// scenarios: `work` is declared by the caller, and what the host is handed
/// as its final result decides between `recover` and `done`.
fn recovery_workflow(work: &str) -> String {
    format!(
        r#"digraph G {{
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [{work}]
    recover [shape=parallelogram, script="echo recovered"]
    done [shape=parallelogram, script="echo done"]
    start -> work
    work -> recover [condition="outcome=failed"]
    work -> done
    recover -> exit
    done -> exit
}}"#
    )
}

/// One long command, for the stop and recovery scenarios.
const LONG: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    long [shape=parallelogram, script="echo go > running; sleep 30"]
    start -> long
    long -> exit
}"#;

/// One command that prints thousands of lines, for the backlog scenario.
const CHATTY: &str = r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    chatty [shape=parallelogram, script="seq 1 2000"]
    start -> chatty
    chatty -> exit
}"#;

/// Real commands and human gates, simulated agents: the runtime the plan
/// asks for (scripted agents, real commands), with the host's extension
/// points installed when a scenario has them.
fn runtime(dir: &RunDir, hooks: Option<Arc<dyn ExecutionHooks>>) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    // The scenarios read the workspace after the run; `RunDir` removes it.
    options.retention = Retention::Always;
    options.echo = false;
    let mut registry = Runtime::standard().registry().clone();
    for kind in [&AGENT_KIND, &WAIT_KIND, &WORKFLOW_KIND] {
        registry.register_runner(Arc::new(StubStep::new((*kind).clone())));
    }
    registry.register(CommandStep);
    registry.register(HumanStep);
    registry.register(StageStep);
    registry.register(ForkStep);
    registry.register(BranchStep);
    registry.register(FanInStep);
    let rt = Runtime::standard()
        .frontend(Fabro::new())
        .steps(registry)
        .options(options);
    match hooks {
        Some(hooks) => rt.hooks(hooks),
        None => rt,
    }
}

fn lower(rt: &Runtime, dir: &RunDir, text: &str) -> Lowered {
    let path = dir.path().join("wf.fabro");
    fs::write(&path, text).expect("write the workflow");
    let lowered = rt
        .check(&path, None, None, &CompileInputs::new())
        .expect("loads");
    assert!(
        lowered.graph.is_some(),
        "the workflow lowers: {:?}",
        lowered.diagnostics
    );
    lowered
}

/// Script the agent stub `work` to ask for a retry on every attempt, with a
/// short backoff so the retry does not slow the scenario.
fn script_work_retrying(graph: &mut Graph) {
    let node = graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == "work")
        .expect("the work node");
    node.retry.backoff.initial = Duration::from_millis(10);
    node.retry.backoff.max = Duration::from_millis(10);
    let Value::Object(config) = &mut node.step.config else {
        panic!("an object config");
    };
    config.insert(
        "simulate".into(),
        json!({ "outcome": "failed", "failure_class": "retry_requested" }),
    );
}

/// Script the agent stub: the first attempt asks for a retry, the second
/// succeeds.
fn script_flaky(graph: &mut Graph) {
    let node = graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == "flaky")
        .expect("the flaky node");
    let Value::Object(config) = &mut node.step.config else {
        panic!("an object config");
    };
    config.insert(
        "simulate".into(),
        json!({
            "calls": [
                { "outcome": "failed", "failure_class": "retry_requested" },
                { "outcome": "succeeded" }
            ]
        }),
    );
}

/// The edge from `from` to `to` in the lowered graph.
fn edge_between(graph: &Graph, from: &str, to: &str) -> EdgeId {
    let target = graph
        .nodes
        .iter()
        .find(|n| n.name == to)
        .expect("the target node")
        .id;
    graph
        .nodes
        .iter()
        .find(|n| n.name == from)
        .expect("the source node")
        .routing
        .edges()
        .find(|edge| edge.to == target)
        .expect("an edge between them")
        .id
}

/// Answers every gate `Y`.
struct SayYes;

#[async_trait::async_trait]
impl Interviewer for SayYes {
    async fn reply(
        &self,
        _request: InterviewRequest,
        _cancel: CancellationToken,
    ) -> InterviewReply {
        InterviewReply::Answered(Answer::choice("Y"))
    }
}

// ── The fake host ──────────────────────────────────────────────────────────

/// What the fake host is told to do, by node.
#[derive(Clone, Debug, Default)]
struct Script {
    /// Hold this node's admission until released.
    pause:             Option<&'static str>,
    skip:              Option<&'static str>,
    block:             Option<&'static str>,
    /// Make this node's final failure a partial success.
    accept_failure_of: Option<&'static str>,
    /// Make this node's final partial success a failure again.
    fail_final_of:     Option<&'static str>,
    /// Replace this node's selected route with this edge.
    override_route:    Option<(&'static str, EdgeId)>,
    /// Sleep this long in this node's transition.
    delay:             Option<(&'static str, Duration)>,
    /// Fail this node's required transition work.
    fatal_transition:  Option<&'static str>,
    /// Report a best-effort problem in this node's transition.
    metadata_problem:  Option<&'static str>,
}

/// A trace entry the fake host writes as a note, so the order of its
/// callbacks is in the durable record too.
fn marker(phase: &str, view: &FiringView) -> Note {
    Note::new(
        "fake_host",
        json!({
            "phase": phase,
            "node": view.node_name(),
            "visit": view.visit,
            "attempt": view.attempt,
        }),
    )
}

/// Which nodes get checkpoint work, decided from source metadata and branch
/// role alone: the parallel parent and the fan-in keep it, internal branch
/// members, start nodes and synthetic nodes do not.
fn wants_checkpoint(view: &FiringView) -> bool {
    let kind = view.meta().get("kind").and_then(Value::as_str);
    if kind == Some("start") || view.meta().get("synthetic") == Some(&Value::Bool(true)) {
        return false;
    }
    !matches!(view.branch, BranchRole::Member(_))
}

/// What the host was handed to prepare, for one attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Handed {
    node:       String,
    attempt:    u32,
    status:     String,
    /// The stage's reported `output.outcome`, when the step set one.
    reported:   Option<String>,
    will_retry: bool,
    exhausted:  bool,
}

struct FakeHost {
    script:      Script,
    release:     Notify,
    paused:      AtomicU32,
    /// Every checkpoint decision, by node.
    checkpoints: Mutex<Vec<(String, bool)>>,
    /// Every callback, in the order the host ran them.
    calls:       Mutex<Vec<String>>,
    /// Every result the host was asked to prepare, in order.
    handed:      Mutex<Vec<Handed>>,
}

impl FakeHost {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            release: Notify::new(),
            paused: AtomicU32::new(0),
            checkpoints: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            handed: Mutex::new(Vec::new()),
        })
    }

    fn call(&self, what: String) {
        self.calls.lock().expect("not poisoned").push(what);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("not poisoned").clone()
    }

    /// What the host was handed for `node`, attempt by attempt.
    fn handed(&self, node: &str) -> Vec<Handed> {
        self.handed
            .lock()
            .expect("not poisoned")
            .iter()
            .filter(|h| h.node == node)
            .cloned()
            .collect()
    }
}

#[async_trait::async_trait]
impl ExecutionHooks for FakeHost {
    async fn before_attempt(
        &self,
        _context: &HookContext,
        request: AdmitAttempt,
    ) -> AttemptDecision {
        let view = &request.view;
        let name = view.node_name();
        self.call(format!("before_attempt {name} {}", view.attempt.raw()));
        if self.script.pause == Some(name) {
            self.paused.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
        }
        let admission = if self.script.skip == Some(name) {
            Admission::Skip {
                outcome: Outcome::new(Status::Skipped, json!({"by": "host"})),
            }
        } else if self.script.block == Some(name) {
            Admission::Block {
                reason: "the host refused this attempt".into(),
            }
        } else {
            Admission::Admit
        };
        AttemptDecision {
            admission,
            notes: vec![marker("before_attempt", view)],
        }
    }

    async fn prepare_result(
        &self,
        _context: &HookContext,
        request: PrepareResult,
    ) -> Result<Prepared, PrepareError> {
        let view = &request.view;
        let name = view.node_name();
        self.call(format!("prepare_result {name} {}", view.attempt.raw()));
        self.handed.lock().expect("not poisoned").push(Handed {
            node:       name.to_owned(),
            attempt:    view.attempt.raw(),
            status:     request.outcome.status.tag().to_owned(),
            reported:   request
                .outcome
                .output
                .get("outcome")
                .and_then(Value::as_str)
                .map(str::to_owned),
            will_retry: request.will_retry,
            exhausted:  request.exhausted,
        });
        let mut prepared = Prepared::unchanged();
        prepared.notes.push(marker("prepare_result", view));
        // An attempt the engine will retry is not the one to prepare: the
        // host waits for the final result.
        if request.will_retry {
            return Ok(prepared);
        }
        if self.script.accept_failure_of == Some(name)
            && let Status::Failure(info) = &request.outcome.status
        {
            prepared.adjustment = ResultAdjustment {
                status: Some(Status::partial(info.clone())),
                reason: Some("the host accepts this failure".into()),
                ..ResultAdjustment::default()
            };
        }
        if self.script.fail_final_of == Some(name)
            && let Status::PartialSuccess {
                underlying: Some(UnderlyingFailure::Failure(info)),
            } = &request.outcome.status
        {
            prepared.adjustment = ResultAdjustment {
                status: Some(Status::Failure(info.clone())),
                reason: Some("the host rejects this partial success".into()),
                ..ResultAdjustment::default()
            };
        }
        Ok(prepared)
    }

    async fn after_record(&self, _context: &HookContext, recorded: Recorded) -> Vec<Note> {
        let view = &recorded.view;
        self.call(format!("after_record {}", view.node_name()));
        vec![marker("after_record", view)]
    }

    async fn transition(
        &self,
        _context: &HookContext,
        transition: Transition,
    ) -> Result<TransitionReport, TransitionError> {
        let view = &transition.view;
        let name = view.node_name().to_owned();
        self.call(format!("transition {name}"));
        let checkpoint = wants_checkpoint(view);
        self.checkpoints
            .lock()
            .expect("not poisoned")
            .push((name.clone(), checkpoint));
        if let Some((node, delay)) = self.script.delay
            && node == name
        {
            sleep(delay).await;
        }
        if self.script.fatal_transition == Some(name.as_str()) {
            return Err(TransitionError::new("git commit failed: nothing to commit"));
        }
        let mut report = TransitionReport {
            notes: vec![marker("transition", view)],
            ..TransitionReport::default()
        };
        if self.script.metadata_problem == Some(name.as_str()) {
            report
                .problems
                .push("metadata write failed: connection refused".into());
        }
        if let Some((node, edge)) = self.script.override_route
            && node == name
        {
            report.overrides.push(RouteOverride { group: 0, edge });
        }
        Ok(report)
    }
}

// ── A fake hook service ────────────────────────────────────────────────────

/// Records every point it is asked about and skips one node.
struct FakeHooks {
    skip:  &'static str,
    calls: Mutex<Vec<(HookPoint, String, u32)>>,
}

#[async_trait::async_trait]
impl HookService for FakeHooks {
    async fn run(&self, request: HookRequest) -> HookReport {
        let Some(view) = request.view.as_deref() else {
            // The run-level points carry no firing.
            self.calls
                .lock()
                .expect("not poisoned")
                .push((request.point, String::new(), 0));
            return HookReport::proceed(request.point);
        };
        self.calls.lock().expect("not poisoned").push((
            request.point,
            view.node_name().to_owned(),
            view.attempt.raw(),
        ));
        let mut report = HookReport::proceed(request.point);
        if request.point == HookPoint::BeforeVisit && view.node_name() == self.skip {
            report.decision = HookDecision::Skip {
                status: Status::Skipped,
            };
            report.hooks.push(HookRun {
                name:        "stage_start".into(),
                state:       "executed".into(),
                duration_ms: Some(3),
                message:     None,
                usage:       None,
            });
        }
        report
    }
}

// ── Running ────────────────────────────────────────────────────────────────

struct RunOutcome {
    report:  driver::ExecutionReport,
    events:  Vec<RunEvent>,
    receipt: ProjectionReceipt,
}

/// A sink that takes its time: the slow consumer.
struct SlowSink {
    inner: CollectingSink,
    delay: Duration,
}

#[async_trait::async_trait]
impl RunEventSink for SlowSink {
    async fn deliver(&self, event: RunEvent) -> Result<(), SinkError> {
        sleep(self.delay).await;
        self.inner.deliver(event).await
    }
}

/// A sink that fails after `n` events.
struct FailingSink {
    inner:     CollectingSink,
    remaining: AtomicU32,
}

#[async_trait::async_trait]
impl RunEventSink for FailingSink {
    async fn deliver(&self, event: RunEvent) -> Result<(), SinkError> {
        if self.remaining.fetch_sub(1, Ordering::SeqCst) == 0 {
            return Err(SinkError::new("the projection store is down"));
        }
        self.inner.deliver(event).await
    }
}

/// A sink that takes `accepted` events and then never returns: the stalled
/// consumer.
struct StallingSink {
    inner:    CollectingSink,
    accepted: AtomicU32,
}

#[async_trait::async_trait]
impl RunEventSink for StallingSink {
    async fn deliver(&self, event: RunEvent) -> Result<(), SinkError> {
        if self.accepted.fetch_sub(1, Ordering::SeqCst) == 0 {
            pending::<()>().await;
        }
        self.inner.deliver(event).await
    }
}

async fn run_projected(
    rt: &Runtime,
    lowered: Lowered,
    sink: Arc<dyn RunEventSink>,
    observers: Vec<Arc<dyn ExecutionObserver>>,
) -> (driver::ExecutionReport, ProjectionReceipt) {
    run_projected_with(rt, lowered, sink, observers, ProjectorOptions::default()).await
}

async fn run_projected_with(
    rt: &Runtime,
    lowered: Lowered,
    sink: Arc<dyn RunEventSink>,
    observers: Vec<Arc<dyn ExecutionObserver>>,
    options: ProjectorOptions,
) -> (driver::ExecutionReport, ProjectionReceipt) {
    let projector = EventProjector::with_options(sink, options);
    let dispatcher = InterviewDispatcher::new(Arc::new(SayYes));
    let mut host_run = HostRun::new(lowered.graph.expect("lowers"))
        .with_children(lowered.children)
        .observe(projector.clone() as Arc<dyn ExecutionObserver>)
        .observe(Arc::new(dispatcher.clone()));
    for observer in observers {
        host_run = host_run.observe(observer);
    }
    let report = host::run_configured(rt, host_run, |handle, secrets| {
        dispatcher.wire(handle, secrets);
    })
    .await
    .expect("the run completes");
    let interview = dispatcher.shutdown().await;
    assert!(interview.is_clean(), "{:?}", interview.errors);
    let receipt = projector.shutdown().await;
    (report, receipt)
}

/// Run the baseline workflow, collect its events, and return everything.
async fn run_workflow(dir: &RunDir, hooks: Option<Arc<dyn ExecutionHooks>>) -> RunOutcome {
    run_workflow_with(dir, hooks, script_flaky).await
}

async fn run_workflow_with(
    dir: &RunDir,
    hooks: Option<Arc<dyn ExecutionHooks>>,
    edit: impl FnOnce(&mut Graph),
) -> RunOutcome {
    run_text_with(dir, WORKFLOW, hooks, edit).await
}

async fn run_text_with(
    dir: &RunDir,
    text: &str,
    hooks: Option<Arc<dyn ExecutionHooks>>,
    edit: impl FnOnce(&mut Graph),
) -> RunOutcome {
    let rt = runtime(dir, hooks);
    let mut lowered = lower(&rt, dir, text);
    edit(lowered.graph.as_mut().expect("lowers"));
    let sink = Arc::new(CollectingSink::default());
    let (report, receipt) = run_projected(&rt, lowered, sink.clone(), Vec::new()).await;
    RunOutcome {
        report,
        events: sink.events(),
        receipt,
    }
}

/// An event without its recording time, for the one comparison where a
/// re-recorded record legitimately carries a later time than its original.
/// The record carries the time too, so it is cleared there as well.
fn timeless(event: &RunEvent) -> RunEvent {
    use petri::execution::events::Record;
    let record = event.record.clone().map(|record| match record {
        Record::Coordinator(mut record) => {
            record.recorded_at = 0;
            Record::Coordinator(record)
        }
        Record::Engine(mut record) => {
            record.recorded_at = 0;
            Record::Engine(record)
        }
    });
    RunEvent {
        recorded_at: 0,
        record,
        ..event.clone()
    }
}

/// Run the baseline workflow under a host that replaced the hook service:
/// the service behind the adapter and behind the `HookServiceHandle` the
/// steps ask, and nothing of the local service.
async fn run_workflow_hosting(dir: &RunDir, service: Arc<dyn HookService>) -> RunOutcome {
    let adapter: Arc<dyn ExecutionHooks> = Arc::new(HookAdapter::new(service.clone()));
    let rt = runtime(dir, Some(adapter)).capability(HookServiceHandle(service));
    let mut lowered = lower(&rt, dir, WORKFLOW);
    script_flaky(lowered.graph.as_mut().expect("lowers"));
    let sink = Arc::new(CollectingSink::default());
    let (report, receipt) = run_projected(&rt, lowered, sink.clone(), Vec::new()).await;
    RunOutcome {
        report,
        events: sink.events(),
        receipt,
    }
}

/// Live events with the live-only timestamp removed, in identity order, so a
/// stream compares with its replay (which lists the coordinator log first).
/// The recording time stays: it is the same live and on replay.
fn normalized(events: &[RunEvent]) -> Vec<RunEvent> {
    let mut events: Vec<RunEvent> = events
        .iter()
        .cloned()
        .map(|mut e| {
            e.observed_at = None;
            e
        })
        .collect();
    events.sort_by_key(|e| e.id);
    events
}

// ── Reconstruction from public events ──────────────────────────────────────

/// What one node did, as a host would project it: from events alone.
#[derive(Debug, Default, PartialEq)]
struct NodeTimeline {
    kind:         String,
    visits:       u32,
    attempts:     Vec<(u32, String)>,
    final_status: Option<String>,
    executed:     Option<bool>,
    branch:       Option<String>,
    routes:       Vec<String>,
    waits:        Vec<WaitState>,
    questions:    Vec<String>,
    answers:      Vec<String>,
    notes:        Vec<String>,
    duration_ms:  u64,
}

/// One branch result as reconstructed: index, last node, status.
type JoinedBranch = (u32, String, String);

#[derive(Debug, Default)]
struct Timeline {
    run_status:  Option<RunStatus>,
    nodes:       BTreeMap<String, NodeTimeline>,
    forks:       Vec<(String, usize)>,
    joins:       Vec<(String, Vec<JoinedBranch>)>,
    ids:         Vec<EventId>,
    invocations: BTreeSet<u64>,
}

impl Timeline {
    fn from_events(events: &[RunEvent]) -> Self {
        let mut timeline = Self::default();
        for event in events {
            timeline.ids.push(event.id);
            if let Some(invocation) = event.context.invocation {
                timeline.invocations.insert(invocation.raw());
            }
            // A synthetic node is a lowering artifact (a parallel branch's
            // delegate, a goal check): the stage it stands for has events of
            // its own, so it takes no node timeline.
            let synthetic = event.subject.as_ref().is_some_and(|subject| {
                subject.node.meta.get("synthetic") == Some(&Value::Bool(true))
            });
            let node = event
                .subject
                .as_ref()
                .filter(|_| !synthetic)
                .map(|subject| {
                    let entry = timeline
                        .nodes
                        .entry(subject.node.name.to_string())
                        .or_default();
                    subject
                        .node
                        .meta
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .clone_into(&mut entry.kind);
                    if let Some(visit) = subject.visit {
                        entry.visits = entry.visits.max(visit);
                    }
                    entry.branch = Some(match &subject.branch {
                        BranchRole::None => "none".to_owned(),
                        BranchRole::Fork { branches } => format!("fork:{branches}"),
                        BranchRole::Member(branch) => format!("member:{}", branch.index),
                        BranchRole::Join { .. } => "join".to_owned(),
                    });
                    entry
                });
            if let Some(CoordinatorEvent::RunFinished { status, .. }) = event.coordinator() {
                timeline.run_status = Some(*status);
            }
            if let Some(ViewEvent::ForkCompleted { fork, results, .. }) = event.view() {
                timeline.joins.push((
                    fork.name.to_string(),
                    results
                        .iter()
                        .map(|r| {
                            (
                                r.branch.index,
                                r.node.name.to_string(),
                                r.status.tag().to_owned(),
                            )
                        })
                        .collect(),
                ));
            }
            let Some(entry) = node else {
                continue;
            };
            match (event.engine(), &event.derived) {
                (Some(Event::StepFinished { outcome, .. }), _) => {
                    let attempt = event
                        .subject
                        .as_ref()
                        .and_then(|s| s.attempt)
                        .map_or(0, Attempt::raw);
                    entry
                        .attempts
                        .push((attempt, outcome.status.tag().to_owned()));
                    entry.duration_ms += outcome.metrics.duration_ms.unwrap_or(0);
                }
                (Some(Event::RouteApplied { .. }), derived) => {
                    entry.routes.push(match derived {
                        Some(Derived::RouteApplied { target, .. }) => target.name.to_string(),
                        _ => "none".to_owned(),
                    });
                }
                (
                    Some(Event::ControlRequested { .. }),
                    Some(Derived::ControlRequested {
                        deliverable: true,
                        answer: Some(answer),
                    }),
                ) => entry
                    .answers
                    .push(answer.choice.clone().unwrap_or_default()),
                _ => {}
            }
            match event.parsed() {
                Some(Parsed::Question { question }) => {
                    entry.questions.push(question.text.clone());
                }
                Some(Parsed::Note { note, .. }) => {
                    let phase = note
                        .payload
                        .get("phase")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    entry.notes.push(format!("{}:{phase}", note.kind));
                }
                _ => {}
            }
            match event.view() {
                Some(ViewEvent::VisitCompleted {
                    outcome, executed, ..
                }) => {
                    entry.final_status = Some(outcome.status.tag().to_owned());
                    entry.executed = Some(*executed);
                }
                Some(ViewEvent::WaitStateChanged { state }) => entry.waits.push(*state),
                Some(ViewEvent::ForkStarted { branches, .. }) => {
                    timeline.forks.push((entry.kind.clone(), branches.len()));
                }
                _ => {}
            }
        }
        timeline
    }

    fn node(&self, name: &str) -> &NodeTimeline {
        self.nodes
            .get(name)
            .unwrap_or_else(|| panic!("node `{name}` in {:?}", self.nodes.keys()))
    }
}

fn bodies_of<'a>(events: &'a [RunEvent], node: &str) -> Vec<&'a RunEvent> {
    events
        .iter()
        .filter(|e| e.subject.as_ref().is_some_and(|s| s.node.name == node))
        .collect()
}

/// The note an event of `kind` carries, when it is one.
fn note_of<'a>(event: &'a RunEvent, kind: &str) -> Option<&'a Value> {
    event
        .note()
        .filter(|note| note.kind == kind)
        .map(|note| &note.payload)
}

fn assert_unique_ids(events: &[RunEvent]) {
    let ids: BTreeSet<EventId> = events.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), events.len(), "every event id is unique");
}

fn workspace(dir: &RunDir) -> PathBuf {
    dir.path().join("scopes/invocation-0-scope-0/work")
}

// ── Scenarios ──────────────────────────────────────────────────────────────

/// No adapters: the workflow runs, and the public events alone reconstruct
/// the timeline, the retry, the branches, the question and the accounting.
#[tokio::test]
async fn the_workflow_runs_without_adapters_and_the_events_reconstruct_it() {
    let dir = RunDir::new("embed-plain");
    let outcome = run_workflow(&dir, None).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    assert!(outcome.receipt.is_clean(), "{:?}", outcome.receipt);
    assert_eq!(
        outcome.receipt.delivered,
        outcome.events.len() as u64,
        "every projected event reached the sink"
    );
    assert_unique_ids(&outcome.events);

    let timeline = Timeline::from_events(&outcome.events);
    assert_eq!(timeline.run_status, Some(RunStatus::Success));
    // The root and the two branch children.
    assert_eq!(timeline.invocations, BTreeSet::from([0, 1, 2]));

    // The retry: two attempts, the first not final, then one final record.
    let flaky = timeline.node("flaky");
    assert_eq!(flaky.kind, "agent");
    assert_eq!(flaky.visits, 1);
    assert_eq!(flaky.attempts, vec![
        (1, "failure".to_owned()),
        (2, "success".to_owned())
    ]);
    assert_eq!(flaky.final_status.as_deref(), Some("success"));
    assert_eq!(flaky.executed, Some(true));
    assert!(
        flaky.waits.contains(&WaitState::AwaitingRetry),
        "{:?}",
        flaky.waits
    );
    let flaky_bodies = bodies_of(&outcome.events, "flaky");
    assert!(flaky_bodies.iter().any(|b| matches!(
        b.derived,
        Some(Derived::StepFinished {
            is_final:  false,
            exhausted: false,
        })
    )));
    assert!(
        flaky_bodies
            .iter()
            .any(|b| matches!(b.view(), Some(ViewEvent::RetryScheduled { .. })))
    );
    assert!(
        flaky_bodies
            .iter()
            .any(|b| matches!(b.engine(), Some(Event::RetryElapsed { .. })))
    );

    // The branches: keyed on source metadata, not on today's node shape.
    let fan = timeline.node("fan");
    assert_eq!(fan.kind, "parallel");
    assert_eq!(fan.branch.as_deref(), Some("fork:2"));
    assert_eq!(timeline.node("left").branch.as_deref(), Some("member:0"));
    assert_eq!(timeline.node("right").branch.as_deref(), Some("member:1"));
    let join = timeline.node("join");
    assert_eq!(join.kind, "parallel.fan_in");
    assert_eq!(join.branch.as_deref(), Some("join"));
    assert_eq!(timeline.forks, vec![("parallel".to_owned(), 2)]);
    assert_eq!(timeline.joins, vec![("fan".to_owned(), vec![
        (0, "left".to_owned(), "success".to_owned()),
        (1, "right".to_owned(), "success".to_owned()),
    ])]);
    assert_eq!(timeline.node("gate").branch.as_deref(), Some("none"));

    // The question and its answer, with the wait states between.
    let gate = timeline.node("gate");
    assert_eq!(gate.questions, vec!["Ship?".to_owned()]);
    assert_eq!(gate.answers, vec!["Y".to_owned()]);
    assert_eq!(gate.waits, vec![
        WaitState::AwaitingAdmission,
        WaitState::Running,
        WaitState::AwaitingAnswer,
        WaitState::Running,
    ]);
    assert_eq!(gate.routes, vec!["ship".to_owned()]);
    assert!(!timeline.nodes.contains_key("hold"));

    // Every executed node's attempt has an observed duration.
    for name in ["prepare", "left", "right", "ship"] {
        assert_eq!(timeline.node(name).executed, Some(true));
        assert!(
            timeline.node(name).attempts.len() == 1,
            "{name}: {:?}",
            timeline.node(name).attempts
        );
    }
    assert!(timeline.node("ship").duration_ms > 0 || timeline.node("prepare").duration_ms > 0);
    // The start noop runs as a step; the exit node has no routes at all.
    assert_eq!(timeline.node("start").executed, Some(true));
    assert!(timeline.node("exit").routes.is_empty());

    // The stream from the run dir equals the live stream, identity for
    // identity, with the live-only timestamp stripped.
    let mut replayed = replay_run_dir(dir.path())
        .await
        .expect("the run dir projects");
    replayed.sort_by_key(|e| e.id);
    assert_eq!(replayed, normalized(&outcome.events));

    assert!(workspace(&dir).join("shipped.txt").exists());
}

/// With the fake host installed, the callbacks run in the plan's order for
/// each completed node, the notes land in the durable record before the
/// records they precede, and the checkpoint filter follows source metadata.
#[tokio::test]
async fn adapters_run_in_order_and_checkpoint_work_follows_source_metadata() {
    let dir = RunDir::new("embed-order");
    let host = FakeHost::new(Script::default());
    let outcome = run_workflow(&dir, Some(host.clone())).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );

    // The order for one ordinary completed node, from the host's own trace.
    let calls = host.calls();
    let of = |node: &str| -> Vec<&str> {
        calls
            .iter()
            .filter(|c| c.split(' ').nth(1) == Some(node))
            .map(String::as_str)
            .collect()
    };
    assert_eq!(of("prepare"), vec![
        "before_attempt prepare 1",
        "prepare_result prepare 1",
        "after_record prepare",
        "transition prepare",
    ]);
    // Retry attempts each get admission and result preparation; the record
    // and transition points run once, on the final attempt.
    assert_eq!(of("flaky"), vec![
        "before_attempt flaky 1",
        "prepare_result flaky 1",
        "before_attempt flaky 2",
        "prepare_result flaky 2",
        "after_record flaky",
        "transition flaky",
    ]);
    // A no-route completion still gets its transition.
    assert_eq!(of("exit"), vec![
        "before_attempt exit 1",
        "prepare_result exit 1",
        "after_record exit",
        "transition exit",
    ]);

    // The same order, from the durable record.
    let timeline = Timeline::from_events(&outcome.events);
    assert_eq!(timeline.node("prepare").notes, vec![
        "fake_host:before_attempt",
        "fake_host:prepare_result",
        "fake_host:after_record",
        "fake_host:transition",
    ]);
    // Notes precede the records they annotate.
    let prepare = bodies_of(&outcome.events, "prepare");
    let position = |pred: &dyn Fn(&RunEvent) -> bool| {
        prepare
            .iter()
            .position(|b| pred(b))
            .expect("the event is present")
    };
    let note_at = |phase: &str| {
        position(&|b| {
            b.note().is_some_and(|note| {
                note.payload.get("phase").and_then(Value::as_str) == Some(phase)
            })
        })
    };
    let admitted = |b: &RunEvent| {
        matches!(
            b.engine(),
            Some(Event::AdmissionDecided {
                decision_id: DecisionId::AttemptStart { .. },
                ..
            })
        )
    };
    assert!(note_at("before_attempt") < position(&admitted));
    assert!(
        note_at("prepare_result")
            < position(&|b| matches!(b.engine(), Some(Event::StepFinished { .. })))
    );
    assert!(
        position(&|b| matches!(b.view(), Some(ViewEvent::VisitCompleted { .. })))
            < note_at("after_record")
    );
    let resolved = |b: &RunEvent| matches!(b.engine(), Some(Event::RoutingResolved { .. }));
    assert!(note_at("after_record") < position(&resolved));
    assert!(note_at("transition") < position(&resolved));
    assert!(
        position(&resolved) < position(&|b| matches!(b.engine(), Some(Event::RouteApplied { .. })))
    );

    // Checkpoint work: kept for the parallel parent and the fan-in, omitted
    // for the internal branches and the start node.
    let checkpoints: BTreeMap<String, bool> = host
        .checkpoints
        .lock()
        .expect("not poisoned")
        .iter()
        .cloned()
        .collect();
    assert_eq!(checkpoints.get("start"), Some(&false));
    assert_eq!(checkpoints.get("fan"), Some(&true));
    assert_eq!(checkpoints.get("left"), Some(&false));
    assert_eq!(checkpoints.get("right"), Some(&false));
    assert_eq!(checkpoints.get("join"), Some(&true));
    assert_eq!(checkpoints.get("prepare"), Some(&true));

    // Replay still equals the live stream with hooks installed.
    let mut replayed = replay_run_dir(dir.path())
        .await
        .expect("the run dir projects");
    replayed.sort_by_key(|e| e.id);
    assert_eq!(replayed, normalized(&outcome.events));
}

/// A paused visit keeps one identity, starts no attempt, and continues when
/// released; waiting creates no extra visit.
#[tokio::test]
async fn a_paused_admission_holds_one_visit_and_resumes_on_release() {
    let dir = RunDir::new("embed-pause");
    let host = FakeHost::new(Script {
        pause: Some("fan"),
        ..Script::default()
    });
    let sink = Arc::new(CollectingSink::default());
    let rt = runtime(&dir, Some(host.clone()));
    let mut lowered = lower(&rt, &dir, WORKFLOW);
    script_flaky(lowered.graph.as_mut().expect("lowers"));

    let releaser = host.clone();
    let sink_view = sink.clone();
    let release = tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(20);
        while releaser.paused.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "never paused");
            sleep(Duration::from_millis(10)).await;
        }
        // Let the stream settle, then look at what exists while paused.
        sleep(Duration::from_millis(200)).await;
        let paused_events = sink_view.events();
        releaser.release.notify_waiters();
        paused_events
    });
    let (report, receipt) = run_projected(&rt, lowered, sink.clone(), Vec::new()).await;
    let paused_events = release.await.expect("the releaser ran");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(receipt.is_clean());

    let fan_while_paused = bodies_of(&paused_events, "fan");
    assert!(
        fan_while_paused
            .iter()
            .any(|b| matches!(b.view(), Some(ViewEvent::VisitStarted { .. }))),
        "the visit exists while paused"
    );
    assert!(
        !fan_while_paused
            .iter()
            .any(|b| matches!(b.engine(), Some(Event::StepStarted { .. }))),
        "no attempt starts while paused"
    );
    let timeline = Timeline::from_events(&sink.events());
    assert_eq!(timeline.node("fan").visits, 1);
    assert_eq!(host.paused.load(Ordering::SeqCst), 1);
    let firings: BTreeSet<_> = sink
        .events()
        .iter()
        .filter_map(|e| e.subject.as_ref())
        .filter(|s| s.node.name == "fan")
        .filter_map(|s| s.firing)
        .collect();
    assert_eq!(firings.len(), 1, "one firing identity across the pause");
}

/// A paused visit is still cancellable: the cancel settles it without an
/// attempt and the run ends cancelled.
#[tokio::test]
async fn a_paused_visit_is_cancelled_without_starting() {
    let dir = RunDir::new("embed-pause-cancel");
    let host = FakeHost::new(Script {
        pause: Some("long"),
        ..Script::default()
    });
    let rt = runtime(&dir, Some(host.clone()));
    let lowered = lower(&rt, &dir, LONG);
    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::new(sink.clone());
    let host_run = HostRun::new(lowered.graph.expect("lowers"))
        .observe(projector.clone() as Arc<dyn ExecutionObserver>);
    let canceller = host.clone();
    let report = host::run_configured(&rt, host_run, move |handle, _| {
        tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(20);
            while canceller.paused.load(Ordering::SeqCst) == 0 {
                assert!(Instant::now() < deadline, "never paused");
                sleep(Duration::from_millis(10)).await;
            }
            handle.cancel_root();
        });
    })
    .await
    .expect("the run completes");
    let receipt = projector.shutdown().await;
    assert!(receipt.is_clean());
    assert_eq!(report.status, RunStatus::Cancelled);
    let events = sink.events();
    let long = bodies_of(&events, "long");
    assert!(
        !long
            .iter()
            .any(|b| matches!(b.engine(), Some(Event::StepStarted { .. })))
    );
    assert!(long.iter().any(|b| matches!(
        b.view(),
        Some(ViewEvent::VisitCompleted { outcome, executed: false, .. })
            if outcome.status == Status::Cancelled
    )));
    assert!(
        !workspace(&dir).join("running").exists(),
        "the step never ran"
    );
}

/// A host skip ends the visit without an attempt and routing still runs; a
/// host block fails the attempt and the run.
#[tokio::test]
async fn skip_and_block_end_a_visit_without_an_attempt() {
    let dir = RunDir::new("embed-skip");
    let host = FakeHost::new(Script {
        skip: Some("ship"),
        ..Script::default()
    });
    let outcome = run_workflow(&dir, Some(host)).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    let timeline = Timeline::from_events(&outcome.events);
    let ship = timeline.node("ship");
    assert_eq!(ship.final_status.as_deref(), Some("skipped"));
    assert_eq!(ship.executed, Some(false));
    assert!(ship.attempts.is_empty());
    assert_eq!(ship.routes, vec!["exit".to_owned()]);
    assert!(bodies_of(&outcome.events, "ship").iter().any(|b| matches!(
        b.engine(),
        Some(Event::AdmissionDecided { decision: Admission::Skip { .. }, trace, .. })
            if trace.iter().any(|k| k.as_str() == "host.before_attempt")
    )));
    assert!(!workspace(&dir).join("shipped.txt").exists());

    let dir = RunDir::new("embed-block");
    let host = FakeHost::new(Script {
        block: Some("prepare"),
        ..Script::default()
    });
    let outcome = run_workflow(&dir, Some(host)).await;
    assert_eq!(outcome.report.status, RunStatus::Failed);
    let timeline = Timeline::from_events(&outcome.events);
    let prepare = timeline.node("prepare");
    assert_eq!(prepare.final_status.as_deref(), Some("failure"));
    assert!(prepare.attempts.is_empty());
    assert!(!workspace(&dir).join("prepared.txt").exists());
    assert!(
        bodies_of(&outcome.events, "prepare")
            .iter()
            .any(|b| matches!(
                b.engine(),
                Some(Event::AdmissionDecided {
                    decision: Admission::Block { .. },
                    ..
                })
            ))
    );
}

/// A host that accepts a failure records the original evidence beside the
/// effective result.
#[tokio::test]
async fn a_prepared_result_keeps_the_original_attempt_evidence() {
    let dir = RunDir::new("embed-prepare");
    let host = FakeHost::new(Script {
        accept_failure_of: Some("prepare"),
        ..Script::default()
    });
    let outcome = run_workflow_with(&dir, Some(host), |graph| {
        script_flaky(graph);
        let node = graph
            .body
            .nodes
            .iter_mut()
            .find(|n| n.name == "prepare")
            .expect("prepare");
        node.step.config["script"] = json!("echo boom >&2; exit 3");
    })
    .await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    let timeline = Timeline::from_events(&outcome.events);
    let prepare = timeline.node("prepare");
    assert_eq!(prepare.final_status.as_deref(), Some("partial_success"));
    assert_eq!(prepare.attempts, vec![(1, "partial_success".to_owned())]);
    let evidence = bodies_of(&outcome.events, "prepare")
        .into_iter()
        .find_map(|b| note_of(b, RESULT_PREPARED_KIND))
        .expect("the original evidence is recorded");
    assert_eq!(
        evidence["original"]["failure"]["class"],
        json!("exit_status:3")
    );
    assert_eq!(
        evidence["effective"]["partial_success"]["underlying"]["failure"]["class"],
        json!("exit_status:3")
    );
    assert_eq!(evidence["reason"], json!("the host accepts this failure"));
    // The effective record carries the underlying failure too.
    let recorded = outcome
        .report
        .state
        .history()
        .iter()
        .find(|r| r.name == "prepare")
        .expect("recorded");
    assert!(matches!(
        &recorded.outcome.status,
        Status::PartialSuccess { underlying: Some(UnderlyingFailure::Failure(info)) }
            if info.class == "exit_status:3"
    ));
}

/// The `result_prepared` note recorded for `node`, when the host changed a
/// result.
fn result_prepared_note(events: &[RunEvent], node: &str) -> Value {
    bodies_of(events, node)
        .into_iter()
        .find_map(|b| note_of(b, RESULT_PREPARED_KIND).cloned())
        .expect("the original evidence is recorded")
}

/// The position of the first event of `node` that `pred` accepts.
fn position_of(events: &[RunEvent], node: &str, pred: &dyn Fn(&RunEvent) -> bool) -> usize {
    bodies_of(events, node)
        .iter()
        .position(|b| pred(b))
        .expect("the event is present")
}

/// Exhausted retries under the engine's own exhaustion policy: the host is
/// handed each attempt, and on the final one the effective result the engine
/// will record (the `PartialSuccess` that `AcceptPartial` makes), not the
/// step's intermediate failure. It may change that result, and the routes
/// follow the change.
#[tokio::test]
async fn an_exhausted_retry_is_prepared_as_the_final_effective_result() {
    let accept_partial = |graph: &mut Graph| {
        script_work_retrying(graph);
        let node = graph
            .body
            .nodes
            .iter_mut()
            .find(|n| n.name == "work")
            .expect("the work node");
        // Another dialect's exhaustion policy, set on the node directly: the
        // Fabro lowering keeps the engine's exhaustion at `Fail`.
        node.retry.on_exhaustion = Exhaustion::AcceptPartial;
    };
    let workflow = recovery_workflow(r#"shape=box, prompt="try", max_retries=1"#);

    // Left alone, the final attempt is handed over already finalized, and the
    // record is what the host saw.
    let dir = RunDir::new("embed-exhausted-seen");
    let host = FakeHost::new(Script::default());
    let outcome = run_text_with(&dir, &workflow, Some(host.clone()), accept_partial).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    assert_eq!(host.handed("work"), vec![
        Handed {
            node:       "work".into(),
            attempt:    1,
            status:     "failure".into(),
            reported:   Some("failed".into()),
            will_retry: true,
            exhausted:  false,
        },
        Handed {
            node:       "work".into(),
            attempt:    2,
            status:     "partial_success".into(),
            reported:   Some("failed".into()),
            will_retry: false,
            exhausted:  true,
        },
    ]);
    let timeline = Timeline::from_events(&outcome.events);
    let work = timeline.node("work");
    assert_eq!(work.attempts, vec![
        (1, "failure".to_owned()),
        (2, "partial_success".to_owned())
    ]);
    assert_eq!(work.final_status.as_deref(), Some("partial_success"));
    assert_eq!(work.routes, vec!["done".to_owned()]);

    // Changed by the host, the final result is recorded as changed, the
    // original evidence beside it, and routing takes the failure edge the
    // effective result matches.
    let dir = RunDir::new("embed-exhausted-changed");
    let host = FakeHost::new(Script {
        fail_final_of: Some("work"),
        ..Script::default()
    });
    let outcome = run_text_with(&dir, &workflow, Some(host.clone()), accept_partial).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    assert_eq!(host.handed("work").len(), 2);
    let timeline = Timeline::from_events(&outcome.events);
    let work = timeline.node("work");
    assert_eq!(work.attempts, vec![
        (1, "failure".to_owned()),
        (2, "failure".to_owned())
    ]);
    assert_eq!(work.final_status.as_deref(), Some("failure"));
    assert_eq!(work.routes, vec!["recover".to_owned()]);
    let evidence = result_prepared_note(&outcome.events, "work");
    assert_eq!(evidence["attempt"], json!(2));
    assert_eq!(
        evidence["original"]["partial_success"]["underlying"]["failure"]["class"],
        json!("retry_requested")
    );
    assert_eq!(
        evidence["effective"]["failure"]["class"],
        json!("retry_requested")
    );
    assert_eq!(
        evidence["reason"],
        json!("the host rejects this partial success")
    );
    // The preparation precedes the final attempt's record.
    let final_note = position_of(&outcome.events, "work", &|b| {
        note_of(b, RESULT_PREPARED_KIND).is_some()
    });
    let records: Vec<usize> = bodies_of(&outcome.events, "work")
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b.engine(), Some(Event::StepFinished { .. })))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(records.len(), 2);
    assert!(
        final_note > records[0],
        "the note follows the first attempt's record"
    );
    assert!(
        final_note < records[1],
        "the note precedes the final attempt's record"
    );
    let recorded = outcome
        .report
        .state
        .history()
        .iter()
        .find(|r| r.name == "work")
        .expect("recorded");
    assert!(matches!(
        &recorded.outcome.status,
        Status::Failure(info) if info.class == "retry_requested"
    ));
}

/// Exhausted retries under Fabro's own policy: the stage finalizes the
/// exhausted retry inside the step, so the host is handed that decision (here
/// the failure kept for its explicit `outcome=failed` edge under
/// `on_failure="succeed"`), and a host that accepts it moves the run onto
/// the success edge.
#[tokio::test]
async fn a_fabro_exhaustion_decision_is_prepared_before_its_record() {
    let workflow =
        recovery_workflow(r#"shape=box, prompt="try", max_retries=1, on_failure="succeed""#);

    let dir = RunDir::new("embed-fabro-exhausted-seen");
    let host = FakeHost::new(Script::default());
    let outcome = run_text_with(&dir, &workflow, Some(host.clone()), script_work_retrying).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    assert_eq!(host.handed("work"), vec![
        Handed {
            node:       "work".into(),
            attempt:    1,
            status:     "failure".into(),
            reported:   Some("failed".into()),
            will_retry: true,
            exhausted:  false,
        },
        Handed {
            node:       "work".into(),
            attempt:    2,
            status:     "failure".into(),
            reported:   Some("failed".into()),
            will_retry: false,
            exhausted:  true,
        },
    ]);
    let timeline = Timeline::from_events(&outcome.events);
    assert_eq!(
        timeline.node("work").final_status.as_deref(),
        Some("failure")
    );
    assert_eq!(timeline.node("work").routes, vec!["recover".to_owned()]);

    let dir = RunDir::new("embed-fabro-exhausted-changed");
    let host = FakeHost::new(Script {
        accept_failure_of: Some("work"),
        ..Script::default()
    });
    let outcome = run_text_with(&dir, &workflow, Some(host.clone()), script_work_retrying).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    assert_eq!(
        host.handed("work").len(),
        2,
        "both attempts are handed over"
    );
    let timeline = Timeline::from_events(&outcome.events);
    let work = timeline.node("work");
    assert_eq!(work.attempts, vec![
        (1, "failure".to_owned()),
        (2, "partial_success".to_owned())
    ]);
    assert_eq!(work.final_status.as_deref(), Some("partial_success"));
    assert_eq!(work.routes, vec!["done".to_owned()]);
    let evidence = result_prepared_note(&outcome.events, "work");
    assert_eq!(evidence["attempt"], json!(2));
    assert_eq!(
        evidence["original"]["failure"]["class"],
        json!("retry_requested")
    );
}

/// An ordinary, non-retryable failure: one attempt, handed over as final and
/// exhausted, and a host that accepts it moves the run onto the success edge
/// instead of the explicit failure edge.
#[tokio::test]
async fn an_ordinary_failure_is_prepared_before_its_record_and_routes_on_the_effective_result() {
    let workflow = recovery_workflow(r#"shape=parallelogram, script="echo boom >&2; exit 3""#);

    let dir = RunDir::new("embed-failure-seen");
    let host = FakeHost::new(Script::default());
    let outcome = run_text_with(&dir, &workflow, Some(host.clone()), |_| {}).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    assert_eq!(host.handed("work"), vec![Handed {
        node:       "work".into(),
        attempt:    1,
        status:     "failure".into(),
        reported:   Some("failed".into()),
        will_retry: false,
        exhausted:  true,
    }]);
    let timeline = Timeline::from_events(&outcome.events);
    assert_eq!(
        timeline.node("work").final_status.as_deref(),
        Some("failure")
    );
    assert_eq!(timeline.node("work").routes, vec!["recover".to_owned()]);

    let dir = RunDir::new("embed-failure-changed");
    let host = FakeHost::new(Script {
        accept_failure_of: Some("work"),
        ..Script::default()
    });
    let outcome = run_text_with(&dir, &workflow, Some(host.clone()), |_| {}).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    let timeline = Timeline::from_events(&outcome.events);
    let work = timeline.node("work");
    assert_eq!(work.attempts, vec![(1, "partial_success".to_owned())]);
    assert_eq!(work.final_status.as_deref(), Some("partial_success"));
    assert_eq!(work.routes, vec!["done".to_owned()]);
    let evidence = result_prepared_note(&outcome.events, "work");
    assert_eq!(
        evidence["original"]["failure"]["class"],
        json!("exit_status:3")
    );
    assert_eq!(
        evidence["effective"]["partial_success"]["underlying"]["failure"]["class"],
        json!("exit_status:3")
    );
    let note = position_of(&outcome.events, "work", &|b| {
        note_of(b, RESULT_PREPARED_KIND).is_some()
    });
    let record = position_of(&outcome.events, "work", &|b| {
        matches!(b.engine(), Some(Event::StepFinished { .. }))
    });
    assert!(note < record, "the preparation precedes the record");
}

/// A route override changes where the run goes; a fatal transition blocks
/// advancement; a best-effort problem is recorded and the run continues; a
/// slow transition delays without losing anything.
#[tokio::test]
async fn transitions_override_block_or_continue() {
    // Override: the gate says ship, the host says hold.
    let dir = RunDir::new("embed-override");
    let rt = runtime(&dir, None);
    let lowered = lower(&rt, &dir, WORKFLOW);
    let hold = edge_between(lowered.graph.as_ref().expect("lowers"), "gate", "hold");
    let host = FakeHost::new(Script {
        override_route: Some(("gate", hold)),
        ..Script::default()
    });
    let outcome = run_workflow(&dir, Some(host)).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    let timeline = Timeline::from_events(&outcome.events);
    assert_eq!(timeline.node("gate").routes, vec!["hold".to_owned()]);
    assert!(timeline.nodes.contains_key("hold"));
    assert!(!timeline.nodes.contains_key("ship"));
    assert!(bodies_of(&outcome.events, "gate").iter().any(|b| matches!(
        b.engine(),
        Some(Event::RoutingResolved { groups, .. })
            if groups[0].trace.iter().any(|i| matches!(i, Intervention::Override { middleware, .. } if middleware.as_str() == "host.transition"))
    )));
    assert!(bodies_of(&outcome.events, "gate").iter().any(|b| {
        note_of(b, TRANSITION_KIND).is_some_and(|payload| {
            payload["overrides"]
                .as_array()
                .is_some_and(|o| o.len() == 1)
        })
    }));
    assert!(workspace(&dir).join("held.txt").exists());
    assert!(!workspace(&dir).join("shipped.txt").exists());

    // Fatal: the required work after `prepare` fails; nothing advances.
    let dir = RunDir::new("embed-fatal");
    let host = FakeHost::new(Script {
        fatal_transition: Some("prepare"),
        ..Script::default()
    });
    let outcome = run_workflow(&dir, Some(host)).await;
    assert_eq!(outcome.report.status, RunStatus::Failed);
    let timeline = Timeline::from_events(&outcome.events);
    assert_eq!(
        timeline.node("prepare").final_status.as_deref(),
        Some("success")
    );
    assert_eq!(timeline.node("prepare").routes, vec!["none".to_owned()]);
    assert!(
        !timeline.nodes.contains_key("flaky"),
        "advancement was blocked"
    );
    assert!(bodies_of(&outcome.events, "prepare").iter().any(|b| matches!(
        b.engine(),
        Some(Event::RoutingResolved { groups, .. })
            if matches!(&groups[0].decision, RouteDecision::Block { reason } if reason.contains("git commit failed"))
    )));
    assert!(bodies_of(&outcome.events, "prepare").iter().any(|b| {
        note_of(b, TRANSITION_KIND).is_some_and(|payload| {
            payload["blocked"]
                .as_str()
                .is_some_and(|r| r.contains("git commit failed"))
        })
    }));
    assert!(
        workspace(&dir).join("prepared.txt").exists(),
        "the step itself ran"
    );

    // Best effort: the metadata write fails, the run continues.
    let dir = RunDir::new("embed-metadata");
    let host = FakeHost::new(Script {
        metadata_problem: Some("join"),
        delay: Some(("fan", Duration::from_millis(300))),
        ..Script::default()
    });
    let outcome = run_workflow(&dir, Some(host)).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    let timeline = Timeline::from_events(&outcome.events);
    assert_eq!(timeline.node("join").routes, vec!["gate".to_owned()]);
    assert!(bodies_of(&outcome.events, "join").iter().any(|b| {
        note_of(b, TRANSITION_KIND).is_some_and(|payload| {
            payload["problems"][0]
                .as_str()
                .is_some_and(|p| p.contains("metadata write failed"))
        })
    }));
    assert!(workspace(&dir).join("shipped.txt").exists());
    // The delayed fork transition still applied both branches, once.
    assert_eq!(timeline.node("fan").routes, vec![
        "left".to_owned(),
        "right".to_owned()
    ]);
}

/// The hook service is asked once per point per attempt, at the plan's
/// points, and its decision takes effect through the same adapter that a
/// platform host's service would use. The points the steps ask themselves
/// reach the same service, once each, with nothing of the local service
/// installed: `start` asks for the environment, the run start and its own
/// admission with the sandbox in place, in that order; the fork and the
/// fan-in announce the branches; the run's end and the scope's release
/// arrive with no firing.
#[tokio::test]
async fn a_hook_service_runs_each_hook_once_at_its_point() {
    let dir = RunDir::new("embed-hooks");
    let service = Arc::new(FakeHooks {
        skip:  "ship",
        calls: Mutex::new(Vec::new()),
    });
    let outcome = run_workflow_hosting(&dir, service.clone()).await;
    assert_eq!(
        outcome.report.status,
        RunStatus::Success,
        "{:?}",
        outcome.report.state.errors()
    );
    let calls = service.calls.lock().expect("not poisoned").clone();
    let of = |node: &str| -> Vec<(HookPoint, u32)> {
        calls
            .iter()
            .filter(|(_, n, _)| n == node)
            .map(|(p, _, a)| (*p, *a))
            .collect()
    };
    assert_eq!(of("start"), vec![
        (HookPoint::ScopeReady, 1),
        (HookPoint::RunStarted, 1),
        (HookPoint::BeforeVisit, 1),
        (HookPoint::BeforeAttempt, 1),
        (HookPoint::AfterAttempt, 1),
        (HookPoint::AfterVisit, 1),
        (HookPoint::RouteSelected, 1),
    ]);
    assert_eq!(of("prepare"), vec![
        (HookPoint::BeforeVisit, 1),
        (HookPoint::BeforeAttempt, 1),
        (HookPoint::AfterAttempt, 1),
        (HookPoint::AfterVisit, 1),
        (HookPoint::RouteSelected, 1),
    ]);
    assert_eq!(of("fan"), vec![
        (HookPoint::BeforeVisit, 1),
        (HookPoint::BeforeAttempt, 1),
        (HookPoint::ForkStarted, 1),
        (HookPoint::AfterAttempt, 1),
        (HookPoint::AfterVisit, 1),
        (HookPoint::RouteSelected, 1),
    ]);
    assert_eq!(of("join"), vec![
        (HookPoint::BeforeVisit, 1),
        (HookPoint::BeforeAttempt, 1),
        (HookPoint::ForkCompleted, 1),
        (HookPoint::AfterAttempt, 1),
        (HookPoint::AfterVisit, 1),
        (HookPoint::RouteSelected, 1),
    ]);
    // The run-level points carry no firing.
    assert_eq!(of(""), vec![
        (HookPoint::RunFinished, 0),
        (HookPoint::ScopeReleased, 0)
    ]);
    assert_eq!(of("flaky"), vec![
        (HookPoint::BeforeVisit, 1),
        (HookPoint::BeforeAttempt, 1),
        (HookPoint::AfterAttempt, 1),
        (HookPoint::Retrying, 2),
        (HookPoint::BeforeAttempt, 2),
        (HookPoint::AfterAttempt, 2),
        (HookPoint::AfterVisit, 2),
        (HookPoint::RouteSelected, 2),
    ]);
    // The skipped node was asked once and never ran.
    assert_eq!(of("ship"), vec![
        (HookPoint::BeforeVisit, 1),
        (HookPoint::AfterVisit, 1),
        (HookPoint::RouteSelected, 1)
    ]);
    let mut seen = BTreeSet::new();
    for call in &calls {
        assert!(seen.insert(call.clone()), "hook ran twice: {call:?}");
    }
    let timeline = Timeline::from_events(&outcome.events);
    assert_eq!(
        timeline.node("ship").final_status.as_deref(),
        Some("skipped")
    );
    assert!(bodies_of(&outcome.events, "ship").iter().any(|b| {
        note_of(b, HOOK_NOTE_KIND).is_some_and(|payload| {
            payload["decision"]["decision"] == json!("skip")
                && payload["hooks"][0]["name"] == json!("stage_start")
        })
    }));
    assert!(!workspace(&dir).join("shipped.txt").exists());
}

/// A slow consumer whose backlog stays within the queue's capacity delays
/// delivery and loses nothing; a failing consumer stops with an honest
/// receipt, and the run dir still projects everything.
#[tokio::test]
async fn slow_and_failing_consumers_are_lossless_or_honest() {
    let dir = RunDir::new("embed-slow");
    let rt = runtime(&dir, None);
    let mut lowered = lower(&rt, &dir, WORKFLOW);
    script_flaky(lowered.graph.as_mut().expect("lowers"));
    let slow = Arc::new(SlowSink {
        inner: CollectingSink::default(),
        delay: Duration::from_millis(3),
    });
    let (report, receipt) = run_projected(&rt, lowered, slow.clone(), Vec::new()).await;
    assert_eq!(report.status, RunStatus::Success);
    assert!(receipt.is_clean(), "{receipt:?}");
    let delivered = slow.inner.events();
    assert_eq!(receipt.delivered, delivered.len() as u64);
    assert_eq!(receipt.projected, receipt.delivered);
    let replayed = replay_run_dir(dir.path()).await.expect("projects");
    assert_eq!(
        replayed.len(),
        delivered.len(),
        "the slow sink lost nothing"
    );

    let dir = RunDir::new("embed-failing");
    let rt = runtime(&dir, None);
    let mut lowered = lower(&rt, &dir, WORKFLOW);
    script_flaky(lowered.graph.as_mut().expect("lowers"));
    let failing = Arc::new(FailingSink {
        inner:     CollectingSink::default(),
        remaining: AtomicU32::new(10),
    });
    let (report, receipt) = run_projected(&rt, lowered, failing.clone(), Vec::new()).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "a sink failure never fails the run"
    );
    assert!(!receipt.is_clean());
    assert_eq!(receipt.delivered, 10);
    assert!(receipt.undelivered > 0);
    assert_eq!(receipt.projected, receipt.delivered + receipt.undelivered);
    assert_eq!(
        receipt.failure.as_deref(),
        Some("the projection store is down")
    );
    let replayed = replay_run_dir(dir.path()).await.expect("projects");
    assert_eq!(
        replayed.len() as u64,
        receipt.projected,
        "recovery rebuilds the whole stream"
    );
    let replayed_ids: BTreeSet<EventId> = replayed.iter().map(|e| e.id).collect();
    let delivered = failing.inner.events();
    assert_eq!(delivered.len(), 10);
    assert!(delivered.iter().all(|e| replayed_ids.contains(&e.id)));
}

/// A sink that stops returning is abandoned after the stall budget: the run
/// finishes, shutdown completes, the receipt names the stall and counts the
/// events the sink never took, and the run dir still projects everything.
#[tokio::test]
async fn a_stalled_sink_is_abandoned_after_its_budget_and_the_run_dir_still_projects() {
    let dir = RunDir::new("embed-stalled");
    let rt = runtime(&dir, None);
    let mut lowered = lower(&rt, &dir, WORKFLOW);
    script_flaky(lowered.graph.as_mut().expect("lowers"));
    let stalled = Arc::new(StallingSink {
        inner:    CollectingSink::default(),
        accepted: AtomicU32::new(10),
    });
    let options = ProjectorOptions {
        stall_timeout: Duration::from_millis(300),
        ..ProjectorOptions::default()
    };
    let (report, receipt) = timeout(
        Duration::from_secs(60),
        run_projected_with(&rt, lowered, stalled.clone(), Vec::new(), options),
    )
    .await
    .expect("shutdown is bounded by the stall budget");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "a stalled sink never fails the run"
    );
    assert!(!receipt.is_clean());
    assert_eq!(receipt.delivered, 10);
    assert!(receipt.undelivered > 0, "{receipt:?}");
    assert_eq!(receipt.projected, receipt.delivered + receipt.undelivered);
    let failure = receipt.failure.as_deref().expect("the stall is reported");
    assert!(
        failure.contains("stalled") && failure.contains("300ms"),
        "{failure}"
    );
    let replayed = replay_run_dir(dir.path()).await.expect("projects");
    assert_eq!(
        replayed.len() as u64,
        receipt.projected,
        "recovery rebuilds the whole stream"
    );
    let replayed_ids: BTreeSet<EventId> = replayed.iter().map(|e| e.id).collect();
    let delivered = stalled.inner.events();
    assert_eq!(delivered.len(), 10);
    assert!(delivered.iter().all(|e| replayed_ids.contains(&e.id)));
}

/// A sink that keeps returning but cannot keep up with a chatty run leaves
/// the overflow to the durable log: the queue never holds more than its
/// capacity, the run is not slowed, live delivery keeps each source's order
/// with gaps, the receipt counts what was left behind, and `replay_run`
/// completes the stream.
#[tokio::test]
async fn a_sink_that_falls_behind_leaves_the_overflow_to_the_durable_log() {
    let dir = RunDir::new("embed-backlog");
    let rt = runtime(&dir, None);
    let lowered = lower(&rt, &dir, CHATTY);
    let slow = Arc::new(SlowSink {
        inner: CollectingSink::default(),
        delay: Duration::from_millis(5),
    });
    let options = ProjectorOptions {
        capacity: 16,
        ..ProjectorOptions::default()
    };
    let (report, receipt) =
        run_projected_with(&rt, lowered, slow.clone(), Vec::new(), options).await;
    assert_eq!(report.status, RunStatus::Success);
    assert!(receipt.failure.is_none(), "{receipt:?}");
    assert!(
        receipt.overflowed > 0,
        "the producer outran the sink without waiting for it: {receipt:?}"
    );
    assert_eq!(receipt.undelivered, receipt.overflowed);
    assert_eq!(receipt.projected, receipt.delivered + receipt.undelivered);
    let delivered = slow.inner.events();
    assert_eq!(delivered.len() as u64, receipt.delivered);
    assert!(
        delivered.len() >= 16,
        "the queue's worth was delivered at least: {}",
        delivered.len()
    );
    // What the sink saw is a subsequence of each source's stream.
    let mut last: BTreeMap<EventSource, EventId> = BTreeMap::new();
    for event in &delivered {
        if let Some(previous) = last.insert(event.id.source, event.id) {
            assert!(
                previous < event.id,
                "delivery kept the source order: {previous:?} before {:?}",
                event.id
            );
        }
    }
    let replayed = replay_run_dir(dir.path()).await.expect("projects");
    assert_eq!(
        replayed.len() as u64,
        receipt.projected,
        "the log holds every event, delivered or not"
    );
    let replayed_ids: BTreeSet<EventId> = replayed.iter().map(|e| e.id).collect();
    assert!(delivered.iter().all(|e| replayed_ids.contains(&e.id)));
    // The chatty command's lines are the bulk of the overflow.
    let lines = replayed
        .iter()
        .filter(|e| matches!(e.engine(), Some(Event::StepProgressRecorded { .. })))
        .count();
    assert!(lines >= 2000, "{lines} output lines");
}

/// Recovery: a resumed run re-delivers the regenerated suffix with the same
/// identities, deduplication by id yields the full stream, and the run dir
/// projects the same events after the resume.
#[tokio::test]
async fn recovery_redelivers_with_stable_identities() {
    let dir = RunDir::new("embed-recover");
    let rt = runtime(&dir, None);
    let mut lowered = lower(&rt, &dir, WORKFLOW);
    script_flaky(lowered.graph.as_mut().expect("lowers"));
    let first = Arc::new(CollectingSink::default());
    let (report, receipt) = run_projected(&rt, lowered, first.clone(), Vec::new()).await;
    assert_eq!(report.status, RunStatus::Success);
    assert!(receipt.is_clean());
    let complete = first.events();

    // The crash: the root execution's last record never reached disk.
    let events_path = dir.path().join("executions/0000000000000000/events.jsonl");
    let text = fs::read_to_string(&events_path).expect("reads");
    let mut lines: Vec<&str> = text.lines().collect();
    lines.pop();
    fs::write(&events_path, format!("{}\n", lines.join("\n"))).expect("writes");
    // And the run never recorded its finish: cut the coordinator log at
    // the execution's finish record.
    let coordinator = dir.path().join("coordinator.jsonl");
    let text = fs::read_to_string(&coordinator).expect("reads");
    let lines: Vec<&str> = text.lines().collect();
    // The root execution's finish: the branch children finished before it.
    let cut = lines
        .iter()
        .position(|line| line.contains("execution.finished") && line.contains("\"execution\":0"))
        .expect("the root execution finished");
    fs::write(&coordinator, format!("{}\n", lines[..cut].join("\n"))).expect("writes");
    // A run that never recorded its finish never released its sandbox
    // either: forget the release the first run made, so the resume makes
    // it, and records it, where the first run did.
    let resources = dir.path().join("resources.jsonl");
    let text = fs::read_to_string(&resources).expect("reads");
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| {
            !line.contains("\"pending\":\"stop\"") && !line.contains("\"state\":\"stopped\"")
        })
        .collect();
    fs::write(&resources, format!("{}\n", lines.join("\n"))).expect("writes");

    let second = Arc::new(CollectingSink::default());
    let projector = EventProjector::primed_run_dir(second.clone(), dir.path())
        .await
        .expect("primes");
    let rt = runtime(&dir, None);
    let resumed = host::resume_configured(&rt, Vec::new(), vec![projector.clone()], |_, _| {})
        .await
        .expect("resumes");
    assert_eq!(resumed.status, RunStatus::Success);
    let receipt = projector.shutdown().await;
    assert!(receipt.is_clean());
    let redelivered = second.events();
    assert!(
        !redelivered.is_empty(),
        "the regenerated suffix was re-delivered"
    );
    // A re-delivered record was re-recorded by the resume: the same identity
    // and body, and a recording time of its own, since the original never
    // reached disk.
    let complete = normalized(&complete);
    for event in normalized(&redelivered) {
        let original = complete
            .iter()
            .find(|e| e.id == event.id)
            .unwrap_or_else(|| panic!("a re-delivered event has a known identity: {:?}", event.id));
        assert!(
            event.recorded_at >= original.recorded_at,
            "re-recorded no earlier than first recorded: {event:?}"
        );
        assert_eq!(
            timeless(&event),
            timeless(original),
            "a re-delivered event equals the original"
        );
    }
    // Deduplication by id over both deliveries yields the complete stream; the
    // run dir carries the resume's recording time for the re-recorded suffix,
    // so the comparison is over everything but that time.
    let mut merged: BTreeMap<EventId, RunEvent> = BTreeMap::new();
    for event in complete.iter().chain(normalized(&redelivered).iter()) {
        merged.entry(event.id).or_insert_with(|| event.clone());
    }
    let mut replayed = replay_run_dir(dir.path()).await.expect("projects");
    replayed.sort_by_key(|e| e.id);
    assert_eq!(
        merged.values().map(timeless).collect::<Vec<_>>(),
        replayed.iter().map(timeless).collect::<Vec<_>>(),
        "the recovered run projects the same stream"
    );
}

// ── Readiness item 8: the milestone workflow through the embedding boundary ──

/// The intermediate milestone workflow's shape, as the embedding host sees
/// it: `[run.prepare]` steps, real commands, scripted agents, a human
/// decision, a bounded `for_each` fan-out whose results a command consumes,
/// and final file checks. The fake host pauses the fork's admission, accepts
/// the final check's failure as a partial success, and reports a best-effort
/// problem on the join's transition; the run is reconstructed from public
/// events alone and replay yields the same stream.
#[tokio::test]
async fn the_milestone_workflow_runs_through_the_embedding_boundary() {
    const MILESTONE: &str = r#"digraph M {
        start [shape=Mdiamond]
        exit [shape=Msquare]
        plan [shape=box, prompt="Plan the note"]
        edit [shape=parallelogram, script="printf 'reviewed\n' >> notes.txt; echo edited"]
        gate [shape=hexagon, label="Ship?", question_type="yes_no"]
        jobs [shape=parallelogram, output_schema="routing", script="printf '%s' '{\"context_updates\":{\"jobs\":[{\"name\":\"alpha\"},{\"name\":\"beta\"}]}}'"]
        fan [shape=component, for_each="context.jobs", max_parallel=2]
        job [shape=box, prompt="Review the item"]
        join [shape=tripleoctagon]
        report [shape=parallelogram, script="cat > results.json", stdin_source="context.parallel.results"]
        check [shape=parallelogram, script="cat notes.txt results.json; echo unsigned >&2; exit 3", on_failure="exit"]
        hold [shape=parallelogram, script="echo held > held.txt"]
        start -> plan -> edit -> gate
        gate -> jobs [label="[Y] Yes"]
        gate -> hold [label="[N] No"]
        jobs -> fan -> job -> join -> report -> check -> exit
        hold -> exit
    }"#;
    let dir = RunDir::new("embed-milestone");
    fs::write(
        dir.path().join("workflow.toml"),
        "[run.prepare]\n[[run.prepare.steps]]\nscript = \"printf 'draft\\\\n' > notes.txt\"\n",
    )
    .expect("write workflow.toml");
    let host = FakeHost::new(Script {
        pause: Some("fan"),
        accept_failure_of: Some("check"),
        metadata_problem: Some("join"),
        ..Script::default()
    });
    let rt = runtime(&dir, Some(host.clone()));
    let lowered = lower(&rt, &dir, MILESTONE);
    let sink = Arc::new(CollectingSink::default());
    let releaser = host.clone();
    let release = tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(20);
        while releaser.paused.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "the fork was never paused");
            sleep(Duration::from_millis(10)).await;
        }
        sleep(Duration::from_millis(100)).await;
        releaser.release.notify_waiters();
    });
    let (report, receipt) = run_projected(&rt, lowered, sink.clone(), Vec::new()).await;
    release.await.expect("the releaser ran");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(receipt.is_clean(), "{receipt:?}");
    let events = sink.events();
    assert_unique_ids(&events);
    let timeline = Timeline::from_events(&events);
    assert_eq!(timeline.run_status, Some(RunStatus::Success));

    // Setup ran as a stage before the nodes; the agents, the command edit,
    // the decision and the fan-out all left their marks.
    assert_eq!(timeline.node("run_prepare_1").kind, "command");
    assert_eq!(
        timeline.node("run_prepare_1").final_status.as_deref(),
        Some("success")
    );
    assert_eq!(timeline.node("plan").kind, "agent");
    assert_eq!(timeline.node("gate").answers, vec!["Y".to_owned()]);
    assert!(
        timeline
            .node("gate")
            .waits
            .contains(&WaitState::AwaitingAnswer),
        "{:?}",
        timeline.node("gate").waits
    );
    assert!(
        !timeline.nodes.contains_key("hold"),
        "the refused route never ran"
    );
    // The root and the two branch children, each its own invocation.
    assert_eq!(timeline.invocations, BTreeSet::from([0, 1, 2]));
    assert_eq!(
        timeline.node("join").final_status.as_deref(),
        Some("success")
    );
    assert_eq!(
        timeline.node("report").final_status.as_deref(),
        Some("success")
    );

    // Awaited admission: the fork's visit existed while paused and no attempt
    // had started; once released it ran once.
    assert_eq!(host.paused.load(Ordering::SeqCst), 1);
    assert_eq!(timeline.node("fan").visits, 1);
    assert_eq!(timeline.node("fan").attempts.len(), 1);

    // The `for_each` fan-out is a fork on the typed stream, with the same
    // identities a static fork has: `fork_started` on the parallel node once
    // the expansion knows its two items, each clone a member of its item's
    // branch, `branch_completed` per clone and `fork_completed` at the
    // fan-in in item order.
    assert_eq!(timeline.node("fan").branch.as_deref(), Some("fork:2"));
    assert_eq!(timeline.node("join").branch.as_deref(), Some("join"));
    assert_eq!(timeline.forks, vec![("parallel".to_owned(), 2)]);
    assert_eq!(timeline.joins, vec![("fan".to_owned(), vec![
        (0, "job#0".to_owned(), "success".to_owned()),
        (1, "job#1".to_owned(), "success".to_owned()),
    ])]);

    // Result preparation: the check's failure became a partial success with
    // the original evidence recorded beside it, and the run went on.
    let check = timeline.node("check");
    assert_eq!(check.final_status.as_deref(), Some("partial_success"));
    let evidence = bodies_of(&events, "check")
        .into_iter()
        .find_map(|b| note_of(b, RESULT_PREPARED_KIND))
        .expect("the original evidence is recorded");
    assert_eq!(
        evidence["original"]["failure"]["class"],
        json!("exit_status:3")
    );

    // Transition ordering, per completed node, from the durable notes.
    for node in ["run_prepare_1", "plan", "edit", "gate", "jobs", "report"] {
        assert_eq!(
            timeline.node(node).notes,
            vec![
                "fake_host:before_attempt",
                "fake_host:prepare_result",
                "fake_host:after_record",
                "fake_host:transition",
            ],
            "{node}"
        );
    }
    // The adjusted node carries the original evidence between preparation
    // and the record.
    assert_eq!(check.notes, vec![
        "fake_host:before_attempt",
        "fake_host:prepare_result",
        "result_prepared:",
        "fake_host:after_record",
        "fake_host:transition",
    ]);
    // The join's best-effort metadata problem was recorded and did not stop
    // the run.
    assert!(
        bodies_of(&events, "join").iter().any(|b| {
            note_of(b, TRANSITION_KIND).is_some_and(|payload| {
                payload["problems"]
                    .as_array()
                    .is_some_and(|p| !p.is_empty())
            })
        }),
        "{:#?}",
        bodies_of(&events, "join")
    );

    // The files: setup, the edit, and the consumed branch results.
    let ws = workspace(&dir);
    assert_eq!(
        fs::read_to_string(ws.join("notes.txt")).expect("notes.txt"),
        "draft\nreviewed\n"
    );
    let results: Value =
        serde_json::from_str(&fs::read_to_string(ws.join("results.json")).expect("results.json"))
            .expect("results.json is JSON");
    let results = results.as_array().expect("a list");
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0]["item_label"], json!("alpha"));
    assert_eq!(results[1]["item_label"], json!("beta"));
    assert!(results.iter().all(|r| r["status"] == json!("succeeded")));
    assert!(!ws.join("held.txt").exists());

    // Replay yields the same public stream, identity for identity.
    let mut replayed = replay_run_dir(dir.path()).await.expect("replays");
    replayed.sort_by_key(|e| e.id);
    assert_eq!(replayed, normalized(&events));
}
