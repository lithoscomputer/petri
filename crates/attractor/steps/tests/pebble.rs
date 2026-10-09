//! Native sessions against a scripted model and real Petri execution scopes.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use attractor_steps::pebble::PebbleClient;
use attractor_steps::pebble::environment::PebbleEnvironment;
use attractor_steps::register;
use frontend::Diagnostics;
use frontend_attractor::RunSettings;
use frontend_attractor::mcps::{DEFAULT_TOOL_TIMEOUT_MS, McpServer, McpTransport};
use ir::{CancelScopeId, Graph, RunStatus, ScopeId};
use lithos_llm::types::ReasoningEffort;
use pebble_coding_agent::environment::{Environment, ExecRequest};
use pebble_coding_agent::test_support::{
    EnvironmentContract, ScriptedCall, scripted_client, text_response, tool_call_response,
};
use runtime::driver::{
    DeliverDisposition, EventObserver, ExecutionReport, ObserveError, RunHandle,
};
use runtime::engine::{EngineState, Event, EventRecord, ReplayMismatch};
use runtime::executor::sandbox::HostExecutor;
use runtime::executor::{AcquireContext, Executor, MapSecrets, Retention, ScopeOutcome, ScopeSpec};
use runtime::frontend::{CompileInputs, NoFiles};
use runtime::steps::{Answer, Question};
use runtime::{RunOptions, Runtime};
use serde_json::{Value, json};
use smol_str::SmolStr;
use testkit::{RunDir, output_of};
use tokio::fs;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

fn graph(extra: &str) -> Graph {
    let source = format!(
        r#"digraph T {{
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        a [prompt="Make the change and verify it" {extra}]
        exit [shape=Msquare]
        start -> a -> exit
    }}"#
    );
    let lowered = frontend_attractor::load("test.fabro", &source, &NoFiles, &CompileInputs::new());
    lowered.graph.expect("valid workflow")
}

fn runtime(dir: &RunDir, client: lithos_llm::Client) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(100);
    options.retention = Retention::Never;
    options.echo = false;
    register(Runtime::standard())
        .capability(PebbleClient(client))
        .options(options)
}

fn metrics(report: &ExecutionReport) -> &BTreeMap<SmolStr, Value> {
    &report
        .state
        .history()
        .iter()
        .find(|row| row.name == "a")
        .expect("agent outcome")
        .outcome
        .metrics
        .custom
}

#[tokio::test]
async fn environment_meets_the_pebble_contract() {
    let dir = RunDir::new("pebble-contract");
    let executor = HostExecutor::new(dir.path());
    let scope = ScopeSpec::new(ScopeId::new(1), "pebble").with_grace(Duration::from_millis(50));
    let handle = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("scope");
    let environment = PebbleEnvironment::prepare(
        handle.exec(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("prepare");
    let contract = EnvironmentContract::new(&environment, "contract");
    contract.verify_files().await.expect("files");
    contract.verify_search().await.expect("search");
    contract.verify_commands().await.expect("commands");
    let path = "odd 'name; $(touch injected)\nfile";
    environment
        .write_file(path, "safe")
        .await
        .expect("write odd path");
    environment
        .rename_file(path, "moved 'name")
        .await
        .expect("move odd path");
    assert_eq!(
        environment
            .read_file_text("moved 'name")
            .await
            .expect("read"),
        "safe"
    );
    assert!(!environment.file_exists("injected").await.expect("exists"));
    assert!(
        executor
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
}

#[tokio::test]
async fn native_tools_edit_and_verify_in_the_scope() {
    let dir = RunDir::new("pebble-edit-test");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "write_file",
            "write",
            json!({"path":"answer.txt","content":"42\n"}),
        )),
        ScriptedCall::response(tool_call_response(
            "shell",
            "verify",
            json!({"command":"test \"$(cat answer.txt)\" = 42 && printf verified"}),
        )),
        ScriptedCall::response(text_response("Done and verified.")),
    ]);
    let report = runtime(&dir, client).run(graph("")).await.expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "Done and verified.");
    assert_eq!(provider.requests().len(), 3);
    let requests = serde_json::to_string(&provider.requests()).expect("requests");
    assert!(requests.contains("verified"), "{requests}");
    assert_eq!(metrics(&report)["pebble.usage"]["tokens"]["input"], 30);
    assert_eq!(metrics(&report)["pebble.usage"]["tokens"]["output"], 15);
    // One route, so one entry: all of the session's usage, on its route.
    assert_eq!(
        metrics(&report)["pebble.usage_by_model"],
        json!([{
            "provider": "test",
            "model": "model",
            "usage": metrics(&report)["pebble.usage"],
        }])
    );
}

