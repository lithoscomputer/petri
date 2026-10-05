//! Readiness item 10 through the embedding boundary: the combined readiness
//! workflow of `crates/petri/cli/tests/fabro_readiness_blackbox.rs` (the
//! same workflow text, `workflow.toml`, twin scripts and hooks), run
//! in-process by a host that replaces the terminal and the services. The
//! host installs its own `ExecutionHooks` (which wraps the hooks `register`
//! installed, the local hook service behind its adapter, pauses the fan-out's
//! admission once and records a marker note at every point), its own
//! interviewer, and its own event sink, and rebuilds the run from the public
//! events alone. The workflow's semantics are the
//! ones the binary showed: the same files, the same twin scripts spent in the
//! same order, the same status, every item 9 family on the stream, and a
//! replay identical to the live stream.
//!
//! Real Pebble, real shell tools, the scripted `mcp_server.py`, the provider
//! twins on loopback. No Fabro, no database, no server.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command as GitCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use std::{env, fs};

use lithos_llm::credentials::{CredentialHeader, Credentials, SecretValue, StaticCredentials};
use petri::attractor::pebble::PebbleClient;
use petri::attractor::register;
use petri::attractor::skills::FabroHome;
use petri::driver::lifecycle::{
    AdmitAttempt, AttemptDecision, ExecutionHooks, HookContext, Note, PrepareError, PrepareResult,
    Prepared, Recorded, RunFinished, ScopeReleased, Transition, TransitionError, TransitionReport,
};
use petri::engine::Event;
use petri::execution::events::{
    CollectingSink, Derived, EventProjector, Parsed, RunEvent, ViewEvent, replay_run_dir,
};
use petri::execution::host::{self, HostRun};
use petri::execution::{
    CoordinatorEvent, ExecutionObserver, InterviewDispatcher, InterviewReply, InterviewRequest,
    Interviewer,
};
use petri::executor::Retention;
use petri::frontend::CompileInputs;
use petri::frontend::fabro::Fabro;
use petri::ir::{FinalizationFailure, RunStatus};
use petri::steps::Answer;
use petri::{LlmClientConfig, RunOptions, Runtime, build_llm_client};
use serde_json::{Map, Value, json};
use testkit::{RunDir, backend_event};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use tokio_util::sync::CancellationToken;
use twin_anthropic::config::Config as AnthropicConfig;
use twin_openai::config::Config as OpenAiConfig;

// ── The twins, served in-process ───────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    OpenAi,
    Anthropic,
}

impl Provider {
    fn id(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Self::OpenAi => "responses",
            Self::Anthropic => "messages",
        }
    }

    fn model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-5.6-sol",
            Self::Anthropic => "claude-sonnet-5",
        }
    }
}

/// One running twin in strict fixture mode: unmatched calls fail, every
/// request lands in a JSONL log the test reads back.
struct Twin {
    provider: Provider,
    base_url: String,
    log_path: PathBuf,
    task:     JoinHandle<()>,
}

impl Twin {
    async fn start(provider: Provider, dir: &Path, scenarios: Vec<Value>) -> Self {
        fs::create_dir_all(dir).expect("twin dir");
        let scenarios_path = dir.join(format!("{}-scenarios.json", provider.id()));
        fs::write(
            &scenarios_path,
            serde_json::to_vec_pretty(&json!({ "scenarios": scenarios })).expect("scenarios"),
        )
        .expect("write scenarios");
        let log_path = dir.join(format!("{}-requests.jsonl", provider.id()));
        let app = match provider {
            Provider::OpenAi => {
                let mut config = OpenAiConfig::from_lookup(&|_| None).expect("twin-openai config");
                config.scenarios_path = Some(scenarios_path);
                config.request_log_path = Some(log_path.clone());
                config.allow_unmatched = false;
                config.require_auth = true;
                twin_openai::build_app_with_config(config).expect("twin-openai app")
            }
            Provider::Anthropic => {
                let mut config =
                    AnthropicConfig::from_lookup(&|_| None).expect("twin-anthropic config");
                config.scenarios_path = Some(scenarios_path);
                config.request_log_path = Some(log_path.clone());
                config.allow_unmatched = false;
                config.require_auth = true;
                twin_anthropic::build_app_with_config(config).expect("twin-anthropic app")
            }
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind twin");
        let base_url = format!("http://{}", listener.local_addr().expect("twin address"));
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                panic!("twin server failed: {error}");
            }
        });
        Self {
            provider,
            base_url,
            log_path,
            task,
        }
    }

    fn request_log(&self) -> Vec<Value> {
        let text = fs::read_to_string(&self.log_path).unwrap_or_default();
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect("request log line"))
            .collect()
    }

    /// The scenario ids spent, in request order.
    fn consumed(&self) -> Vec<String> {
        self.request_log()
            .iter()
            .filter_map(|record| record["scenario_id"].as_str().map(str::to_owned))
            .collect()
    }

    fn unmatched(&self) -> usize {
        self.request_log()
            .iter()
            .filter(|record| record["scenario_id"].is_null())
            .count()
    }

    fn catalog_layer(&self) -> String {
        format!(
            "schema_version = 1\n[providers.{}]\nbase_url = {:?}\n",
            self.provider.id(),
            self.base_url
        )
    }

    fn stop(self) {
        self.task.abort();
    }
}

