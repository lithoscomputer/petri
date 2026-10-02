//! The manager loop (`attractor/workflow`) under a controlled clock, against a
//! fake invocation client: one child per manager attempt, reattached on a
//! re-dispatch of the same attempt; Fabro's poll interval and `max_cycles`
//! defaults; the stop condition evaluated at each poll against the parent
//! context with a reference success outcome; a satisfied condition cancels
//! the child and succeeds; poll exhaustion cancels it and fails; a child
//! that completes first returns its status, its failure and its filtered
//! context updates; a parent cancel cancels the child.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use attractor_steps::workflow::{
    ChildInvoker, DEFAULT_MAX_CYCLES, DEFAULT_POLL_INTERVAL_MS, WorkflowConfig,
};
use attractor_steps::{WORKFLOW_KIND, WorkflowStep};
use execution::{
    CallSite, CancelRequest, ExecutionId, GraphDigest, InvocationClient, InvocationHandle,
    InvocationId, InvocationRequest, InvocationResult, InvocationStatus, InvokeError,
};
use executor::{EnvError, ExecEnv, MapSecrets, ProcessHandle, ProcessSpec};
use ir::{Attempt, Control, FailureInfo, FiringId, Outcome, RunStatus, ScopeId, Status, Value};
use serde_json::json;
use smol_str::SmolStr;
use steps::{Capabilities, ProgressSender, Step, StepCtx};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time;

/// One child the fake client declared.
struct Child {
    request: InvocationRequest,
    status:  watch::Sender<InvocationStatus>,
}

/// A client that records every start and hands back a handle over a status
/// channel the test drives. A repeated request at one call site attaches to
/// the child it declared, as the coordinator does.
#[derive(Default)]
struct FakeClient {
    children:  Mutex<Vec<Child>>,
    cancelled: Mutex<Vec<InvocationId>>,
    cancel_tx: Mutex<Option<mpsc::UnboundedSender<CancelRequest>>>,
}