/// The node's step config, as the lowering wrote it.
fn node_config<'a>(graph: &'a mut Graph, name: &str) -> &'a mut Value {
    &mut graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("node `{name}`"))
        .step
        .config
}

#[tokio::test]
async fn tool_shells_see_workflow_secrets_beneath_their_own_env() {
    let dir = RunDir::new("pebble-workflow-secrets");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "shell",
            "check",
            json!({"command":"test \"$AGENT_KEY\" = workflow-secret-value && echo $((6 * 7))-seen"}),
        )),
        ScriptedCall::response(text_response("Checked.")),
    ]);
    let mut graph = graph("");
    node_config(&mut graph, "a")["env"] = json!({"AGENT_KEY": {"$secret": "WORKFLOW_KEY"}});
    let secrets = MapSecrets::from_pairs(&[("WORKFLOW_KEY", "workflow-secret-value")]);
    let report = runtime(&dir, client)
        .secrets(secrets)
        .run(graph)
        .await
        .expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    let requests = serde_json::to_string(&provider.requests()).expect("requests");
    assert!(requests.contains("42-seen"), "{requests}");
}

#[tokio::test]
async fn a_workflow_secret_the_run_cannot_supply_fails_the_native_node() {
    let dir = RunDir::new("pebble-missing-secret");
    let (client, _provider) =
        scripted_client(vec![ScriptedCall::response(text_response("Never asked."))]);
    let mut graph = graph("");
    node_config(&mut graph, "a")["env"] = json!({"AGENT_KEY": {"$secret": "NOT_THERE"}});
    let report = runtime(&dir, client).run(graph).await.expect("replay");
    // The node fails before the session opens; this graph routes the failure
    // on, so the node's outcome is what says so.
    let output = output_of(&report, "a");
    assert_eq!(output["outcome"], json!("failed"));
    assert_eq!(output["failure_class"], json!("secret_unavailable"));
    assert!(
        output["failure_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("NOT_THERE")),
        "{output}"
    );
}

#[tokio::test]
async fn repairs_share_history_and_sum_accounting() {
    let dir = RunDir::new("pebble-repair");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(text_response("not JSON")),
        ScriptedCall::response(text_response(r#"{"value":42}"#)),
    ]);
    let report = runtime(&dir, client).run(graph(r#", output_schema="{\"type\":\"object\",\"required\":[\"value\"],\"properties\":{\"value\":{\"type\":\"integer\"}}}""#)).await.expect("replay");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["structured"]["value"], 42);
    assert_eq!(metrics(&report)["pebble.prompts"], 2);
    assert_eq!(metrics(&report)["pebble.usage"]["tokens"]["input"], 20);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        serde_json::to_string(&requests[1])
            .expect("request")
            .contains("not JSON")
    );
}

#[tokio::test]
async fn invalid_output_keeps_accounting() {
    let dir = RunDir::new("pebble-bad-output");
    let (client, _) = scripted_client(vec![ScriptedCall::response(text_response("not JSON"))]);
    let report = runtime(&dir, client)
        .run(graph(r#", output_schema="routing", output_retries=1"#))
        .await
        .expect("replay");
    assert_eq!(testkit::status_of(&report, "a").as_deref(), Some("failure"));
    assert_eq!(output_of(&report, "a")["failure_class"], "bad_output");
    assert_eq!(metrics(&report)["pebble.prompts"], 2);
    assert_eq!(metrics(&report)["pebble.usage"]["tokens"]["input"], 20);
}

#[tokio::test]
async fn cancellation_settles_the_prompt_and_preserves_usage() {
    let dir = RunDir::new("pebble-cancel");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "shell",
            "work",
            json!({"command":"printf ready"}),
        )),
        ScriptedCall::PendingOpen,
    ]);
    let rt = runtime(&dir, client);
    let driver = rt.driver(graph(""));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    timeout(Duration::from_secs(15), async {
        provider.wait_for_call().await;
        provider.wait_for_call().await;
    })
    .await
    .expect("second call");
    handle.cancel(CancelScopeId::ROOT).await;
    let report = timeout(Duration::from_secs(10), run)
        .await
        .expect("cancel settles")
        .expect("run task");
    assert_ne!(report.status, RunStatus::Success);
    assert_eq!(metrics(&report)["pebble.usage"]["tokens"]["input"], 10);
    assert_eq!(metrics(&report)["pebble.prompts"], 1);
}

