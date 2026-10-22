//! Readiness item 9b (milestone C2) through the shipped binary: a
//! `[run.agent.mcps]` server configured in `workflow.toml` is started by
//! Petri, its tool is called by the twin's scripted model under Fabro's
//! qualified name, the effect lands in the workspace, the result reaches the
//! next model request, hooks apply, failures and cancellation are handled,
//! and the server is gone when the run ends. Provider twins on loopback, the
//! scripted `mcp_server.py` as the server, no Fabro, no live provider.

mod support;

use std::path::{Path, PathBuf};
use std::{env, fs};

use serde_json::{Value, json};
use support::fabro::launch::{Case, Launch};
use support::fabro::twins::{Provider, Twin, model, scenario, text, tool_call};

/// The scripted server, versioned under the acceptance crate's test data.
fn server_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fabro/acceptance/testdata/mcp_server.py")
        .canonicalize()
        .expect("the scripted MCP server exists")
}

/// A `stdio` entry for the scripted server, tagged with the case's run dir
/// so a leaked server process fails `assert_no_leaked_processes`, logging
/// its lifecycle to `log`.
fn stdio_entry(name: &str, case: &Case, log: &Path, extra_args: &[&str], extra: &str) -> String {
    let mut args = vec![
        "python3".to_owned(),
        server_script().display().to_string(),
        "--tag".to_owned(),
        case.run_dir.display().to_string(),
    ];
    args.extend(extra_args.iter().map(|arg| (*arg).to_owned()));
    let command = serde_json::to_string(&args).expect("argv");
    format!(
        "[run.agent.mcps.{name}]\ntype = \"stdio\"\ncommand = {command}\nenv = {{ MCP_TEST_LOG = {:?} }}\n{extra}\n",
        log.display()
    )
}

fn agent_workflow(provider: Provider, attrs: &str, after: &str) -> String {
    format!(
        r#"digraph Mcp {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Take a note with the notes server.", model="{}", provider="{}", on_failure="exit" {attrs}]
    {after}
}}"#,
        model(provider),
        provider.id()
    )
}

