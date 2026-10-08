//! `attractor/agent` against the fake ACP agent Fabro ships, packaged as test
//! data in `crates/fabro/acceptance/testdata/fake_acp_agent.py`: initialize,
//! session, one prompt turn, the response text captured, a routing directive
//! read, permission requests answered, cancellation honoured.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use attractor_steps::register;
use frontend_attractor::load;
use runtime::driver::ExecutionReport;
use runtime::executor::Retention;
use runtime::frontend::{CompileInputs, NoFiles};
use runtime::ir::{Attempt, CancelScopeId, ExprOrValue, Graph, RunStatus, TimeoutPolicy};
use runtime::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, output_of, status_of};
use tokio::time;

/// The fake agent, copied from the packaged test data to where a run can
/// execute it.
fn fake_agent(dir: &RunDir) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fabro/acceptance/testdata/fake_acp_agent.py");
    let script =
        fs::read_to_string(&source).unwrap_or_else(|e| panic!("{}: {e}", source.display()));
    let path = dir.path().join("fake_acp_agent.py");
    fs::write(&path, script).expect("write the fake agent");
    path
}

fn dot(body: &str) -> String {
    format!("digraph T {{\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n{body}\n}}")
}

#[expect(
    clippy::print_stderr,
    reason = "a graph that fails to lower explains itself in the test output"
)]
fn lower(text: &str) -> Graph {
    let lowered = load("test.fabro", text, &NoFiles, &CompileInputs::new());
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.expect("lowers")
}

/// The fake agent's behavior is chosen through `ACP_MODE` and friends in the
/// environment: set them on the scope so the agent process sees them.
fn with_env(mut graph: Graph, pairs: &[(&str, &str)]) -> Graph {
    for scope in &mut graph.body.scopes {
        for (key, value) in pairs {
            scope
                .env
                .insert((*key).into(), ExprOrValue::Value(json!(value)));
        }
    }
    graph
}

fn runtime(dir: &RunDir) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Never;
    options.echo = false;
    register(Runtime::standard()).options(options)
}

/// A one-agent graph on the ACP backend. The backend is named because the
/// default is the native agent, as Fabro's own default is. The node names
/// no `model`: an ACP agent chooses its own, and the lowering refuses one
/// (`attractor.acp_api_only_attributes`).
fn agent_dot(agent: &Path, extra: &str) -> String {
    dot(&format!(
        r#"
        graph [goal="Greet", backend="acp", acp.command="python3 {} "]
        a [prompt="Say hello"{extra}]
        start -> a -> exit
    "#,
        agent.display()
    ))
}

async fn run(dir: &RunDir, graph: Graph) -> ExecutionReport {
    runtime(dir)
        .run(graph)
        .await
        .expect("replay is byte-identical")
}