#[tokio::test]
async fn oversized_process_output_keeps_bounded_head_and_tail() {
    let dir = RunDir::new("pebble-output-capture");
    let executor = HostExecutor::new(dir.path());
    let scope = ScopeSpec::new(ScopeId::new(1), "pebble").with_grace(Duration::from_millis(50));
    let handle = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("scope");
    let environment = PebbleEnvironment::prepare(
        handle.exec(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("prepare");
    let outcome = environment.exec(ExecRequest { output_bytes_cap: Some(12), ..ExecRequest::new("printf 'HEAD\\r\\n'; printf '%0200000d' 0; printf '\\000\\377TAIL'; printf 'err\\r\\n\\000\\377' >&2") }).await.expect("capture");
    assert_eq!(outcome.stdout_capture.observed_bytes, 200_012);
    assert_eq!(outcome.stdout_capture.retained_bytes, 12);
    assert_eq!(outcome.stdout_capture.omitted_bytes, 200_000);
    assert_eq!(outcome.result.stdout, "HEAD\r\n\0\u{fffd}TAIL");
    assert_eq!(outcome.result.stderr, "err\r\n\0\u{fffd}");
    assert_eq!(outcome.stderr_capture.observed_bytes, 7);
    assert_eq!(outcome.stderr_capture.retained_bytes, 7);
    assert_eq!(outcome.stderr_capture.omitted_bytes, 0);
    assert!(outcome.result.is_success(), "truncation is not a failure");
    assert!(
        executor
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
}

#[tokio::test]
async fn model_failure_keeps_prior_usage_and_known_cost() {
    use lithos_llm::types::{Cost, CostSource, ErrorKind};
    use pebble_coding_agent::test_support::ScriptedFailure;
    let dir = RunDir::new("pebble-model-error");
    let mut first = tool_call_response("shell", "work", json!({"command":"true"}));
    first.cost = Some(Cost {
        usd_micros: 123,
        source:     CostSource::Provider,
    });
    let (client, _) = scripted_client(vec![
        ScriptedCall::response(first),
        ScriptedCall::Failure(ScriptedFailure::terminal(
            ErrorKind::Authentication,
            "test failure",
        )),
    ]);
    let report = runtime(&dir, client).run(graph("")).await.expect("replay");
    assert_eq!(
        output_of(&report, "a")["failure_class"],
        "llm:authentication"
    );
    assert_eq!(metrics(&report)["pebble.usage"]["tokens"]["input"], 10);
    assert_eq!(
        metrics(&report)["pebble.usage"]["cost"],
        json!({"usd_micros": 123, "source": "provider"})
    );
}

#[test]
fn backend_selection_inherits_and_accepts_stylesheets() {
    for source in [
        r#"digraph T { start [shape=Mdiamond]; a [backend="api", prompt="hello"]; exit [shape=Msquare]; start -> a -> exit; }"#,
        r#"digraph T { graph [backend="api"]; start [shape=Mdiamond]; a [prompt="hello"]; exit [shape=Msquare]; start -> a -> exit; }"#,
        r#"digraph T { graph [model_stylesheet="* { backend: api; }"]; start [shape=Mdiamond]; a [prompt="hello"]; exit [shape=Msquare]; start -> a -> exit; }"#,
    ] {
        let result =
            frontend_attractor::load("test.fabro", source, &NoFiles, &CompileInputs::new());
        assert!(
            result.diagnostics.iter().next().is_none(),
            "backend selection must not warn: {:?}",
            result.diagnostics
        );
        let graph = result
            .graph
            .unwrap_or_else(|| panic!("lowering failed: {:?}", result.diagnostics));
        let node = graph
            .body
            .nodes
            .iter()
            .find(|node| node.name == "a")
            .expect("agent");
        assert_eq!(node.step.config["backend"], "api");
    }
    let invalid = frontend_attractor::load(
        "test.fabro",
        r#"digraph T { start [shape=Mdiamond]; a [backend="invalid"]; exit [shape=Msquare]; start -> a -> exit; }"#,
        &NoFiles,
        &CompileInputs::new(),
    );
    assert!(invalid.graph.is_none());
    assert!(
        invalid
            .diagnostics
            .iter()
            .any(|d| d.code == "attractor.bad_backend")
    );
}

struct NativeStarted(mpsc::Sender<ir::FiringId>);
impl EventObserver for NativeStarted {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _: &EngineState) {
        if let Event::StepProgressRecorded {
            firing,
            ev: ir::StepEvent::Custom(value),
        } = &record.event
            && value["kind"] == "pebble"
        {
            let _ = self.0.try_send(*firing);
        }
    }
}

#[tokio::test]
async fn steering_and_attributed_events_reach_the_native_session() {
    let dir = RunDir::new("pebble-steer");
    let gate = dir.path().join("release");
    let command = format!("while [[ ! -f '{}' ]]; do sleep 0.01; done", gate.display());
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "shell",
            "gate",
            json!({"command":command}),
        )),
        ScriptedCall::response(text_response("original answer")),
        ScriptedCall::response(text_response("steered answer")),
    ]);
    let (send, mut receive) = mpsc::channel(1);
    let rt = runtime(&dir, client).observe(Arc::new(NativeStarted(send)));
    let driver = rt.driver(graph(""));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    let firing = timeout(Duration::from_secs(10), receive.recv())
        .await
        .expect("native event")
        .expect("firing");
    timeout(Duration::from_secs(10), provider.wait_for_call())
        .await
        .expect("first model call");
    assert_eq!(
        handle
            .deliver(
                firing,
                ir::Control::Deliver(json!({"text":"Check the edge cases too"}))
            )
            .await,
        DeliverDisposition::Delivered
    );
    fs::write(gate, "go").await.expect("release tool");
    let report = timeout(Duration::from_secs(15), run)
        .await
        .expect("run settles")
        .expect("task");
    assert_eq!(output_of(&report, "a")["text"], "steered answer");
    let requests = provider.requests();
    assert!(
        serde_json::to_string(&requests.last())
            .expect("request")
            .contains("Check the edge cases too")
    );
    let events: Vec<_> = report
        .state
        .log
        .events()
        .filter_map(|event| match event {
            Event::StepProgressRecorded {
                ev: ir::StepEvent::Custom(value),
                ..
            } if value["kind"] == "pebble" => Some(value),
            _ => None,
        })
        .collect();
    assert!(!events.is_empty());
    let stream = &events[0]["event"]["stream_id"];
    for (index, envelope) in events.iter().enumerate() {
        assert_eq!(envelope["node"], "a");
        assert_eq!(envelope["firing"], json!(firing));
        assert!(!envelope["attempt"].is_null());
        assert_eq!(envelope["event"]["seq"], json!(index + 1));
        assert_eq!(&envelope["event"]["stream_id"], stream);
        assert_eq!(&envelope["event"]["session_id"], stream);
    }
    assert!(
        events
            .iter()
            .any(|event| event["event"]["event"].get("ToolCallCompleted").is_some())
    );
}

