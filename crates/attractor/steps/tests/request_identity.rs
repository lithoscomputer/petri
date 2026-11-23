//! The stage's identity on its outbound requests: a native agent node's
//! model calls, a prompt node's model call, and an agent node's requests to
//! an `http` MCP server carry the run and the step as `x-petri-*` headers,
//! with the values the public events name the stage by.
//!
//! The run goes through the coordinator (`execution::host`), which registers
//! the `ExecutionIdentity` the run, invocation and execution come from. The
//! model calls are seen by a middleware on the scripted client, which reads
//! the client's default headers that `lithos-llm` puts on every HTTP request;
//! the MCP requests are recorded on the wire by the scripted server.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use attractor_steps::pebble::PebbleClient;
use attractor_steps::register;
use execution::events::{CollectingSink, EventProjector, RunEvent};
use execution::host::{self, HostRun};
use execution::{ExecutionObserver, RunKey};
use frontend::{CompileInputs, Diagnostics, NoFiles};
use frontend_attractor::RunSettings;
use frontend_attractor::mcps::{McpHttpProtocol, McpServer, McpTransport, McpValue};
use ir::{Graph, RunStatus};
use lithos_llm::Error;
use lithos_llm::middleware::{Call, Middleware, Next, Operation, Output};
use pebble_coding_agent::test_support::{
    ScriptedCall, ScriptedCompletion, ScriptedProvider, scripted_client_builder, text_response,
    tool_call_response,
};
use runtime::executor::Retention;
use runtime::{RunOptions, Runtime};
use serde_json::{Value, json};
use testkit::RunDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

const RUN_KEY: &str = "identity-run";

/// The `x-petri-*` headers of every model call, with its operation.
#[derive(Default)]
struct Headers(Mutex<Vec<(Operation, BTreeMap<String, String>)>>);

#[async_trait]
impl Middleware for Headers {
    async fn handle(&self, call: Call, next: Next) -> Result<Output, Error> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push((
            call.operation(),
            petri_headers(
                call.context()
                    .default_headers()
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.to_str().unwrap_or_default())),
            ),
        ));
        next.run(call).await
    }
}

fn petri_headers<'a>(
    headers: impl Iterator<Item = (&'a str, &'a str)>,
) -> BTreeMap<String, String> {
    headers
        .filter(|(name, _)| name.starts_with("x-petri-"))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn server_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fabro/acceptance/testdata/mcp_server.py")
        .canonicalize()
        .expect("the scripted MCP server exists")
}

async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    listener.local_addr().expect("address").port()
}

fn graph(mcp_port: u16) -> Graph {
    let source = r#"digraph T {
        graph [backend="api", default_model="test/model"]
        start [shape=Mdiamond]
        a [prompt="Use the remote server."]
        p [shape=tab, prompt="Summarize."]
        exit [shape=Msquare]
        start -> a -> p -> exit
    }"#;
    let settings = RunSettings {
        mcps: vec![McpServer {
            name:               "remote".to_owned(),
            transport:          McpTransport::Http {
                protocol: McpHttpProtocol::StreamableHttp,
                url:      format!("http://127.0.0.1:{mcp_port}"),
                headers:  BTreeMap::from([(
                    "X-Case".to_owned(),
                    McpValue::Literal("configured".into()),
                )]),
            },
            startup_timeout_ms: 15_000,
            tool_timeout_ms:    15_000,
            source:             "workflow.toml".to_owned(),
        }],
        ..RunSettings::default()
    };
    let lowered = frontend_attractor::lower(
        "test.fabro",
        source,
        &NoFiles,
        &CompileInputs::new(),
        settings,
        Diagnostics::new(),
    );
    lowered.graph.unwrap_or_else(|| {
        let diagnostics: Vec<String> = lowered
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect();
        panic!("lowers: {diagnostics:?}")
    })
}

