//! The live ACP product tier: Claude Code and Gemini CLI through Petri's
//! ACP step, on the host and in a container.
//!
//! Claude Code speaks ACP through the `claude-code-acp` adapter (the
//! `@zed-industries/claude-code-acp` package; the `claude` binary itself has
//! no ACP mode) with `ANTHROPIC_API_KEY`; Gemini CLI speaks it as
//! `gemini --acp` with `GEMINI_API_KEY`. Every test is `#[ignore]`: it runs
//! with `--ignored`, and then skips itself unless the product's binary is on
//! `PATH` and its credential is set, or fails instead when
//! `PETRI_REQUIRE_ACP_PRODUCTS` is set. The container cells build the
//! `petri-acp-products` image once (both products over `node:22`) and skip
//! without the Docker plugin or a daemon.
//!
//! Each cell runs a one-stage workflow that asks the agent to create a
//! file, checks the file from a command node in the same scope (so the host
//! and the container are read the same way), and reads the public stream:
//! the `acp` envelopes carry the session's tool calls, and the stage's
//! metrics carry the usage the product reports (Gemini CLI reports the
//! session usage extension; the Claude Code adapter at 0.16.2 reports
//! none). The hook cell adds a `[[run.hooks]]` `pre_tool_use` hook that
//! blocks every permission request, and checks that the file was not
//! written and that the block is on the stream.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::{env, fs};

use petri::attractor::acp::ENVELOPE_KIND;
use petri::attractor::hooks::REPORT_EVENT;
use petri::engine::Event;
use petri::execution::host;
use petri::executor::{MapSecrets, Retention};
use petri::frontend::{CompileInputs, MapFiles, fabro};
use petri::ir::{ExprOrValue, Graph, RunStatus, StepEvent, Value};
use petri::{RunOptions, Runtime, driver};
use serde_json::json;
use testkit::{RunDir, output_of, status_of};
use tokio::process::Command;
use tokio::time::timeout;

/// The image the container cells run: both products installed over Node,
/// built here from the Dockerfile below when the daemon does not have it.
const IMAGE: &str = "petri-acp-products:1";