/// The tool results a request carries back to the model, per wire shape.
fn tool_outputs(provider: Provider, request: &Value) -> Vec<String> {
    match provider {
        Provider::OpenAi | Provider::OpenRouter => request["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "function_call_output")
            .filter_map(|item| item["output"].as_str().map(str::to_owned))
            .collect(),
        Provider::Anthropic => request["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
            .filter(|part| part["type"] == "tool_result")
            .map(|part| match &part["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            })
            .collect(),
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Every file under `root`, for the assertion that a value never reached
/// the run directory.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

async fn effect_case(provider: Provider, label: &str) {
    let mut case = Case::new(label);
    let log = case.root.join("mcp.log");
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "write",
            model(provider),
            "Take a note",
            tool_call(
                "write",
                "mcp__notes__write_file",
                json!({ "path": "note.txt", "content": "hello from mcp\n" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model(provider),
            "wrote 15 bytes to note.txt",
            text("Noted."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &agent_workflow(
            provider,
            "",
            r#"verify [shape=parallelogram, script="cat note.txt"]
    start -> agent -> verify -> exit"#,
        ),
        Some(&stdio_entry("notes", &case, &log, &[], "")),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    // The effect: the server ran in the workspace and wrote the file.
    assert_eq!(read(&case.workspace().join("note.txt")), "hello from mcp\n");
    assert_eq!(twin.consumed(), ["write", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        tool_outputs(provider, &requests[1]),
        ["wrote 15 bytes to note.txt"],
        "the tool result reached the next request: {}",
        requests[1]
    );
    // The model was offered the tool under Fabro's qualified name.
    let first = serde_json::to_string(&requests[0]).expect("request");
    assert!(first.contains("\"mcp__notes__write_file\""), "{first}");
    assert_eq!(
        read(&log),
        "started\ninitialize\ncall write_file\nshutdown\n",
        "the server saw the handshake, one call and a clean shutdown"
    );
    let context = finished.final_context();
    assert_eq!(context["response.agent"], json!("Noted."));
    let echoed = finished.echoed();
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "verify" && line == "hello from mcp"),
        "{echoed:?}"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A configured `stdio` server exposes a tool the OpenAI twin's model calls
/// by its qualified name; the file appears in the workspace, the result
/// reaches the next request, the server stops with the run.
#[tokio::test]
async fn a_configured_stdio_server_exposes_a_tool_that_writes_into_the_workspace() {
    effect_case(Provider::OpenAi, "mcp-effect-openai").await;
}

/// The same through the Anthropic twin: the qualified name is unchanged by
/// Claude's tool vocabulary.
#[tokio::test]
async fn the_same_tool_reaches_the_model_through_native_anthropic() {
    effect_case(Provider::Anthropic, "mcp-effect-anthropic").await;
}

/// A configured `pre_tool_use` hook blocks an MCP tool by its qualified
/// name: the server never sees the call, the model sees the reason, and a
/// later allowed call runs.
#[tokio::test]
async fn a_pre_tool_use_hook_blocks_an_mcp_tool() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("mcp-hook-block");
    let log = case.root.join("mcp.log");
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "secret",
            model(provider),
            "Take a note",
            tool_call(
                "secret",
                "mcp__notes__write_file",
                json!({ "path": "secret.txt", "content": "leak" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "safe",
            model(provider),
            "secret files are off limits",
            tool_call(
                "safe",
                "mcp__notes__write_file",
                json!({ "path": "ok.txt", "content": "fine" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model(provider),
            "wrote 4 bytes to ok.txt",
            text("Kept the safe note."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let toml = format!(
        r#"{}
[[run.hooks]]
name = "no-secrets"
event = "pre_tool_use"
matcher = "^mcp__notes__"
script = "if grep -q secret.txt \"$FABRO_HOOK_CONTEXT\"; then echo '{{\"decision\":\"block\",\"reason\":\"secret files are off limits\"}}'; exit 2; fi"

[[run.hooks]]
name = "log-tools"
event = "post_tool_use"
matcher = "^mcp__"
script = "echo ran:$FABRO_NODE_ID >> tool-hooks.log"
"#,
        stdio_entry("notes", &case, &log, &[], "")
    );
    let workflow = case.workflow(
        &agent_workflow(provider, "", "start -> agent -> exit"),
        Some(&toml),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let workspace = case.workspace();
    assert!(
        !workspace.join("secret.txt").exists(),
        "the blocked call never reached the server"
    );
    assert_eq!(read(&workspace.join("ok.txt")), "fine");
    assert_eq!(read(&workspace.join("tool-hooks.log")), "ran:agent\n");
    assert_eq!(twin.consumed(), ["secret", "safe", "answer"]);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(tool_outputs(provider, &requests[1]), [
        "secret files are off limits"
    ]);
    assert_eq!(
        read(&log),
        "started\ninitialize\ncall write_file\nshutdown\n",
        "the server saw one call"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A result the server marks as an error reaches the model as the tool's
/// error; a server that exits mid-session fails the call with a reason and
/// the run continues.
#[tokio::test]
async fn an_error_result_and_a_crashed_server_reach_the_model_with_reasons() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("mcp-failures");
    let log = case.root.join("mcp.log");
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "fail",
            model(provider),
            "Take a note",
            tool_call(
                "fail",
                "mcp__notes__fail",
                json!({ "message": "disk full" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "crash",
            model(provider),
            "disk full",
            tool_call("crash", "mcp__notes__crash", json!({})),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model(provider),
            "failed the call to `crash`",
            text("The notes server is gone."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &agent_workflow(provider, "", "start -> agent -> exit"),
        Some(&stdio_entry("notes", &case, &log, &[], "")),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["fail", "crash", "answer"]);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(tool_outputs(provider, &requests[1]), ["disk full"]);
    let outputs = tool_outputs(provider, &requests[2]);
    assert_eq!(outputs.len(), 2, "{outputs:?}");
    assert!(
        outputs[1].contains("MCP server `notes` failed the call to `crash`"),
        "{outputs:?}"
    );
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("The notes server is gone.")
    );
    assert_eq!(
        read(&log),
        "started\ninitialize\ncall fail\ncall crash\ncrash\n"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Ctrl-C while an MCP call waits on the server: the run is cancelled and
/// the server process is gone.
#[tokio::test]
async fn cancellation_stops_a_slow_mcp_call_and_its_server() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("mcp-cancel");
    let log = case.root.join("mcp.log");
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "start",
            model(provider),
            "Take a note",
            tool_call(
                "start",
                "mcp__notes__write_file",
                json!({ "path": "started.txt", "content": "go" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "slow",
            model(provider),
            "wrote 2 bytes to started.txt",
            // The server touches `sleeping.txt` as the call arrives, and
            // the interrupt waits for that: `started.txt` exists before the
            // model has even asked for the slow call, so an interrupt on it
            // could close the server before the call reached it.
            tool_call(
                "slow",
                "mcp__notes__sleep",
                json!({ "ms": 60000, "marker": "sleeping.txt" }),
            ),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &agent_workflow(provider, "", "start -> agent -> exit"),
        Some(&stdio_entry("notes", &case, &log, &[], "")),
    );
    let finished = case
        .run_with(&workflow, &[], Launch {
            interrupt_when: Some(case.workspace().join("sleeping.txt")),
            ..Launch::default()
        })
        .await;
    assert!(!finished.timed_out, "the interrupt ended the run");
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["start", "slow"]);
    let log = read(&log);
    assert!(
        log.starts_with("started\ninitialize\ncall write_file\ncall sleep\n"),
        "{log}"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Two `full` nodes on one thread: each starts its own server and registers
/// the tool again, and the second node's request carries the first node's
/// MCP tool exchange.
#[tokio::test]
async fn a_retained_thread_carries_mcp_tool_calls_into_the_next_node() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("mcp-retained");
    let log = case.root.join("mcp.log");
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "one",
            model(provider),
            "Plan the notes",
            tool_call(
                "one",
                "mcp__notes__echo",
                json!({ "message": "first note" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "plan",
            model(provider),
            "first note",
            text("PLAN: keep notes"),
        ),
        scenario(
            provider,
            &case.credential,
            "two",
            model(provider),
            "Implement the notes",
            tool_call(
                "two",
                "mcp__notes__echo",
                json!({ "message": "second note" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "done",
            model(provider),
            "second note",
            text("DONE"),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Threads {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    plan [prompt="Plan the notes.", model="{model}", provider="openai", fidelity="full", thread_id="notes"]
    implement [prompt="Implement the notes.", model="{model}", provider="openai", fidelity="full", thread_id="notes"]
    start -> plan -> implement -> exit
}}"#,
            model = model(provider)
        ),
        Some(&stdio_entry("notes", &case, &log, &[], "")),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["one", "plan", "two", "done"]);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 4);
    let third = serde_json::to_string(&requests[2]).expect("request");
    assert!(
        third.contains("first note") && third.contains("PLAN: keep notes"),
        "the second node continues the conversation with its MCP calls: {third}"
    );
    assert_eq!(tool_outputs(provider, &requests[3]), [
        "first note",
        "second note"
    ]);
    assert_eq!(
        read(&log),
        "started\ninitialize\ncall echo\nshutdown\nstarted\ninitialize\ncall echo\nshutdown\n",
        "one server lifetime per node"
    );
    assert_eq!(
        finished.final_context()["response.implement"],
        json!("DONE")
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A server that does not start is reported on the terminal with its own
/// error output and the run continues with the servers that did.
#[tokio::test]
async fn a_server_that_fails_to_start_is_reported_and_the_run_continues() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("mcp-start-failure");
    let log = case.root.join("mcp.log");
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "echo",
            model(provider),
            "Take a note",
            tool_call(
                "echo",
                "mcp__notes__echo",
                json!({ "message": "still here" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model(provider),
            "still here",
            text("Only the notes server answered."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let toml = format!(
        "{}{}",
        stdio_entry("broken", &case, &log, &["--fail-init"], ""),
        stdio_entry("notes", &case, &log, &[], "")
    );
    let workflow = case.workflow(
        &agent_workflow(provider, "", "start -> agent -> exit"),
        Some(&toml),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let echoed = finished.echoed();
    assert!(
        echoed.iter().any(|(node, line)| {
            node == "agent"
                && line.starts_with("mcp server `broken` failed to start:")
                && line.contains("refusing to start: --fail-init")
        }),
        "the failure and the server's stderr reach the terminal: {echoed:?}"
    );
    assert_eq!(twin.consumed(), ["echo", "answer"]);
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A `{{ secrets.NAME }}` value in the server's `env` reaches the server
/// process and nothing else: not the terminal, not the run directory.
#[tokio::test]
async fn a_secret_reaches_the_server_environment_and_stays_masked() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("mcp-secret");
    let log = case.root.join("mcp.log");
    let value = "mcp-token-3f9a2c7e1b";
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "env",
            model(provider),
            "Take a note",
            tool_call(
                "env",
                "mcp__notes__echo",
                json!({ "message": "__env:NOTES_TOKEN__" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model(provider),
            value,
            text("The server has its token."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let toml = format!(
        "{}[run.agent.mcps.notes.env]\nNOTES_TOKEN = \"{{{{ secrets.NOTES_TOKEN }}}}\"\n",
        stdio_entry("notes", &case, &log, &[], "").replace(
            &format!("env = {{ MCP_TEST_LOG = {:?} }}\n", log.display()),
            ""
        )
    );
    let workflow = case.workflow(
        &agent_workflow(provider, "", "start -> agent -> exit"),
        Some(&toml),
    );
    let finished = case
        .run_with(&workflow, &[], Launch {
            env: vec![("PETRI_SECRET_NOTES_TOKEN".into(), value.into())],
            ..Launch::default()
        })
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    // The server received the value (the twin saw it in the tool result)...
    assert_eq!(twin.consumed(), ["env", "answer"]);
    // ...and it never reached the terminal or the run directory.
    assert!(!finished.stderr.contains(value), "{}", finished.stderr);
    assert!(!finished.stdout.contains(value), "{}", finished.stdout);
    for path in files_under(&case.run_dir) {
        let bytes = fs::read(&path).unwrap_or_default();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(value),
            "the secret reached {}",
            path.display()
        );
    }
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A catalog reference is refused before any node runs, with the specific
/// diagnostic; a malformed entry (here, both `script` and `command`, and a
/// `protocol` that is neither of Fabro's two) is an error.
#[tokio::test]
async fn unsupported_mcp_settings_are_refused_before_the_run() {
    let provider = Provider::OpenAi;
    for (label, toml, code) in [
        (
            "mcp-reference",
            "[run.agent.mcps.sentry]\nid = \"sentry\"\n",
            "unsupported.workflow_toml.run.agent.mcps.reference",
        ),
        (
            "mcp-malformed",
            "[run.agent.mcps.files]\ntype = \"stdio\"\nscript = \"a\"\ncommand = [\"b\"]\n",
            "fabro.mcps.entry",
        ),
        (
            "mcp-protocol",
            "[run.agent.mcps.legacy]\ntype = \"http\"\nurl = \"http://127.0.0.1:1/mcp\"\nprotocol = \"websocket\"\n",
            "fabro.mcps.entry",
        ),
    ] {
        let case = Case::new(label);
        let workflow = case.workflow(
            &agent_workflow(provider, "", "start -> agent -> exit"),
            Some(toml),
        );
        let finished = case.run(&workflow, &[]).await;
        finished.assert_code(1);
        assert!(
            finished.stderr.contains(code),
            "{label}: {}",
            finished.stderr
        );
        assert!(
            finished.status_line().is_none(),
            "{label}: no node ran: {}",
            finished.stderr
        );
    }
}

/// A `sandbox` server inside a Docker scope. Petri launches the server in
/// the container and reaches it through the route the Docker plugin opens
/// (a forward on Petri's loopback bridged into the container; nothing is
/// published on the daemon); the tool's effect lands in the container's
/// workspace, the result reaches the next model request, and the server is
/// stopped before the next stage runs.
#[tokio::test]
async fn a_sandbox_server_in_a_docker_scope_is_reached_through_the_plugins_forward() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let provider = Provider::OpenAi;
    let mut case = Case::new("mcp-docker-sandbox").docker();
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "write",
            model(provider),
            "Take a note",
            tool_call(
                "write",
                "mcp__scoped__write_file",
                json!({ "path": "note.txt", "content": "hello from the box\n" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "answer",
            model(provider),
            "wrote 19 bytes to note.txt",
            text("Noted in the box."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    // The container's workspace starts empty and nothing on the host mirrors
    // it, so a setup command writes the scripted server into it first.
    let script = fs::read_to_string(server_script()).expect("the scripted server is readable");
    assert!(!script.lines().any(|line| line == "PETRI_MCP_SERVER"));
    let setup = serde_json::to_string(&format!(
        "cat > mcp_server.py <<'PETRI_MCP_SERVER'\n{}\nPETRI_MCP_SERVER\n",
        script.trim_end()
    ))
    .expect("quote the setup script for DOT");
    let toml = "[run.agent.mcps.scoped]\ntype = \"sandbox\"\ncommand = [\"python3\", \"mcp_server.py\", \"--http\", \"8765\"]\nport = 8765\nenv = { MCP_TEST_LOG = \"mcp.log\" }\nstartup_timeout = \"60s\"\n";
    let workflow = case.workflow(
        &agent_workflow(
            provider,
            "",
            &format!(r#"install_server [shape=parallelogram, script={setup}, timeout="5m", on_failure="exit"]
    verify [shape=parallelogram, script="cat note.txt; cat mcp.log"]
    start -> install_server -> agent -> verify -> exit"#),
        ),
        Some(toml),
    );
    let finished = case.run(&workflow, &["--retain", "never"]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["write", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        tool_outputs(provider, &requests[1]),
        ["wrote 19 bytes to note.txt"],
        "the tool result reached the next request: {}",
        requests[1]
    );
    let echoed = finished.echoed();
    assert!(
        !echoed
            .iter()
            .any(|(_, line)| line.contains("failed to start")),
        "the server started through the forward: {echoed:?}"
    );
    let verify: Vec<&str> = echoed
        .iter()
        .filter(|(node, _)| node == "verify")
        .map(|(_, line)| line.as_str())
        .collect();
    assert_eq!(
        verify,
        [
            "hello from the box",
            "started",
            "initialize",
            "call write_file"
        ],
        "the effect and the server's own log are in the container's workspace: {}",
        finished.stderr
    );
    assert_eq!(
        finished.final_context()["response.agent"],
        json!("Noted in the box.")
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}