/// The headers a request from `node` must carry, read off the public
/// events: the run key, the event's context and its subject.
fn expected(events: &[RunEvent], node: &str) -> BTreeMap<String, String> {
    let event = events
        .iter()
        .find(|event| {
            event.subject.as_ref().is_some_and(|subject| {
                subject.node.name == node && subject.firing.is_some() && subject.attempt.is_some()
            })
        })
        .unwrap_or_else(|| panic!("an event of `{node}`'s firing"));
    let subject = event.subject.as_ref().expect("subject");
    BTreeMap::from([
        ("x-petri-run-id".to_owned(), RUN_KEY.to_owned()),
        (
            "x-petri-invocation".to_owned(),
            event
                .context
                .invocation
                .expect("invocation")
                .raw()
                .to_string(),
        ),
        (
            "x-petri-execution".to_owned(),
            event
                .context
                .execution
                .expect("execution")
                .raw()
                .to_string(),
        ),
        ("x-petri-node".to_owned(), node.to_owned()),
        (
            "x-petri-firing".to_owned(),
            subject.firing.expect("firing").0.to_string(),
        ),
        (
            "x-petri-attempt".to_owned(),
            subject.attempt.expect("attempt").0.to_string(),
        ),
    ])
}

#[tokio::test]
async fn model_and_http_mcp_requests_carry_the_stages_identity() {
    let dir = RunDir::new("request-identity");
    let mcp_port = free_port().await;
    let mcp_headers = dir.path().join("mcp-headers.jsonl");
    let mut server = Command::new("python3")
        .arg(server_script())
        .args(["--http", &mcp_port.to_string()])
        .env("MCP_TEST_HEADERS", &mcp_headers)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("the MCP server starts");
    timeout(Duration::from_secs(15), async {
        while TcpStream::connect(("127.0.0.1", mcp_port)).await.is_err() {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the MCP server listens");

    let provider = ScriptedProvider::new(vec![
        ScriptedCall::response(tool_call_response(
            "mcp__remote__echo",
            "echo",
            json!({"message": "hello"}),
        )),
        ScriptedCall::response(text_response("Done.")),
    ])
    .completing(vec![ScriptedCompletion::response(text_response(
        "Summary.",
    ))]);
    let recorded = Arc::new(Headers::default());
    let (builder, provider) = scripted_client_builder(provider);
    let builder = builder.middleware_arc(recorded.clone());
    let client = builder.build().expect("the client builds").client;

    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_millis(200);
    options.retention = Retention::Always;
    options.echo = false;
    options.run_key = Some(RunKey::new(RUN_KEY));
    let rt = register(Runtime::standard())
        .capability(PebbleClient(client))
        .options(options);
    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::new(sink.clone());
    let report = host::run_configured(
        &rt,
        HostRun::new(graph(mcp_port)).observe(projector.clone() as Arc<dyn ExecutionObserver>),
        |_, _| {},
    )
    .await
    .expect("the run completes");
    projector.shutdown().await;
    let _ = server.kill().await;
    let events = sink.events();
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{}",
        serde_json::to_string(&events).unwrap_or_default()
    );
    let agent = expected(&events, "a");
    let prompt = expected(&events, "p");
    assert_ne!(agent, prompt);

    // The agent's two streamed rounds and the prompt node's one completion,
    // each under its own stage.
    assert_eq!(provider.call_count(), 2);
    let calls = recorded
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(calls, [
        (Operation::Stream, agent.clone()),
        (Operation::Stream, agent.clone()),
        (Operation::Complete, prompt),
    ]);

    // Every request the agent's session sent the MCP server, on the wire:
    // the configured header beside the stage's.
    let lines = fs::read_to_string(&mcp_headers).expect("the server recorded requests");
    let requests: Vec<Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect();
    assert!(
        requests.len() >= 3,
        "initialize, list and call: {requests:?}"
    );
    for request in &requests {
        let headers = request.as_object().expect("an object");
        assert_eq!(headers["x-case"], "configured", "{request}");
        let petri = petri_headers(
            headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str().unwrap_or_default())),
        );
        assert_eq!(petri, agent, "{request}");
    }
}