fn scenario(
    provider: Provider,
    namespace: &str,
    id: &str,
    input_contains: &str,
    script: Value,
) -> Value {
    let mut scenario = Map::new();
    scenario.insert("scenario_id".into(), json!(id));
    scenario.insert("namespace".into(), json!(namespace));
    scenario.insert(
        "matcher".into(),
        json!({
            "endpoint": provider.endpoint(),
            "model": provider.model(),
            "input_contains": input_contains,
        }),
    );
    scenario.insert("script".into(), script);
    Value::Object(scenario)
}

fn text(text: &str) -> Value {
    json!({
        "kind": "success",
        "response_text": text,
        "usage": { "input_tokens": 10, "output_tokens": 5 },
    })
}

fn tool_call(id: &str, name: &str, arguments: &Value) -> Value {
    json!({
        "kind": "success",
        "tool_calls": [{ "id": id, "name": name, "arguments": arguments }],
        "usage": { "input_tokens": 10, "output_tokens": 5 },
    })
}

fn spawn_and_wait(task: &str) -> Value {
    json!({
        "kind": "success",
        "tool_calls": [
            { "id": "spawn-0", "name": "spawn_agent", "arguments": { "task": task } },
            { "id": "wait", "name": "wait", "arguments": {} },
        ],
        "usage": { "input_tokens": 10, "output_tokens": 5 },
    })
}

fn error(status: u16, error_type: &str, code: &str, message: &str) -> Value {
    json!({
        "kind": "error",
        "status": status,
        "error_type": error_type,
        "code": code,
        "message": message,
    })
}

// ── The workflow (the black box suite's text) ──────────────────────────────

const MEMORY_RULE: &str = "Always sign release notes with -- petri";
const OVER_THRESHOLD: u64 = 900_000;
const SHELL: &str = "shell_command";

fn workflow(model: &str) -> String {
    format!(
        r#"digraph Readiness {{
    graph [backend="api", goal="Ship the release note"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    plan [prompt="Plan the release note with the sign skill.", model="{model}", provider="openai", fidelity="full", thread_id="notes", on_failure="exit"]
    write [prompt="Write the release note with the notes server.", model="{model}", provider="openai", fidelity="full", thread_id="notes", on_failure="exit"]
    delegate [prompt="Delegate the changelog to a child.", model="{model}", provider="openai", fidelity="full", thread_id="notes", on_failure="exit"]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no"]
    jobs [shape=parallelogram, output_schema="routing", script="printf '%s' '{{\"context_updates\":{{\"jobs\":[{{\"name\":\"alpha\"}},{{\"name\":\"beta\"}}]}}}}'"]
    fan [shape=component, for_each="context.jobs", max_parallel=2]
    job [prompt="Review the item and report one finding.", model="{model}", provider="openai", output_schema="routing"]
    join [shape=tripleoctagon]
    report [shape=parallelogram, script="cat > results.json", stdin_source="context.parallel.results"]
    polish [prompt="Polish the note.", model="{model}", provider="openai", fidelity="full", thread_id="notes", on_failure="exit"]
    review [prompt="Review the polished note.", model="{model}", provider="openai", fidelity="full", thread_id="notes", on_failure="exit"]
    draft_docs [prompt="Draft the docs page.", model="{model}", provider="openai", fidelity="full", thread_id="docs", on_failure="exit"]
    finish_docs [prompt="Finish the docs page.", model="{model}", provider="openai", fidelity="full", thread_id="docs", on_failure="exit"]
    hold [shape=parallelogram, script="echo held > decision.txt"]
    check [shape=parallelogram, script="cat notes.txt note.txt CHANGELOG.md results.json f5.txt tool-hooks.log"]
    start -> plan -> write -> delegate -> gate
    gate -> jobs [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    jobs -> fan -> job -> join -> report -> polish -> review -> draft_docs -> finish_docs -> check -> exit
    hold -> exit
}}"#
    )
}