/// Reports the firing of node `a` once its step has started: the native
/// session's build begins at once and, here, waits on an MCP server that
/// never answers.
struct BuildStarted(mpsc::Sender<ir::FiringId>);
impl EventObserver for BuildStarted {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, state: &EngineState) {
        if let Event::StepStarted { firing, .. } = &record.event
            && state
                .firing_node(*firing)
                .and_then(|id| state.graph().node(id))
                .is_some_and(|node| node.name == "a")
        {
            let _ = self.0.try_send(*firing);
        }
    }
}

/// A delivery that arrives while the session is still being built waits on
/// the node run's steering bus and reaches the session when it attaches, in
/// the mode it was sent: a follow-up, which runs as its own turn once the
/// first answer is reached. Here Pebble holds the build on an MCP server
/// that never completes its handshake; the node proceeds without it.
#[tokio::test]
async fn a_delivery_before_the_session_is_built_runs_as_a_follow_up() {
    let dir = RunDir::new("pebble-deliver-before-build");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(text_response("original answer")),
        ScriptedCall::response(text_response("follow-up answer")),
    ]);
    // One stdio server whose handshake never completes within its startup
    // timeout, as the Fabro frontend resolves `[run.agent.mcps.slow]` with
    // `command = ["sleep", "5"]` and `startup_timeout = "1s"`.
    let settings = RunSettings {
        mcps: vec![McpServer {
            name:               "slow".to_owned(),
            transport:          McpTransport::Stdio {
                command: vec!["sleep".to_owned(), "5".to_owned()],
                env:     BTreeMap::new(),
            },
            startup_timeout_ms: 1_000,
            tool_timeout_ms:    DEFAULT_TOOL_TIMEOUT_MS,
            source:             "workflow.toml".to_owned(),
        }],
        ..RunSettings::default()
    };
    let lowered = frontend_attractor::lower(
        "wf/w.fabro",
        r#"digraph T {
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        a [prompt="Make the change and verify it"]
        exit [shape=Msquare]
        start -> a -> exit
    }"#,
        &NoFiles,
        &CompileInputs::new(),
        settings,
        Diagnostics::new(),
    );
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    let graph = lowered.graph.expect("valid workflow");
    let (send, mut receive) = mpsc::channel(1);
    let rt = runtime(&dir, client).observe(Arc::new(BuildStarted(send)));
    let driver = rt.driver(graph);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    let firing = timeout(Duration::from_secs(10), receive.recv())
        .await
        .expect("the step starts")
        .expect("firing");
    // The step is started before its control channel is live; the build
    // holds for the server's startup timeout, so a short retry lands the
    // delivery well inside it.
    let mut disposition = DeliverDisposition::NotLive;
    for _ in 0..50 {
        disposition = handle
            .deliver(
                firing,
                ir::Control::Deliver(json!({"text":"Also check the docs"})),
            )
            .await;
        if disposition == DeliverDisposition::Delivered {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(disposition, DeliverDisposition::Delivered);
    let report = timeout(Duration::from_secs(30), run)
        .await
        .expect("run settles")
        .expect("task");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "follow-up answer");
    let requests: Vec<String> = provider
        .requests()
        .iter()
        .map(|request| serde_json::to_string(request).expect("request"))
        .collect();
    assert_eq!(requests.len(), 2, "the prompt, then the follow-up turn");
    assert!(
        !requests[0].contains("Also check the docs"),
        "the first turn is the prompt alone: {}",
        requests[0]
    );
    assert!(
        requests[1].contains("Also check the docs"),
        "the follow-up ran after the first answer: {}",
        requests[1]
    );
}