const DOCKERFILE: &str = r"
FROM node:22-bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git python3 \
 && rm -rf /var/lib/apt/lists/*
RUN npm install -g @zed-industries/claude-code-acp@0.16.2 @google/gemini-cli@0.45.2
";

/// One ACP product: how it is launched and what it needs.
struct Product {
    name:       &'static str,
    /// The binary on `PATH`, here and in the image.
    binary:     &'static str,
    args:       &'static [&'static str],
    credential: &'static str,
    /// Whether the product reports the session usage extension.
    usage:      bool,
}

const CLAUDE: Product = Product {
    name:       "claude-code",
    binary:     "claude-code-acp",
    args:       &[],
    credential: "ANTHROPIC_API_KEY",
    usage:      false,
};

const GEMINI: Product = Product {
    name:       "gemini-cli",
    binary:     "gemini",
    args:       &["--acp"],
    credential: "GEMINI_API_KEY",
    usage:      true,
};

impl Product {
    fn command(&self) -> String {
        let mut words = vec![self.binary.to_owned()];
        words.extend(self.args.iter().map(|arg| (*arg).to_owned()));
        words.join(" ")
    }

    fn on_path(&self) -> bool {
        env::var_os("PATH")
            .is_some_and(|path| env::split_paths(&path).any(|dir| dir.join(self.binary).is_file()))
    }

    fn credential(&self) -> Option<String> {
        env::var(self.credential)
            .ok()
            .filter(|value| !value.is_empty())
    }

    /// The credential when the product can run here, else the reason it
    /// cannot. `PETRI_REQUIRE_ACP_PRODUCTS` turns the skip into a failure.
    #[expect(
        clippy::print_stderr,
        reason = "the skip notice belongs to the test runner's output"
    )]
    fn require(&self, container: bool) -> Option<String> {
        let mut missing = Vec::new();
        if !container && !self.on_path() {
            missing.push(format!("`{}` on PATH", self.binary));
        }
        if self.credential().is_none() {
            missing.push(format!("{} set", self.credential));
        }
        if missing.is_empty() {
            return self.credential();
        }
        let reason = format!("{} needs {}", self.name, missing.join(" and "));
        assert!(
            env::var_os("PETRI_REQUIRE_ACP_PRODUCTS").is_none_or(|v| v.is_empty()),
            "PETRI_REQUIRE_ACP_PRODUCTS is set, but {reason}"
        );
        eprintln!("skipping: {reason}");
        None
    }
}

/// The one-stage workflow: the agent creates the file, a command node in
/// the same scope reads it back (or checks it is absent).
fn workflow(product: &Product, check: &str) -> String {
    format!(
        r#"digraph W {{
    graph [goal="Create a file", backend="acp", acp.command="{}"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    write [prompt="Create a file named hello.txt in the current working directory whose whole content is exactly the line `hello from acp`. Use your file writing tool directly; do not ask any question. When the file is written, reply with the single word done.", timeout="240s"]
    check [shape=parallelogram, script="{check}"]
    start -> write -> check -> exit
}}"#,
        product.command()
    )
}

const CONTAINER_TOML: &str = r#"
[run.environment]
id = "box"

[environments.box]
provider = "docker"

[environments.box.image]
docker = "petri-acp-products:1"
"#;

const DENY_HOOK_TOML: &str = r#"
[[run.hooks]]
name = "deny-everything"
event = "pre_tool_use"
script = "exit 2"
"#;

fn lower(dot: &str, toml: &str) -> Graph {
    let files = MapFiles(BTreeMap::from([(
        "wf/workflow.toml".to_string(),
        toml.to_string(),
    )]));
    let lowered = fabro::load("wf/w.fabro", dot, &files, &CompileInputs::new());
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        lowered.diagnostics
    );
    lowered.graph.expect("lowers")
}

/// The product's environment on the host: the Claude Code adapter refuses
/// to start inside a Claude Code session unless `CLAUDECODE` is unset, and
/// an empty value is unset to it.
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

/// The agent stage names the product's credential, as a workflow does with
/// `KEY = "{{ secrets.KEY }}"` under `[run.environment.env]`: Petri gives an
/// ACP agent no secret it does not name.
fn with_credential(mut graph: Graph, product: &Product) -> Graph {
    let write = graph
        .body
        .nodes
        .iter_mut()
        .find(|node| node.name == "write")
        .expect("the write stage");
    write.step.config["env"] = json!({ product.credential: { "$secret": product.credential } });
    graph
}

/// The runtime with the product's credential as the run's secret, which the
/// workflow names for the agent, on the host and in the container alike.
fn runtime(dir: &RunDir, product: &Product, credential: &str) -> Runtime {
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(5);
    options.retention = Retention::Never;
    options.echo = false;
    petri::runtime()
        .secrets(MapSecrets::from_pairs(&[(product.credential, credential)]))
        .options(options)
}

async fn run(rt: &Runtime, graph: Graph) -> driver::ExecutionReport {
    host::run(rt, graph).await.expect("the run completes")
}

/// The `step.progress.recorded` custom payloads of the run, in order.
fn customs(report: &driver::ExecutionReport) -> Vec<Value> {
    report
        .state
        .log
        .events()
        .filter_map(|event| match event {
            Event::StepProgressRecorded {
                ev: StepEvent::Custom(value),
                ..
            } => Some(value.clone()),
            _ => None,
        })
        .collect()
}

fn envelopes(report: &driver::ExecutionReport) -> Vec<Value> {
    customs(report)
        .into_iter()
        .filter(|value| value["kind"] == ENVELOPE_KIND)
        .collect()
}

fn updates_of(envelopes: &[Value], kind: &str) -> Vec<Value> {
    envelopes
        .iter()
        .filter(|e| e["event"]["update"]["sessionUpdate"] == kind)
        .cloned()
        .collect()
}

fn metrics(report: &driver::ExecutionReport, node: &str) -> BTreeMap<String, Value> {
    report
        .state
        .history()
        .iter()
        .find(|row| row.name == node)
        .unwrap_or_else(|| panic!("`{node}` finished"))
        .outcome
        .metrics
        .custom
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// The stream carries the session's tool calls (a call, and the call
/// reported finished) and, when the product reports it, its usage.
fn assert_session_on_stream(report: &driver::ExecutionReport, product: &Product) {
    let envelopes = envelopes(report);
    assert!(
        !envelopes.is_empty(),
        "the agent's updates are on the stream"
    );
    let sessions: Vec<&str> = envelopes
        .iter()
        .filter_map(|e| e["event"]["session_id"].as_str())
        .collect();
    assert!(
        !sessions.is_empty() && sessions.iter().all(|s| *s == sessions[0]),
        "one session on every envelope: {sessions:?}"
    );
    let seqs: Vec<u64> = envelopes
        .iter()
        .filter_map(|e| e["event"]["seq"].as_u64())
        .collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "the sequence climbs: {seqs:?}"
    );
    let calls = updates_of(&envelopes, "tool_call");
    assert!(
        !calls.is_empty(),
        "the session's tool calls are on the stream"
    );
    assert!(
        calls.iter().all(|c| c["event"]["tool_call_id"].is_string()),
        "every call names its id: {calls:?}"
    );
    let finished = updates_of(&envelopes, "tool_call_update");
    assert!(
        finished
            .iter()
            .any(|u| u["event"]["update"]["status"] == "completed"),
        "a tool call reported finished: {finished:?}"
    );
    let metrics = metrics(report, "write");
    assert!(
        metrics["acp.turns"]
            .as_u64()
            .is_some_and(|turns| turns >= 1),
        "{metrics:?}"
    );
    if product.usage {
        let tokens = metrics["acp.usage"]["tokens"]["input"]
            .as_u64()
            .unwrap_or(0)
            + metrics["acp.usage"]["tokens"]["output"]
                .as_u64()
                .unwrap_or(0);
        let context = updates_of(&envelopes, "usage_update");
        assert!(
            tokens > 0 || !context.is_empty(),
            "{} reports usage, and it is on the stage: {metrics:?}",
            product.name
        );
    }
}

/// The write cell: the file exists afterwards, read back by the command
/// node, and the session is on the stream.
async fn writes_a_file(product: &Product, rt: &Runtime, toml: &str, env: &[(&str, &str)]) {
    let graph = with_env(lower(&workflow(product, "cat hello.txt"), toml), env);
    let graph = with_credential(graph, product);
    let report = run(rt, graph).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(status_of(&report, "write").as_deref(), Some("success"));
    let read_back = output_of(&report, "check")["stdout"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        read_back.contains("hello from acp"),
        "the agent created the file: {read_back:?}"
    );
    assert_session_on_stream(&report, product);
}

/// The hook cell: every permission request is blocked, so the file is not
/// written, and the block is on the stream as the permission exchange and
/// the hook's report.
async fn a_hook_blocks_the_write(product: &Product, rt: &Runtime, env: &[(&str, &str)]) {
    let graph = with_env(
        lower(&workflow(product, "test ! -e hello.txt"), DENY_HOOK_TOML),
        env,
    );
    let graph = with_credential(graph, product);
    let report = run(rt, graph).await;
    assert_eq!(
        status_of(&report, "check").as_deref(),
        Some("success"),
        "the blocked write did not happen: {:?}",
        report.state.errors()
    );
    let envelopes = envelopes(&report);
    let blocked: Vec<&Value> = envelopes
        .iter()
        .filter(|e| e["event"]["method"] == "session/request_permission")
        .collect();
    assert!(
        !blocked.is_empty(),
        "the agent asked permission for the write: {:?}",
        envelopes
            .iter()
            .map(|e| e["event"]["update"]["sessionUpdate"].clone())
            .collect::<Vec<_>>()
    );
    assert!(
        blocked.iter().all(|e| e["event"]["blocked"].is_string()
            && e["event"]["outcome"]["outcome"] != "cancelled"),
        "every request was answered with the rejection: {blocked:?}"
    );
    let all = customs(&report);
    let reports: Vec<&Value> = all
        .iter()
        .filter(|v| v["kind"] == REPORT_EVENT && v["event"] == "pre_tool_use")
        .collect();
    assert!(
        reports
            .iter()
            .any(|r| r["report"]["decision"]["decision"] == "block"),
        "the hook's block is on the stream: {reports:?}"
    );
}

/// Build the product image once. `None` when Docker is not usable here.
#[expect(
    clippy::print_stderr,
    reason = "the skip notice belongs to the test runner's output"
)]
async fn product_image() -> Option<()> {
    if !testkit::is_docker_ready().await {
        return None;
    }
    let present = Command::new("docker")
        .args(["image", "inspect", IMAGE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success());
    if present {
        return Some(());
    }
    eprintln!("building {IMAGE}");
    let context = env::temp_dir().join(format!("petri-acp-products-{}", testkit::unique_id()));
    fs::create_dir_all(&context).expect("image context");
    fs::write(context.join("Dockerfile"), DOCKERFILE).expect("Dockerfile");
    let status = timeout(
        Duration::from_secs(900),
        Command::new("docker")
            .args(["build", "--quiet", "-t", IMAGE])
            .arg(&context)
            .status(),
    )
    .await
    .expect("the image builds within 15 minutes")
    .expect("docker build runs");
    let _ = fs::remove_dir_all(&context);
    assert!(status.success(), "docker build failed");
    Some(())
}

/// The host cell's environment: the Claude Code adapter's nested-session
/// check reads `CLAUDECODE`, which a test run inside Claude Code inherits.
const HOST_ENV: &[(&str, &str)] = &[("CLAUDECODE", "")];

fn host_dir(label: &str) -> RunDir {
    RunDir::new(&format!("acp-products-{label}"))
}

fn plugin_path() -> Option<PathBuf> {
    env::var_os("PETRI_SANDBOX_DOCKER_PLUGIN").map(PathBuf::from)
}

#[tokio::test]
#[ignore = "live: needs `claude-code-acp` on PATH and ANTHROPIC_API_KEY"]
async fn claude_code_writes_a_file_on_the_host_and_a_hook_blocks_it() {
    let Some(credential) = CLAUDE.require(false) else {
        return;
    };
    let dir = host_dir("claude-host");
    let rt = runtime(&dir, &CLAUDE, &credential);
    writes_a_file(&CLAUDE, &rt, "", HOST_ENV).await;
    let dir = host_dir("claude-host-hook");
    let rt = runtime(&dir, &CLAUDE, &credential);
    a_hook_blocks_the_write(&CLAUDE, &rt, HOST_ENV).await;
}

#[tokio::test]
#[ignore = "live: needs `gemini` on PATH and GEMINI_API_KEY"]
async fn gemini_cli_writes_a_file_on_the_host_and_a_hook_blocks_it() {
    let Some(credential) = GEMINI.require(false) else {
        return;
    };
    let dir = host_dir("gemini-host");
    let rt = runtime(&dir, &GEMINI, &credential);
    writes_a_file(&GEMINI, &rt, "", HOST_ENV).await;
    let dir = host_dir("gemini-host-hook");
    let rt = runtime(&dir, &GEMINI, &credential);
    a_hook_blocks_the_write(&GEMINI, &rt, HOST_ENV).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs ANTHROPIC_API_KEY, the Docker plugin and a daemon"]
async fn claude_code_writes_a_file_in_a_container() {
    let Some(credential) = CLAUDE.require(true) else {
        return;
    };
    if plugin_path().is_none() || product_image().await.is_none() {
        return;
    }
    let dir = host_dir("claude-docker");
    let rt = runtime(&dir, &CLAUDE, &credential);
    writes_a_file(&CLAUDE, &rt, CONTAINER_TOML, &[]).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "live: needs GEMINI_API_KEY, the Docker plugin and a daemon"]
async fn gemini_cli_writes_a_file_in_a_container() {
    let Some(credential) = GEMINI.require(true) else {
        return;
    };
    if plugin_path().is_none() || product_image().await.is_none() {
        return;
    }
    let dir = host_dir("gemini-docker");
    let rt = runtime(&dir, &GEMINI, &credential);
    writes_a_file(&GEMINI, &rt, CONTAINER_TOML, &[]).await;
}