fn server_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fabro/acceptance/testdata/mcp_server.py")
        .canonicalize()
        .expect("the scripted MCP server exists")
}

fn workflow_toml(run_dir: &Path, remote: &Path, mcp_log: &Path) -> String {
    let command = serde_json::to_string(&[
        "python3".to_owned(),
        server_script().display().to_string(),
        "--tag".to_owned(),
        run_dir.display().to_string(),
    ])
    .expect("argv");
    format!(
        r#"
[run.model.fallbacks]
"gpt-5.6-sol" = ["anthropic:claude-sonnet-5"]

[run.prepare]
timeout = "30s"

[[run.prepare.steps]]
script = 'git init -q && printf "draft\n" > notes.txt && printf "keep\n" > protected.txt && printf "{memory}\n" > AGENTS.md'

[[run.prepare.steps]]
script = 'mkdir -p skills/sign && printf "%s\n" "---" "name: sign" "description: Sign the release note" "---" "SIGN INSTRUCTIONS: append the line -- petri to notes.txt with the shell tool, then report SIGNED." > skills/sign/SKILL.md'

[[run.prepare.steps]]
script = 'git add -A && git -c commit.gpgsign=false -c core.hooksPath=/dev/null -c user.name=petri -c user.email=petri@example.invalid commit -qm base && git init -q --bare "{remote}" && git remote add origin "{remote}" && echo prepared'

[run.agent.mcps.notes]
type = "stdio"
command = {command}
env = {{ MCP_TEST_LOG = {mcp_log:?} }}

[[run.hooks]]
name = "no-destruction"
event = "pre_tool_use"
matcher = "shell|Bash"
script = "if grep -q 'rm ' \"$FABRO_HOOK_CONTEXT\"; then echo '{{\"decision\":\"block\",\"reason\":\"destructive commands are not allowed\"}}'; exit 2; fi"

[[run.hooks]]
name = "no-protected"
event = "pre_tool_use"
matcher = "^mcp__notes__"
script = "if grep -q protected.txt \"$FABRO_HOOK_CONTEXT\"; then echo '{{\"decision\":\"block\",\"reason\":\"protected files are off limits\"}}'; exit 2; fi"

[[run.hooks]]
name = "log-tools"
event = "post_tool_use"
matcher = "shell|Bash|^mcp__|use_skill"
script = "echo ran:$FABRO_NODE_ID >> tool-hooks.log"

[[run.hooks]]
event = "run_complete"
script = "echo run_complete >> run-end.log"

[[run.hooks]]
event = "sandbox_cleanup"
script = "echo sandbox_cleanup >> run-end.log"
"#,
        memory = MEMORY_RULE,
        remote = remote.display(),
        mcp_log = mcp_log.display().to_string(),
    )
}