/// A durable store that refuses every write.
struct RefusingStore;

#[async_trait::async_trait]
impl EventObserver for RefusingStore {
    fn on_record(&self, _: &EventRecord, _recorded_at: u64, _: &EngineState) {}

    async fn durable(&self, _seq: u64) -> Result<(), ObserveError> {
        Err(ObserveError::new("refusing-store", "the write failed"))
    }
}

/// Pebble's events are recorded acknowledged: a store that cannot write them
/// fails the acknowledgement, and the session stops with that failure — here
/// at its first event, before any model request — instead of running on with
/// nothing recording it. The stage's failure names the store and the cause.
#[tokio::test]
async fn a_durable_store_failure_stops_the_native_session() {
    let dir = RunDir::new("pebble-durable-failure");
    let (client, provider) = scripted_client(vec![ScriptedCall::response(text_response(
        "never recorded",
    ))]);
    let report = runtime(&dir, client)
        .observe(Arc::new(RefusingStore))
        .run(graph(""))
        .await
        .expect("replay");
    let failure = report
        .state
        .history()
        .iter()
        .find(|row| row.name == "a")
        .and_then(|row| row.outcome.status.failure_info().cloned())
        .expect("the agent node failed");
    assert_eq!(failure.class.as_str(), "pebble_config");
    assert!(
        failure.message.contains("did not reach durable storage")
            && failure.message.contains("refusing-store: the write failed"),
        "the store's failure is the node's: {}",
        failure.message
    );
    assert!(
        provider.requests().is_empty(),
        "the session stopped before its first model request"
    );
}