impl FakeClient {
    fn new() -> Arc<Self> {
        let client = Arc::new(Self::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        *client.cancel_tx.lock().expect("not poisoned") = Some(tx);
        let sink = client.clone();
        tokio::spawn(async move {
            while let Some(id) = rx.recv().await {
                sink.cancelled
                    .lock()
                    .expect("not poisoned")
                    .push(id.invocation);
            }
        });
        client
    }

    fn starts(&self) -> usize {
        self.children.lock().expect("not poisoned").len()
    }

    fn slot(&self, index: usize) -> CallSite {
        self.children.lock().expect("not poisoned")[index]
            .request
            .site
            .clone()
    }

    fn context(&self, index: usize) -> BTreeMap<SmolStr, Value> {
        self.children.lock().expect("not poisoned")[index]
            .request
            .context
            .clone()
    }

    fn finish(&self, index: usize, result: InvocationResult) {
        let children = self.children.lock().expect("not poisoned");
        let _ = children[index]
            .status
            .send(InvocationStatus::Finished(result));
    }

    async fn cancelled(&self) -> Vec<InvocationId> {
        // The cancel rides a channel; let the sink task drain it.
        time::sleep(Duration::from_millis(1)).await;
        self.cancelled.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl InvocationClient for FakeClient {
    async fn start_or_attach(
        &self,
        request: InvocationRequest,
    ) -> Result<InvocationHandle, InvokeError> {
        let mut children = self.children.lock().expect("not poisoned");
        let cancel = self
            .cancel_tx
            .lock()
            .expect("not poisoned")
            .clone()
            .expect("the cancel sink is installed");
        if let Some((index, child)) = children
            .iter()
            .enumerate()
            .find(|(_, child)| child.request.site == request.site)
        {
            if child.request != request {
                return Err(InvokeError::RequestMismatch);
            }
            return Ok(InvocationHandle::new(
                InvocationId::new(index as u64 + 1),
                child.status.subscribe(),
                cancel,
            ));
        }
        let (status, receiver) = watch::channel(InvocationStatus::Declared);
        children.push(Child { request, status });
        Ok(InvocationHandle::new(
            InvocationId::new(children.len() as u64),
            receiver,
            cancel,
        ))
    }
}

/// An environment no step process ever reaches: the manager loop runs no
/// process of its own.
struct NoEnv;

#[async_trait]
impl ExecEnv for NoEnv {
    async fn spawn(&self, _spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        Err(EnvError::backend("test", "spawn", "no processes here"))
    }

    fn workspace_path(&self) -> &'static str {
        "/work"
    }

    async fn read_file(&self, _relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
        Ok(None)
    }

    async fn write_file(&self, _relative: &Path, _contents: &[u8]) -> Result<(), EnvError> {
        Ok(())
    }

    fn grace(&self) -> Duration {
        Duration::from_millis(10)
    }
}

fn digest() -> GraphDigest {
    GraphDigest::from_bytes([7; 32])
}

fn config(extra: Value) -> Value {
    let mut config = json!({
        "label": "Manager",
        "node": "manager",
        "child_digest": digest().to_hex(),
        "kv": { "n": "0", "response.plan": "hidden", "internal.x": 1 },
    });
    if let Value::Object(extra) = extra {
        config.as_object_mut().expect("object").extend(extra);
    }
    config
}

struct Fixture {
    client:  Arc<FakeClient>,
    control: mpsc::Sender<Control>,
}

/// Run the step on a paused clock with `attempt`, returning its outcome.
async fn run(
    client: Arc<FakeClient>,
    attempt: Attempt,
    config: Value,
) -> (JoinHandle<Outcome>, Fixture) {
    let (logs, mut log_rx) = ProgressSender::channel(64);
    tokio::spawn(async move { while log_rx.recv().await.is_some() {} });
    let (control_tx, control) = mpsc::channel(8);
    let invoker: Arc<dyn InvocationClient> = client.clone();
    let ctx = StepCtx {
        firing: FiringId::new(4),
        attempt,
        max_attempts: NonZeroU32::new(3).expect("non-zero"),
        scope: ScopeId::new(0),
        environment: SmolStr::new("scope-0"),
        node: SmolStr::new("manager"),
        config: config.clone(),
        env: Arc::new(NoEnv),
        runner: None,
        secrets: Arc::new(MapSecrets::empty()),
        caps: Capabilities::builder()
            .provide(ChildInvoker(invoker))
            .build(),
        logs,
        control,
    };
    let typed: WorkflowConfig = serde_json::from_value(config).expect("the config deserializes");
    let task = tokio::spawn(async move { WorkflowStep.run(typed, ctx).await });
    // Let the step reach its first poll.
    time::sleep(Duration::from_millis(1)).await;
    (task, Fixture {
        client,
        control: control_tx,
    })
}

fn finished(status: RunStatus, context: &[(&str, Value)]) -> InvocationResult {
    InvocationResult {
        status,
        failure: (status == RunStatus::Failed).then(|| {
            FailureInfo::new("child boom").with_class(ir::FailureClass::new("exit_status:2"))
        }),
        final_execution: ExecutionId::new(9),
        output: json!({ "child": true }),
        context: context
            .iter()
            .map(|(k, v)| (SmolStr::new(k), v.clone()))
            .collect(),
        updates: BTreeMap::new(),
    }
}

#[test]
fn the_step_registers_under_the_workflow_kind() {
    assert_eq!(WORKFLOW_KIND.as_str(), "attractor/workflow");
    assert_eq!(DEFAULT_POLL_INTERVAL_MS, 45_000);
    assert_eq!(DEFAULT_MAX_CYCLES, 1000);
}

#[tokio::test(start_paused = true)]
async fn a_child_that_completes_before_a_poll_returns_its_status_and_filtered_context() {
    let client = FakeClient::new();
    let (task, fixture) = run(client.clone(), Attempt::FIRST, config(json!({}))).await;
    assert_eq!(fixture.client.starts(), 1, "one child, started at once");
    assert_eq!(fixture.client.slot(0), CallSite {
        firing:  FiringId::new(4),
        attempt: Attempt::FIRST,
        slot:    SmolStr::new("manager"),
    });
    let context = fixture.client.context(0);
    assert_eq!(context.get("n"), Some(&json!("0")));
    assert!(
        !context.contains_key("response.plan") && !context.contains_key("internal.x"),
        "bookkeeping keys stay with the parent: {context:?}"
    );
    fixture.client.finish(
        0,
        finished(RunStatus::Success, &[
            ("n", json!("3")),
            ("thread.id", json!("t")),
            ("current.stage", json!("x")),
            ("graph.goal", json!("g")),
            ("done", json!(true)),
        ]),
    );
    let outcome = task.await.expect("the step finishes");
    assert_eq!(outcome.status, Status::Success, "{outcome:?}");
    assert_eq!(outcome.output["cycles"], json!(1));
    assert_eq!(outcome.output["notes"], json!("Child completed at cycle 1"));
    assert_eq!(outcome.context_updates.get("n"), Some(&json!("3")));
    assert_eq!(outcome.context_updates.get("done"), Some(&json!(true)));
    for hidden in ["thread.id", "current.stage", "graph.goal"] {
        assert!(
            !outcome.context_updates.contains_key(hidden),
            "{hidden} is engine-internal in Fabro: {:?}",
            outcome.context_updates
        );
    }
    assert!(fixture.client.cancelled().await.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_thousand_polls_consume_one_child_then_exhaustion_cancels_it() {
    let client = FakeClient::new();
    let (task, fixture) = run(client.clone(), Attempt::FIRST, config(json!({}))).await;
    // The default: 1000 polls of 45 seconds, one child.
    for _ in 0..999 {
        time::advance(Duration::from_millis(DEFAULT_POLL_INTERVAL_MS)).await;
        assert!(!task.is_finished(), "still polling");
    }
    time::advance(Duration::from_millis(DEFAULT_POLL_INTERVAL_MS)).await;
    let outcome = task.await.expect("the step finishes");
    let Status::Failure(info) = &outcome.status else {
        panic!("exhaustion fails the node: {outcome:?}");
    };
    assert!(
        info.message.contains("Max cycles (1000) exceeded"),
        "{info:?}"
    );
    assert_eq!(info.class.as_str(), "max_cycles");
    assert_eq!(outcome.output["cycles"], json!(1000));
    assert_eq!(
        fixture.client.starts(),
        1,
        "1000 polls, one child invocation"
    );
    assert_eq!(fixture.client.cancelled().await, [InvocationId::new(1)]);
}

#[tokio::test(start_paused = true)]
async fn the_stop_condition_reads_the_parent_context_and_cancels_the_child() {
    let client = FakeClient::new();
    // `outcome=succeeded` is Fabro's reference success outcome at a poll;
    // `n` is the parent's value, not the child's progress.
    let (task, fixture) = run(
        client.clone(),
        Attempt::FIRST,
        config(json!({
            "stop_condition": "outcome=succeeded && context.n=0",
            "poll_interval_ms": 1000,
            "max_cycles": 5,
        })),
    )
    .await;
    assert!(
        !task.is_finished(),
        "the condition is evaluated at a poll, not at start"
    );
    time::advance(Duration::from_secs(1)).await;
    let outcome = task.await.expect("the step finishes");
    assert_eq!(outcome.status, Status::Success, "{outcome:?}");
    assert_eq!(
        outcome.output["notes"],
        json!("Stop condition satisfied at cycle 1")
    );
    let child_keys: Vec<&SmolStr> = outcome
        .context_updates
        .keys()
        .filter(|key| key.as_str() != "failure_class")
        .collect();
    assert!(
        child_keys.is_empty(),
        "a stop-condition exit carries no child updates: {child_keys:?}"
    );
    assert_eq!(fixture.client.cancelled().await, [InvocationId::new(1)]);

    // A condition the parent context never satisfies polls on.
    let client = FakeClient::new();
    let (task, fixture) = run(
        client.clone(),
        Attempt::FIRST,
        config(json!({
            "stop_condition": "context.n=3",
            "poll_interval_ms": 1000,
            "max_cycles": 3,
        })),
    )
    .await;
    // Each elapsed poll needs a turn of the loop before the next advance.
    for _ in 0..2 {
        time::advance(Duration::from_secs(1)).await;
        time::sleep(Duration::from_millis(1)).await;
    }
    assert!(!task.is_finished(), "two polls of one child");
    fixture
        .client
        .finish(0, finished(RunStatus::Success, &[("n", json!("3"))]));
    let outcome = task.await.expect("the step finishes");
    assert_eq!(outcome.status, Status::Success);
    assert_eq!(outcome.output["cycles"], json!(3));
    assert_eq!(fixture.client.starts(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_failing_child_fails_the_node_with_its_failure_and_updates() {
    let client = FakeClient::new();
    let (task, fixture) = run(client.clone(), Attempt::FIRST, config(json!({}))).await;
    fixture.client.finish(
        0,
        finished(RunStatus::Failed, &[("partial", json!("kept"))]),
    );
    let outcome = task.await.expect("the step finishes");
    let Status::Failure(info) = &outcome.status else {
        panic!("{outcome:?}");
    };
    assert_eq!(info.message, "child boom");
    assert_eq!(info.class.as_str(), "exit_status:2");
    assert_eq!(outcome.output["failure_class"], json!("exit_status:2"));
    assert_eq!(
        outcome.context_updates.get("partial"),
        Some(&json!("kept")),
        "a failed child's context changes still reach the parent"
    );
}

#[tokio::test(start_paused = true)]
async fn max_cycles_normalizes_as_fabro_does() {
    // Zero polls once.
    let client = FakeClient::new();
    let (task, fixture) = run(
        client.clone(),
        Attempt::FIRST,
        config(json!({ "max_cycles": 0, "poll_interval_ms": 10 })),
    )
    .await;
    time::advance(Duration::from_millis(10)).await;
    let outcome = task.await.expect("the step finishes");
    assert!(matches!(outcome.status, Status::Failure(_)));
    assert_eq!(outcome.output["cycles"], json!(1));
    assert_eq!(fixture.client.starts(), 1);

    // Missing, non-integer and negative lower to 1000 in the frontend.
    let graph = |attr: &str| {
        let text = format!(
            r#"digraph T {{
                start [shape=Mdiamond]
                exit [shape=Msquare]
                m [shape=house, stack.child_dot_source="digraph C {{ start [shape=Mdiamond] exit [shape=Msquare] start -> exit }}"{attr}]
                start -> m -> exit
            }}"#
        );
        let lowered = frontend_attractor::load_text("m.fabro", &text);
        let graph = lowered
            .graph
            .unwrap_or_else(|| panic!("{:?}", lowered.diagnostics));
        let codes: Vec<String> = lowered
            .diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect();
        let node = graph
            .nodes
            .iter()
            .find(|n| n.name == "m")
            .expect("m")
            .step
            .config
            .clone();
        (node, codes)
    };
    assert_eq!(graph("").0["max_cycles"], json!(1000));
    assert_eq!(graph(", manager.max_cycles=0").0["max_cycles"], json!(1));
    assert_eq!(graph(", manager.max_cycles=7").0["max_cycles"], json!(7));
    let (negative, codes) = graph(", manager.max_cycles=-4");
    assert_eq!(negative["max_cycles"], json!(1000));
    assert!(
        codes.contains(&"attractor.manager.max_cycles".to_string()),
        "{codes:?}"
    );
    let (text, codes) = graph(r#", manager.max_cycles="many""#);
    assert_eq!(text["max_cycles"], json!(1000));
    assert!(
        codes.contains(&"attractor.manager.max_cycles".to_string()),
        "{codes:?}"
    );
    assert!(
        graph("").0.get("poll_interval_ms").is_none(),
        "the 45 s default lives in the step"
    );
}

#[tokio::test(start_paused = true)]
async fn a_redispatched_attempt_reattaches_and_a_new_attempt_starts_a_new_child() {
    let client = FakeClient::new();
    let (first, _) = run(client.clone(), Attempt::FIRST, config(json!({}))).await;
    first.abort();
    let _ = first.await;
    assert_eq!(client.starts(), 1);
    // The same attempt again, as after a crash and resume: the same call
    // site, so the client attaches and no second child starts.
    let (again, fixture) = run(client.clone(), Attempt::FIRST, config(json!({}))).await;
    assert_eq!(fixture.client.starts(), 1, "reattached, not restarted");
    fixture.client.finish(0, finished(RunStatus::Success, &[]));
    let outcome = again.await.expect("the step finishes");
    assert_eq!(outcome.status, Status::Success);
    // A later attempt of the same firing is a fresh child.
    let (later, fixture) = run(client.clone(), Attempt::FIRST.next(), config(json!({}))).await;
    assert_eq!(
        fixture.client.starts(),
        2,
        "a new attempt starts a new child"
    );
    assert_eq!(fixture.client.slot(1).attempt, Attempt::FIRST.next());
    fixture.client.finish(1, finished(RunStatus::Success, &[]));
    assert_eq!(later.await.expect("finishes").status, Status::Success);
}

#[tokio::test(start_paused = true)]
async fn a_parent_cancel_cancels_the_child() {
    let client = FakeClient::new();
    let (task, fixture) = run(client.clone(), Attempt::FIRST, config(json!({}))).await;
    fixture
        .control
        .send(Control::Deliver(json!("steer")))
        .await
        .expect("delivered");
    time::sleep(Duration::from_millis(1)).await;
    assert!(!task.is_finished(), "a delivery does not end the wait");
    fixture
        .control
        .send(Control::Cancel)
        .await
        .expect("cancelled");
    let outcome = task.await.expect("the step finishes");
    assert_eq!(outcome.status, Status::Cancelled);
    assert_eq!(fixture.client.cancelled().await, [InvocationId::new(1)]);
}