fn openai_scripts(credential: &str) -> Vec<Value> {
    let s = |id: &str, matcher: &str, script: Value| {
        scenario(Provider::OpenAi, credential, id, matcher, script)
    };
    let heavy = |mut scenario: Value| {
        scenario["script"]["usage"] = json!({ "input_tokens": OVER_THRESHOLD, "output_tokens": 5 });
        scenario
    };
    let finding = |item: &str| {
        s(
            &format!("job-{item}"),
            &format!("\"name\": \"{item}\""),
            text(&format!(
                r#"{{"outcome":"succeeded","context_updates":{{"output.finder":{{"found":"{item}"}}}}}}"#
            )),
        )
    };
    vec![
        s(
            "plan-load",
            "Plan the release note",
            tool_call("plan-load", "use_skill", &json!({ "skill_name": "sign" })),
        ),
        s(
            "plan-sign",
            "SIGN INSTRUCTIONS",
            tool_call(
                "plan-sign",
                SHELL,
                &json!({ "command": "printf -- '-- petri\\n' >> notes.txt && echo SIGNED" }),
            ),
        ),
        s("plan-done", "SIGNED", text("PLANNED: a signed note")),
        s(
            "write-protected",
            "Write the release note",
            tool_call(
                "write-protected",
                "mcp__notes__write_file",
                &json!({ "path": "protected.txt", "content": "overwrite" }),
            ),
        ),
        s(
            "write-note",
            "protected files are off limits",
            tool_call(
                "write-note",
                "mcp__notes__write_file",
                &json!({ "path": "note.txt", "content": "release note\n" }),
            ),
        ),
        s(
            "write-done",
            "wrote 13 bytes to note.txt",
            text("WROTE the note"),
        ),
        s(
            "delegate",
            "Delegate the changelog",
            spawn_and_wait("child: write CHANGELOG.md"),
        ),
        s(
            "child-destroy",
            "child: write CHANGELOG.md",
            tool_call(
                "child-destroy",
                SHELL,
                &json!({ "command": "rm -f protected.txt && echo REMOVED" }),
            ),
        ),
        s(
            "child-second",
            "destructive commands are not allowed",
            tool_call(
                "child-write",
                SHELL,
                &json!({ "command": "printf 'changelog\\n' > CHANGELOG.md && echo CHANGED" }),
            ),
        ),
        s("child-done", "CHANGED", text("Wrote CHANGELOG.md.")),
        s(
            "synthesize",
            "Wrote CHANGELOG.md.",
            text("DELEGATED the changelog"),
        ),
        finding("alpha"),
        finding("beta"),
        s(
            "r1",
            "Polish the note",
            tool_call(
                "r1",
                SHELL,
                &json!({ "command": "printf one > f1.txt && echo OUT1" }),
            ),
        ),
        s(
            "r2",
            "OUT1",
            tool_call(
                "r2",
                SHELL,
                &json!({ "command": "printf two > f2.txt && echo OUT2" }),
            ),
        ),
        s(
            "r3",
            "OUT2",
            tool_call(
                "r3",
                SHELL,
                &json!({ "command": "printf three > f3.txt && echo OUT3" }),
            ),
        ),
        s(
            "r4",
            "OUT3",
            tool_call(
                "r4",
                SHELL,
                &json!({ "command": "printf four > f4.txt && echo OUT4" }),
            ),
        ),
        heavy(s(
            "r5",
            "OUT4",
            tool_call(
                "r5",
                SHELL,
                &json!({ "command": "printf five > f5.txt && echo OUT5" }),
            ),
        )),
        s(
            "summary",
            "Here is the conversation to summarize",
            text("HANDOFF: the note is signed, written and delegated; f1..f5 written."),
        ),
        s("polish-done", "OUT5", text("POLISHED")),
        s(
            "review-note",
            "Review the polished note",
            tool_call(
                "review-note",
                "mcp__notes__write_file",
                &json!({ "path": "review.txt", "content": "reviewed\n" }),
            ),
        ),
        s("review", "wrote 9 bytes to review.txt", text("REVIEWED")),
        s(
            "docs-down",
            "Draft the docs page",
            error(503, "server_error", "service_unavailable", "gone away"),
        ),
    ]
}

fn anthropic_scripts(credential: &str) -> Vec<Value> {
    vec![
        scenario(
            Provider::Anthropic,
            credential,
            "docs-draft",
            "Draft the docs page",
            text("DRAFT docs on the fallback"),
        ),
        scenario(
            Provider::Anthropic,
            credential,
            "docs-finish",
            "Finish the docs page",
            text("FINISHED docs"),
        ),
    ]
}

/// The OpenAI scripts in the order the binary spent them
/// (`fabro_readiness_blackbox.rs`), the two branch findings in either order.
const BEFORE_FAN_OUT: [&str; 11] = [
    "plan-load",
    "plan-sign",
    "plan-done",
    "write-protected",
    "write-note",
    "write-done",
    "delegate",
    "child-destroy",
    "child-second",
    "child-done",
    "synthesize",
];
const AFTER_FAN_OUT: [&str; 10] = [
    "r1",
    "r2",
    "r3",
    "r4",
    "r5",
    "summary",
    "polish-done",
    "review-note",
    "review",
    "docs-down",
];

// ── The embedding host ─────────────────────────────────────────────────────

/// Answers every question with the first choice, as the binary's interview
/// script did for the gate.
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

/// The host's `ExecutionHooks`: wraps the local hook service (so
/// `[[run.hooks]]` still run, once), holds the fan-out's admission until
/// released, and records a marker note at every point so the order of its
/// callbacks is in the durable record and on the public stream.
struct EmbeddingHost {
    inner:   Arc<dyn ExecutionHooks>,
    pause:   &'static str,
    paused:  AtomicU32,
    release: Notify,
}

impl EmbeddingHost {
    fn marker(phase: &str, node: &str) -> Note {
        Note::new(format!("embedding_host:{phase}"), json!({ "node": node }))
    }
}

#[async_trait::async_trait]
impl ExecutionHooks for EmbeddingHost {
    async fn before_attempt(
        &self,
        context: &HookContext,
        request: AdmitAttempt,
    ) -> AttemptDecision {
        let node = request.view.node_name().to_owned();
        if node == self.pause && self.paused.fetch_add(1, Ordering::SeqCst) == 0 {
            self.release.notified().await;
        }
        let mut decision = self.inner.before_attempt(context, request).await;
        decision.notes.push(Self::marker("before_attempt", &node));
        decision
    }