#[tokio::test]
async fn kill_stops_a_tool_that_ignores_term() {
    let dir = RunDir::new("pebble-kill");
    let marker = dir.path().join("started");
    let command = format!(
        "trap '' TERM; echo $$ > '{}'; while :; do :; done",
        marker.display()
    );
    let (client, _) = scripted_client(vec![ScriptedCall::response(tool_call_response(
        "shell",
        "busy",
        json!({"command":command}),
    ))]);
    let (send, mut receive) = mpsc::channel(1);
    let rt = runtime(&dir, client).observe(Arc::new(NativeStarted(send)));
    let driver = rt.driver(graph(""));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    let firing = timeout(Duration::from_secs(10), receive.recv())
        .await
        .expect("native event")
        .expect("firing");
    assert!(testkit::wait_for_file(&marker, Duration::from_secs(10)).await);
    let pid = fs::read_to_string(&marker).await.expect("pid");
    let _ = firing;
    handle.cancel(CancelScopeId::ROOT).await;
    handle.cancel(CancelScopeId::ROOT).await;
    let report = timeout(Duration::from_secs(5), run)
        .await
        .expect("kill settles")
        .expect("task");
    assert_eq!(
        testkit::status_of(&report, "a").as_deref(),
        Some("cancelled")
    );
    assert_eq!(metrics(&report)["pebble.usage"]["tokens"]["input"], 10);
    let status = Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(Stdio::null())
        .status()
        .await
        .expect("pid probe");
    assert!(!status.success(), "the tool process was reaped");
}

#[tokio::test]
async fn node_settings_select_the_actual_model_and_reasoning() {
    let dir = RunDir::new("pebble-model-settings");
    let (client, provider) = scripted_client(vec![ScriptedCall::response(text_response("done"))]);
    let report = runtime(&dir, client)
        .run(graph(
            r#", model="thinking", provider="test", reasoning_effort="high""#,
        ))
        .await
        .expect("replay");
    assert_eq!(testkit::status_of(&report, "a").as_deref(), Some("success"));
    let requests = provider.requests();
    assert_eq!(requests[0].model(), "test/thinking");
    assert_eq!(requests[0].reasoning_effort(), Some(ReasoningEffort::High));
}

#[tokio::test(flavor = "multi_thread")]
async fn environment_contract_runs_inside_a_container_without_host_files() {
    use runtime::executor::sandbox::RoutingExecutor;
    if !testkit::is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("pebble-docker-contract");
    let executor = RoutingExecutor::local(dir.path(), Retention::Never);
    let scope = ScopeSpec::new(ScopeId::new(1), "pebble-docker")
        .with_grace(Duration::from_millis(100))
        .with_runtime(ir::RuntimeSpec::container("buildpack-deps:noble"));
    let handle = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("container");
    assert!(!handle.exec().shares_host_filesystem());
    let environment = PebbleEnvironment::prepare(
        handle.exec(),
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("prepare container");
    assert_eq!(environment.platform(), "linux");
    let contract = EnvironmentContract::new(&environment, "contract")
        .with_operation_timeout(Duration::from_secs(30));
    contract.verify_files().await.expect("container files");
    contract
        .verify_search()
        .await
        .expect("container search (grep fallback)");
    contract
        .verify_commands()
        .await
        .expect("container commands");
    assert!(
        executor
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
}

#[test]
fn mixed_backends_inherit_only_their_own_configuration() {
    let source = r#"digraph T {
        graph [acp.command="agent-command", default_model="test/model"]
        start [shape=Mdiamond]
        native [backend="api", prompt="native"]
        external [prompt="ACP"]
        exit [shape=Msquare]
        start -> native -> external -> exit
    }"#;
    let result = frontend_attractor::load("mixed.fabro", source, &NoFiles, &CompileInputs::new());
    let graph = result.graph.expect("mixed workflow");
    let native = graph
        .body
        .nodes
        .iter()
        .find(|node| node.name == "native")
        .expect("native");
    let external = graph
        .body
        .nodes
        .iter()
        .find(|node| node.name == "external")
        .expect("external");
    assert_eq!(native.step.config["backend"], "api");
    assert!(native.step.config.get("acp").is_none());
    assert_eq!(external.step.config["acp"]["command"], "agent-command");
}

/// Answers the first core `Question` it sees with `answer`, through the run
/// handle: the smallest stand-in for the host's interview dispatcher.
struct GateAnswerer {
    handle: Mutex<Option<RunHandle>>,
    answer: Answer,
    asked:  Mutex<Vec<Question>>,
}

impl EventObserver for GateAnswerer {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _state: &EngineState) {
        let Event::StepProgressRecorded { firing, ev } = &record.event else {
            return;
        };
        let Some(question) = Question::from_event(ev) else {
            return;
        };
        self.asked
            .lock()
            .expect("not poisoned")
            .push(question.clone());
        let handle = self
            .handle
            .lock()
            .expect("not poisoned")
            .clone()
            .expect("wired");
        let answer = self.answer.clone().for_question(&question.id);
        let firing = *firing;
        tokio::spawn(async move {
            handle.deliver(firing, answer.to_control()).await;
        });
    }
}