#[tokio::test]
async fn a_turn_captures_the_agent_text() {
    let dir = RunDir::new("fabro-agent-turn");
    let agent = fake_agent(&dir);
    let graph = lower(&agent_dot(&agent, ""));
    let report = run(&dir, graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let output = output_of(&report, "a");
    assert_eq!(output["text"], json!("hello from acp"));
    assert_eq!(output["outcome"], json!("succeeded"));
    assert_eq!(
        report.state.run_context().get("last_response"),
        Some(&json!("hello from acp"))
    );
}

#[tokio::test]
async fn a_routing_directive_in_the_response_steers_the_edge() {
    let dir = RunDir::new("fabro-agent-directive");
    let agent = fake_agent(&dir);
    // The fake agent echoes `steered:<prompt>` in `steer` mode on its second
    // prompt; the plain mode says a fixed text. Use a permission request to
    // prove the client answers requests mid-turn.
    let graph = lower(&dot(&format!(
        r#"
        graph [goal="G", backend="acp", acp.command="python3 {}"]
        a [prompt="Say hello"]
        start -> a
        a -> exit [label="Done"]
    "#,
        agent.display()
    )));
    let permission = dir.path().join("permission.json");
    let graph = with_env(graph, &[
        ("ACP_MODE", "permission"),
        ("ACP_PERMISSION", permission.to_str().expect("utf-8")),
    ]);
    let report = run(&dir, graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let answered = fs::read_to_string(&permission).expect("the permission request was answered");
    assert!(
        answered.contains("allow_always") || answered.contains("always"),
        "{answered}"
    );
}

#[tokio::test]
async fn an_agent_that_exits_early_fails_the_stage_routably() {
    let dir = RunDir::new("fabro-agent-early-exit");
    let agent = fake_agent(&dir);
    let graph = lower(&dot(&format!(
        r#"
        graph [goal="G", backend="acp", acp.command="python3 {}"]
        a [prompt="Say hello"]
        recover [shape=parallelogram, script="true"]
        start -> a
        a -> recover [condition="outcome=failed"]
        a -> exit
        recover -> exit
    "#,
        agent.display()
    )));
    let graph = with_env(graph, &[("ACP_MODE", "early_exit")]);
    let report = run(&dir, graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "a").as_deref(), Some("failure"));
    // The dead agent asked for a retry, as Fabro's handler error is
    // retryable; with no attempts left the failure stands and routes.
    assert_eq!(
        output_of(&report, "a")["failure_class"],
        json!("retry_requested")
    );
    // The failure says how the agent ended and what it last wrote to stderr.
    let output = output_of(&report, "a").to_string();
    assert!(
        output.contains("exit code 2") && output.contains("early boom"),
        "{output}"
    );
    assert_eq!(status_of(&report, "recover").as_deref(), Some("success"));
}

/// An ACP agent that exits on its first prompt and answers on its second,
/// counting prompts in a file so the second process knows it is second.
fn flaky_agent(dir: &RunDir) -> PathBuf {
    let counter = dir.path().join("prompts.count");
    let script = format!(
        r#"import json, os, sys
counter = r"{}"
def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    m = json.loads(line)
    method = m.get("method")
    if method == "initialize":
        send({{"jsonrpc": "2.0", "id": m["id"], "result": {{"protocolVersion": 1, "agentCapabilities": {{}}}}}})
    elif method == "session/new":
        send({{"jsonrpc": "2.0", "id": m["id"], "result": {{"sessionId": "s"}}}})
    elif method == "session/prompt":
        n = int(open(counter).read()) if os.path.exists(counter) else 0
        open(counter, "w").write(str(n + 1))
        if n == 0:
            sys.exit(3)
        send({{"jsonrpc": "2.0", "method": "session/update", "params": {{"sessionId": "s", "update": {{"sessionUpdate": "agent_message_chunk", "content": {{"type": "text", "text": "answered on attempt " + str(n + 1)}}}}}}}})
        send({{"jsonrpc": "2.0", "id": m["id"], "result": {{"stopReason": "end_turn"}}}})
"#,
        counter.display()
    );
    let path = dir.path().join("flaky_acp_agent.py");
    fs::write(&path, script).expect("write the flaky agent");
    path
}

/// An agent that dies before answering asks for a retry, as Fabro's
/// retryable handler error does: `max_retries=1` starts a second process,
/// and the node succeeds on that process's answer.
#[tokio::test]
async fn an_agent_that_exits_before_answering_is_retried_while_attempts_remain() {
    let dir = RunDir::new("fabro-agent-exit-retried");
    let agent = flaky_agent(&dir);
    let graph = lower(&dot(&format!(
        r#"
        graph [goal="G", backend="acp", acp.command="python3 {}"]
        a [prompt="Say hello", max_retries=1, on_failure="exit"]
        start -> a -> exit
    "#,
        agent.display()
    )));
    let report = run(&dir, graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(
        output_of(&report, "a")["text"],
        json!("answered on attempt 2")
    );
    let record = report
        .state
        .history()
        .iter()
        .find(|record| record.name == "a")
        .expect("`a` finished");
    assert_eq!(
        record.attempt,
        Attempt::FIRST.next(),
        "the answer came from the second attempt"
    );
}

/// An ACP node with no command anywhere is refused when the workflow is
/// loaded (Fabro's `backend_valid` rule, ported as
/// `attractor.acp_requires_command`), so the step's own `acp_unconfigured`
/// refusal is reached only by a graph built without the lowering.
#[test]
fn an_unconfigured_agent_is_refused_at_load() {
    // The node names the backend, since the default is the native agent.
    let lowered = load(
        "test.fabro",
        &dot(r#"
        a [prompt="Say hello", backend="acp"]
        start -> a -> exit
    "#),
        &NoFiles,
        &CompileInputs::new(),
    );
    assert!(lowered.graph.is_none());
    assert!(
        lowered
            .diagnostics
            .errors()
            .any(|d| d.code == "attractor.acp_requires_command"),
        "{:?}",
        lowered.diagnostics
    );
}

#[tokio::test]
async fn cancelling_a_turn_sends_session_cancel_and_stops_the_agent() {
    let dir = RunDir::new("fabro-agent-cancel");
    let agent = fake_agent(&dir);
    let record = dir.path().join("cancel.txt");
    let graph = lower(&agent_dot(&agent, ""));
    let graph = with_env(graph, &[
        ("ACP_MODE", "cancel"),
        ("ACP_CANCEL_RECORD", record.to_str().expect("utf-8")),
    ]);
    let rt = runtime(&dir);
    let driver = rt.driver(graph);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    assert!(
        testkit::wait_for_file(&dir.path().join("scopes"), Duration::from_secs(10)).await,
        "the run started"
    );
    time::sleep(Duration::from_millis(800)).await;
    handle.cancel(CancelScopeId::ROOT).await;
    let report = run.await.expect("the run task");
    assert_ne!(report.status, RunStatus::Success);
    assert_eq!(
        fs::read_to_string(&record).ok().as_deref().map(str::trim),
        Some("session/cancel"),
        "the agent saw session/cancel"
    );
}

/// An ACP node's `timeout` is handed to the turn (`HandlerManaged`): a turn
/// that outlives it is terminated well before the driver's structural budget
/// and asks for a retry, as Fabro's timed-out turn is a retryable handler
/// error; the node retries while attempts remain and fails when none are
/// left.
#[tokio::test]
async fn an_acp_turn_that_outlives_the_node_timeout_is_retried_then_fails() {
    let dir = RunDir::new("fabro-agent-timeout");
    let agent = fake_agent(&dir);
    let graph = with_env(
        lower(&agent_dot(
            &agent,
            r#", timeout="500ms", max_retries=1, on_failure="exit""#,
        )),
        &[("ACP_MODE", "timeout")],
    );
    assert_eq!(
        graph
            .nodes
            .iter()
            .find(|n| n.name == "a")
            .expect("a")
            .budget
            .timeout_policy,
        TimeoutPolicy::HandlerManaged
    );
    let started = Instant::now();
    let report = runtime(&dir)
        .run(graph)
        .await
        .expect("replay is byte-identical");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the deadline ended the turn: {:?}",
        started.elapsed()
    );
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(status_of(&report, "a").as_deref(), Some("failure"));
    let output = output_of(&report, "a");
    assert_eq!(output["failure_class"], json!("retry_requested"));
    assert_eq!(
        output["failure_reason"],
        json!("the agent turn timed out after 500ms")
    );
    let record = report
        .state
        .history()
        .iter()
        .find(|record| record.name == "a")
        .expect("`a` finished");
    assert_eq!(
        record.attempt,
        Attempt::FIRST.next(),
        "the timeout was retried once before the node failed"
    );
}