    async fn prepare_result(
        &self,
        context: &HookContext,
        request: PrepareResult,
    ) -> Result<Prepared, PrepareError> {
        let node = request.view.node_name().to_owned();
        let mut prepared = self.inner.prepare_result(context, request).await?;
        prepared.notes.push(Self::marker("prepare_result", &node));
        Ok(prepared)
    }

    async fn after_record(&self, context: &HookContext, recorded: Recorded) -> Vec<Note> {
        let node = recorded.view.node_name().to_owned();
        let mut notes = self.inner.after_record(context, recorded).await;
        notes.push(Self::marker("after_record", &node));
        notes
    }

    async fn transition(
        &self,
        context: &HookContext,
        transition: Transition,
    ) -> Result<TransitionReport, TransitionError> {
        let node = transition.view.node_name().to_owned();
        let mut report = self.inner.transition(context, transition).await?;
        report.notes.push(Self::marker("transition", &node));
        Ok(report)
    }

    fn requires_run_finalization(&self) -> bool {
        self.inner.requires_run_finalization()
    }

    async fn finalize_run(
        &self,
        context: &HookContext,
        finished: RunFinished,
    ) -> Result<(), FinalizationFailure> {
        self.inner.finalize_run(context, finished).await
    }

    async fn run_finished(&self, context: &HookContext, finished: RunFinished) -> Vec<Note> {
        self.inner.run_finished(context, finished).await
    }

    async fn scope_released(&self, context: &HookContext, released: ScopeReleased) -> Vec<Note> {
        self.inner.scope_released(context, released).await
    }
}

// ── Reconstruction from public events ──────────────────────────────────────

/// What the host rebuilds from the stream, and nothing else.
#[derive(Debug, Default)]
struct Projected {
    run_status:      Option<RunStatus>,
    finals:          BTreeMap<String, String>,
    kinds:           BTreeMap<(String, String), usize>,
    markers:         BTreeMap<String, Vec<String>>,
    questions:       Vec<String>,
    answers:         Vec<String>,
    invocations:     BTreeSet<u64>,
    expansions:      usize,
    branch_children: usize,
    subagent:        Vec<String>,
    parent_named:    bool,
}

fn project(events: &[RunEvent]) -> Projected {
    let mut out = Projected::default();
    let mut delegate_session: Option<String> = None;
    for event in events {
        if let Some(invocation) = event.context.invocation {
            out.invocations.insert(invocation.raw());
        }
        let subject = event.subject.as_ref();
        let synthetic =
            subject.is_some_and(|s| s.node.meta.get("synthetic") == Some(&Value::Bool(true)));
        let node = subject.map(|s| s.node.name.to_string()).unwrap_or_default();
        // Pebble's events count under `pebble:<Variant>` beside Petri's own
        // kinds; the delegate arm below reads the same events for the
        // sub-agent lifecycle.
        let activity = event.custom().and_then(backend_event);
        if let Some(activity) = &activity
            && activity.backend == "pebble"
        {
            for kind in pebble_kinds(&activity.envelope) {
                *out.kinds.entry((node.clone(), kind)).or_default() += 1;
            }
        }
        match event.coordinator() {
            Some(CoordinatorEvent::RunFinished { status, .. }) => out.run_status = Some(*status),
            Some(CoordinatorEvent::InvocationDeclared { .. }) if event.context.parent.is_some() => {
                out.branch_children += 1;
            }
            _ => {}
        }
        match event.view() {
            Some(ViewEvent::VisitCompleted { outcome, .. }) if !synthetic => {
                out.finals
                    .insert(node.clone(), outcome.status.tag().to_owned());
            }
            _ => {}
        }
        match event.parsed() {
            Some(Parsed::Note { note, .. }) => {
                if let Some(phase) = note.kind.strip_prefix("embedding_host:") {
                    out.markers
                        .entry(node.clone())
                        .or_default()
                        .push(phase.to_owned());
                }
            }
            Some(Parsed::Question { .. }) => out.questions.push(node.clone()),
            _ => {}
        }
        match (event.engine(), &event.derived) {
            (
                Some(Event::ControlRequested { .. }),
                Some(Derived::ControlRequested {
                    deliverable: true,
                    answer: Some(answer),
                }),
            ) => out.answers.push(answer.choice.clone().unwrap_or_default()),
            (Some(Event::NodeExpanded { .. }), _) => out.expansions += 1,
            _ => {}
        }
        match event.custom() {
            Some(value) if activity.is_none() => {
                if let Some(kind) = value["kind"].as_str() {
                    *out.kinds
                        .entry((node.clone(), kind.to_owned()))
                        .or_default() += 1;
                }
            }
            _ => {}
        }
        match activity {
            Some(activity) if node == "delegate" => {
                let variant = activity
                    .envelope
                    .get("event")
                    .and_then(Value::as_object)
                    .and_then(|o| o.keys().next().cloned())
                    .unwrap_or_default();
                if variant.starts_with("SubAgent") {
                    out.subagent.push(variant);
                    delegate_session
                        .get_or_insert_with(|| activity.session.clone().unwrap_or_default());
                }
                if activity.parent_session.is_some() && activity.parent_session == delegate_session
                {
                    out.parent_named = true;
                }
            }
            _ => {}
        }
    }
    out
}