#[tokio::test]
async fn an_agent_question_rides_the_core_question_protocol() {
    let dir = RunDir::new("pebble-question");
    // The test catalog's model runs the Anthropic harness, whose question
    // tool is `AskUserQuestion`; the options become `option_1`, `option_2`.
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "AskUserQuestion",
            "ask",
            json!({"questions":[{"question":"Which file?","header":"File","options":[{"label":"README"},{"label":"CHANGELOG"}],"multiSelect":false}]}),
        )),
        ScriptedCall::response(text_response("Editing CHANGELOG.")),
    ]);
    let rt = runtime(&dir, client);
    let answerer = Arc::new(GateAnswerer {
        handle: Mutex::new(None),
        answer: Answer::choice("option_2"),
        asked:  Mutex::new(Vec::new()),
    });
    let graph = graph("");
    let driver = rt.driver(graph.clone()).observe(answerer.clone());
    *answerer.handle.lock().expect("not poisoned") = Some(driver.handle());
    let report = rt
        .run_verified(graph, |_| Ok::<_, ReplayMismatch>(driver))
        .await
        .expect("replay is byte-identical");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(output_of(&report, "a")["text"], "Editing CHANGELOG.");
    let asked = answerer.asked.lock().expect("not poisoned").clone();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].id.starts_with("a#"), "{}", asked[0].id);
    assert!(asked[0].id.contains("/agent/"), "{}", asked[0].id);
    assert!(asked[0].id.ends_with("/ask/0"), "{}", asked[0].id);
    assert_eq!(asked[0].kind.as_deref(), Some("multiple_choice"));
    assert_eq!(asked[0].options.len(), 2);
    assert_eq!(asked[0].options[1].key, "option_2");
    assert!(asked[0].freeform);
    let requests = serde_json::to_string(&provider.requests()).expect("requests");
    assert!(
        requests.contains("option_2"),
        "the answer reaches the next model request: {requests}"
    );
}

/// A native agent attempt is `ExecutorEnforced`: the driver owns the
/// deadline and cancels Pebble through the prompt's cancellation token.
/// Pebble's own wall-clock timer is left unset, so the attempt reports
/// `timed_out` from the driver, with the settled prompt's usage kept.
#[tokio::test]
async fn the_driver_deadline_cancels_a_native_attempt_through_its_token() {
    let dir = RunDir::new("pebble-deadline");
    let (client, provider) = scripted_client(vec![
        ScriptedCall::response(tool_call_response(
            "shell",
            "work",
            json!({"command":"printf ready"}),
        )),
        ScriptedCall::PendingOpen,
    ]);
    let rt = runtime(&dir, client);
    let graph = graph(r#", timeout="1s""#);
    assert_eq!(
        graph
            .nodes
            .iter()
            .find(|n| n.name == "a")
            .expect("a")
            .budget
            .timeout_policy,
        ir::TimeoutPolicy::ExecutorEnforced
    );
    let driver = rt.driver(graph);
    let run = tokio::spawn(driver.run());
    timeout(Duration::from_secs(15), async {
        provider.wait_for_call().await;
        provider.wait_for_call().await;
    })
    .await
    .expect("second call");
    let report = timeout(Duration::from_secs(15), run)
        .await
        .expect("the deadline settles the attempt")
        .expect("run task");
    assert_eq!(
        testkit::status_of(&report, "a").as_deref(),
        Some("timed_out")
    );
    assert_eq!(metrics(&report)["pebble.usage"]["tokens"]["input"], 10);
    assert_eq!(metrics(&report)["pebble.prompts"], 1);
}