fn kind_total(projected: &Projected, kind: &str) -> usize {
    projected
        .kinds
        .iter()
        .filter(|((_, k), _)| k == kind)
        .map(|(_, n)| n)
        .sum()
}

/// The kinds a Pebble envelope counts under: `pebble:<Variant>`, and
/// `pebble:McpToolCallCompleted` for a completed call to an MCP tool that
/// reached its server (a call a hook denied, or whose arguments Pebble
/// refused, completes without one).
fn pebble_kinds(envelope: &Value) -> Vec<String> {
    let (variant, payload) = match &envelope["event"] {
        Value::String(name) => (name.clone(), Value::Null),
        Value::Object(map) => match map.iter().next() {
            Some((name, payload)) => (name.clone(), payload.clone()),
            None => return Vec::new(),
        },
        _ => return Vec::new(),
    };
    let mut kinds = vec![format!("pebble:{variant}")];
    if variant == "ToolCallCompleted"
        && payload["tool_name"]
            .as_str()
            .is_some_and(|name| name.starts_with("mcp__"))
        && !matches!(
            payload["error_kind"].as_str(),
            Some("denied" | "invalid_arguments")
        )
    {
        kinds.push("pebble:McpToolCallCompleted".to_owned());
    }
    kinds
}

fn count(projected: &Projected, node: &str, kind: &str) -> usize {
    projected
        .kinds
        .get(&(node.to_owned(), kind.to_owned()))
        .copied()
        .unwrap_or(0)
}

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

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = GitCommand::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// The combined readiness workflow through the embedding boundary.
#[tokio::test]
async fn the_combined_workflow_runs_through_the_embedding_boundary() {
    let dir = RunDir::new("embed-readiness");
    let credential = format!("fake-embed-{}", testkit::unique_id());
    let openai = Twin::start(
        Provider::OpenAi,
        &dir.path().join("twin-openai"),
        openai_scripts(&credential),
    )
    .await;
    let anthropic = Twin::start(
        Provider::Anthropic,
        &dir.path().join("twin-anthropic"),
        anthropic_scripts(&credential),
    )
    .await;

    // The host's services: the model client pointed at the twins with fixed
    // credentials and no client retries (the binary ran with
    // `PETRI_LLM_RETRY_ATTEMPTS=1`), the Fabro home, the hooks `register`
    // installs (the local hook service behind its adapter) wrapped by the
    // host's own hooks, the host's interviewer and sink.
    let credentials = StaticCredentials::new()
        .with(
            "openai",
            Credentials::bearer(SecretValue::new(credential.clone())),
        )
        .with(
            "anthropic",
            Credentials::header(CredentialHeader::new(
                "x-api-key",
                SecretValue::new(credential.clone()),
            )),
        );
    let client = build_llm_client(&LlmClientConfig {
        credentials:    Some(Arc::new(credentials)),
        layers:         vec![
            ("openai twin".to_owned(), openai.catalog_layer()),
            ("anthropic twin".to_owned(), anthropic.catalog_layer()),
        ],
        providers:      Some(vec!["openai".to_owned(), "anthropic".to_owned()]),
        retry_attempts: 1,
        timeout:        None,
    })
    .expect("the model client builds");
    let home = dir.path().join("home").join(".fabro");
    fs::create_dir_all(&home).expect("home");
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Always;
    options.echo = false;
    // `register` installs the local hook service as the driver's hooks and
    // as the `HookServiceHandle` the steps ask; the host wraps the former
    // and touches nothing else of it.
    let rt = register(Runtime::standard().frontend(Fabro::new()));
    let host_hooks = Arc::new(EmbeddingHost {
        inner:   rt
            .installed_hooks()
            .expect("register installs the local hook service"),
        pause:   "fan",
        paused:  AtomicU32::new(0),
        release: Notify::new(),
    });
    let rt = rt
        .hooks(host_hooks.clone())
        .capability(PebbleClient(client))
        .capability(FabroHome(home))
        .options(options);

    let remote = dir.path().join("remote.git");
    let mcp_log = dir.path().join("mcp.log");
    let workflow_path = dir.path().join("wf.fabro");
    fs::write(&workflow_path, workflow(Provider::OpenAi.model())).expect("workflow");
    fs::write(
        dir.path().join("workflow.toml"),
        workflow_toml(dir.path(), &remote, &mcp_log),
    )
    .expect("workflow.toml");
    let lowered = rt
        .check(&workflow_path, None, None, &CompileInputs::new())
        .expect("loads");
    let graph = lowered
        .graph
        .unwrap_or_else(|| panic!("lowers: {:?}", lowered.diagnostics));

    let sink = Arc::new(CollectingSink::default());
    let projector = EventProjector::new(sink.clone());
    let dispatcher = InterviewDispatcher::new(Arc::new(SayYes));
    let host_run = HostRun::new(graph)
        .with_children(lowered.children)
        .observe(projector.clone() as Arc<dyn ExecutionObserver>)
        .observe(Arc::new(dispatcher.clone()));
    let releaser = host_hooks.clone();
    let release = tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(240);
        while releaser.paused.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            sleep(Duration::from_millis(10)).await;
        }
        sleep(Duration::from_millis(100)).await;
        releaser.release.notify_waiters();
    });
    let run = host::run_configured(&rt, host_run, |handle, secrets| {
        dispatcher.wire(handle, secrets);
    });
    let ws = dir.path().join("scopes/invocation-0-scope-0/work");
    let Ok(report) = timeout(Duration::from_secs(300), run).await else {
        panic!(
            "the run did not finish in time; openai spent {:?} (unmatched {}), anthropic spent {:?}; tool hooks: {:?}",
            openai.consumed(),
            openai.unmatched(),
            anthropic.consumed(),
            fs::read_to_string(ws.join("tool-hooks.log")).unwrap_or_default(),
        );
    };
    let report = report.expect("the run completes");
    release.await.expect("the releaser ran");
    let interview = dispatcher.shutdown().await;
    let receipt = projector.shutdown().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "errors {:?}; openai spent {:?} (unmatched {}), anthropic spent {:?}; tool hooks {:?}; interview {:?}",
        report.state.errors(),
        openai.consumed(),
        openai.unmatched(),
        anthropic.consumed(),
        fs::read_to_string(ws.join("tool-hooks.log")).unwrap_or_default(),
        interview.errors,
    );
    assert!(interview.is_clean(), "{:?}", interview.errors);
    assert!(receipt.is_clean(), "{receipt:?}");
    assert_eq!(
        host_hooks.paused.load(Ordering::SeqCst),
        1,
        "the fork was paused once"
    );

    // The same scripts, spent in the same order as through the binary.
    let consumed = openai.consumed();
    assert_eq!(consumed[..11], BEFORE_FAN_OUT);
    let mut branches = consumed[11..13].to_vec();
    branches.sort();
    assert_eq!(branches, ["job-alpha", "job-beta"]);
    assert_eq!(consumed[13..], AFTER_FAN_OUT);
    assert_eq!(openai.unmatched(), 0, "{:?}", openai.request_log());
    assert_eq!(anthropic.consumed(), ["docs-draft", "docs-finish"]);
    assert_eq!(anthropic.unmatched(), 0);

    // The same files.
    assert_eq!(read(&ws.join("notes.txt")), "draft\n-- petri\n");
    assert_eq!(read(&ws.join("protected.txt")), "keep\n");
    assert_eq!(read(&ws.join("note.txt")), "release note\n");
    assert_eq!(read(&ws.join("CHANGELOG.md")), "changelog\n");
    assert_eq!(read(&ws.join("f5.txt")), "five");
    assert_eq!(
        read(&ws.join("tool-hooks.log")),
        "ran:plan\nran:plan\nran:write\nran:delegate\nran:polish\nran:polish\nran:polish\nran:polish\nran:polish\nran:review\n"
    );
    assert_eq!(read(&mcp_log).matches("call write_file").count(), 2);
    let results: Vec<Value> =
        serde_json::from_str(&read(&ws.join("results.json"))).expect("results.json");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["item_label"], json!("alpha"));
    assert_eq!(results[1]["item_label"], json!("beta"));
    assert!(!ws.join("decision.txt").exists());
    assert_eq!(
        read(&ws.join("run-end.log")),
        "run_complete\nsandbox_cleanup\n"
    );
    // No platform Git operation: the host performed none, and the run
    // performed none on its own.
    assert_eq!(git(&ws, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(git(&ws, &["tag", "--list"]), "");
    assert!(!git(&ws, &["status", "--porcelain"]).is_empty());
    assert_eq!(git(&remote, &["for-each-ref"]), "");

    // The run, rebuilt from the public events alone.
    let events = sink.events();
    let projected = project(&events);
    assert_eq!(projected.run_status, Some(RunStatus::Success));
    for node in [
        "run_prepare_1",
        "run_prepare_2",
        "run_prepare_3",
        "plan",
        "write",
        "delegate",
        "gate",
        "jobs",
        "fan",
        "join",
        "report",
        "polish",
        "review",
        "draft_docs",
        "finish_docs",
        "check",
    ] {
        assert_eq!(
            projected.finals.get(node).map(String::as_str),
            Some("success"),
            "{node}: {:?}",
            projected.finals
        );
    }
    assert!(!projected.finals.contains_key("hold"));
    assert_eq!(projected.questions, ["gate"]);
    assert_eq!(projected.answers, ["Y"]);
    assert_eq!(projected.invocations, BTreeSet::from([0, 1, 2]));
    assert_eq!(projected.expansions, 1);
    assert_eq!(projected.branch_children, 2);
    assert_eq!(
        kind_total(&projected, "attractor.parallel.branch.started"),
        2,
        "{projected:#?}"
    );
    assert_eq!(
        kind_total(&projected, "attractor.parallel.branch.completed"),
        2,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "join", "attractor.parallel.completed"),
        1,
        "{projected:#?}"
    );
    // Every item 9 family, attributed to its stage, as through the binary.
    assert!(
        count(&projected, "plan", "attractor.skills") >= 1,
        "{projected:#?}"
    );
    // C2 on Pebble's own events: the write's server was ready and one
    // proxied call completed.
    assert!(
        count(&projected, "write", "pebble:McpServerReady") >= 1,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "write", "pebble:McpToolCallCompleted"),
        1,
        "{projected:#?}"
    );
    // The MCP tool is still on the session after the compaction: the review
    // called it on the compacted thread (one pre and one post hook report).
    assert_eq!(
        count(&projected, "review", "pebble:McpToolCallCompleted"),
        1,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "review", "attractor.hook"),
        2,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "polish", "attractor.compaction"),
        1,
        "{projected:#?}"
    );
    // C1: the docs thread's plan (Petri's) and its failover (Pebble's); the
    // second docs node reuses the thread with no plan of its own.
    assert_eq!(
        count(&projected, "draft_docs", "attractor.fallback.plan"),
        1
    );
    assert_eq!(count(&projected, "draft_docs", "pebble:RouteFailover"), 1);
    assert_eq!(
        count(&projected, "finish_docs", "attractor.fallback.plan"),
        0
    );
    assert_eq!(count(&projected, "finish_docs", "attractor.thread"), 1);
    assert_eq!(
        count(&projected, "plan", "attractor.hook"),
        3,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "write", "attractor.hook"),
        3,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "delegate", "attractor.hook"),
        3,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "polish", "attractor.hook"),
        10,
        "{projected:#?}"
    );
    assert_eq!(
        projected.subagent,
        ["SubAgentSpawned", "SubAgentCompleted", "SubAgentClosed"],
        "{projected:#?}"
    );
    assert!(
        projected.parent_named,
        "the child's events name the parent session"
    );

    // The host's awaited points ran in order at every logical stage, and the
    // fan-out's admission was held once until released.
    assert_eq!(host_hooks.paused.load(Ordering::SeqCst), 1);
    for node in [
        "plan", "write", "delegate", "gate", "jobs", "fan", "report", "polish", "review", "check",
    ] {
        assert_eq!(
            projected.markers.get(node).cloned().unwrap_or_default(),
            [
                "before_attempt",
                "prepare_result",
                "after_record",
                "transition"
            ],
            "{node}"
        );
    }

    // Replay yields the same public stream, identity for identity.
    let mut replayed = replay_run_dir(dir.path()).await.expect("replays");
    replayed.sort_by_key(|e| e.id);
    let live = normalized(&events);
    for (index, (from_replay, from_live)) in replayed.iter().zip(&live).enumerate() {
        assert!(
            from_replay == from_live,
            "event {index} of {} replayed / {} live differs:\nreplay {from_replay:?}\nlive   {from_live:?}",
            replayed.len(),
            live.len()
        );
    }
    assert_eq!(
        replayed.len(),
        live.len(),
        "the replay has every live event"
    );
    openai.stop();
    anthropic.stop();
}
