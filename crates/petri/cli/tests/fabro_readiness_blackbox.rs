//! Readiness item 10 (milestone D) through the shipped binary: the item 8
//! milestone workflow with every item 9 facility added to it, in one run.
//!
//! One `full` thread (`notes`) carries a skill-guided plan, a hooked MCP
//! write, a hooked sub-agent, a bounded `for_each` fan-out consumed
//! downstream, a compaction, and a later node that reuses the compacted
//! thread. A second thread (`docs`) fails over from the OpenAI twin to the
//! Anthropic twin on its first node and continues on that route on its
//! second. `[run.prepare]` makes the workspace a Git repository with a local
//! bare remote, so the run's absence of platform Git operations is a fact the
//! repository itself shows afterwards. Provider twins on loopback, the
//! scripted `mcp_server.py`, real shell tools, a scripted human answer; no
//! Fabro, no database, no server, no platform adapters.
//!
//! Failure (the fallback chain exhausted at the end of the run) and
//! cancellation (an interrupt while a sub-agent's tool runs) are separate
//! cases. Every case reads the finished run back through `petri inspect` and
//! through the public event stream (`execution::replay_run`), the projection
//! an embedding host consumes.

mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command as GitCommand, Stdio};
use std::time::Duration;
use std::{env, fs, slice};

use petri::engine::Event;
use petri::execution::events::{Parsed, RunEvent, ViewEvent};
use petri::execution::{CancelReason, CoordinatorEvent};
use serde_json::{Value, json};
use support::fabro::failures::{self, error};
use support::fabro::interview;
use support::fabro::launch::{Case, Finished, Launch};
use support::fabro::scenario::CellRecord;
use support::fabro::subagents::{
    Activity, activities, of_kind, one_call, public_events, spawn_and_wait,
};
use support::fabro::twins::{Provider, Twin, model, scenario, shell_tool, text, tool_call};
use testkit::backend_event;
use tokio::process::Command;
use tokio::time::sleep;

const OPENAI: Provider = Provider::OpenAi;
const ANTHROPIC: Provider = Provider::Anthropic;

const MEMORY_RULE: &str = "Always sign release notes with -- petri";

/// The token count that carries a response past `gpt-5.6-sol`'s compaction
/// trigger (80 percent of its 1,050,000-token window).
const OVER_THRESHOLD: u64 = 900_000;

/// The workflow. `tail` is what follows the `docs` thread: the success case
/// checks the files; the failure and cancellation cases end at `exit`.
fn workflow(model: &str, tail: &str) -> String {
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
    {tail}
    start -> plan -> write -> delegate -> gate
    gate -> jobs [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    jobs -> fan -> job -> join -> report -> polish -> review -> draft_docs -> finish_docs
    hold -> exit
}}"#
    )
}

/// The scripted MCP server, versioned under the acceptance crate's test data.
fn server_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fabro/acceptance/testdata/mcp_server.py")
        .canonicalize()
        .expect("the scripted MCP server exists")
}

/// `workflow.toml`: the fallback chain, the preparation that builds the
/// repository, the memory file and the skill, the MCP server, the hooks
/// (one blocks destructive shell commands, one blocks MCP writes to the
/// protected file, one logs every tool that ran), and the run-end hooks.
fn workflow_toml(case: &Case, remote: &Path, mcp_log: &Path) -> String {
    let command = serde_json::to_string(&[
        "python3".to_owned(),
        server_script().display().to_string(),
        "--tag".to_owned(),
        case.run_dir.display().to_string(),
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
event = "run_failed"
script = "echo run_failed >> run-end.log"

[[run.hooks]]
event = "sandbox_cleanup"
script = "echo sandbox_cleanup >> run-end.log"
"#,
        memory = MEMORY_RULE,
        remote = remote.display(),
        mcp_log = mcp_log.display().to_string(),
    )
}

/// How the sub-agent's second tool call behaves: it writes the changelog,
/// or it blocks until the harness interrupts the run.
enum Child {
    Writes,
    Blocks { marker: String },
}

/// The OpenAI twin's scripts, in request order. Each scenario matches the
/// text the request carries and is spent once, so the order is the order the
/// run makes its requests; the two fan-out branches run concurrently and
/// match on their own item.
fn openai_scripts(case: &Case, child: &Child) -> Vec<Value> {
    let model = model(OPENAI);
    let shell = shell_tool(OPENAI);
    let s = |id: &str, matcher: &str, script: Value| {
        scenario(OPENAI, &case.credential, id, model, matcher, script)
    };
    let heavy = |mut scenario: Value| {
        scenario["script"]["usage"] = json!({ "input_tokens": OVER_THRESHOLD, "output_tokens": 5 });
        scenario
    };
    let finding = |item: &str| {
        s(
            &format!("job-{item}"),
            // The fenced item is pretty JSON (`"name": "alpha"`); the `jobs`
            // stage's compact output in the preamble is not.
            &format!("\"name\": \"{item}\""),
            text(&format!(
                r#"{{"outcome":"succeeded","context_updates":{{"output.finder":{{"found":"{item}"}}}}}}"#
            )),
        )
    };
    let child_second = match child {
        Child::Writes => one_call(
            "child-write",
            shell,
            json!({ "command": "printf 'changelog\\n' > CHANGELOG.md && echo CHANGED" }),
        ),
        Child::Blocks { marker } => one_call(
            "child-block",
            shell,
            json!({ "command": format!("touch waiting.txt; while true; do sleep 0.1; done # {marker}") }),
        ),
    };
    vec![
        // `plan`: the skill guides a real tool call on the retained thread.
        s(
            "plan-load",
            "Plan the release note",
            tool_call("plan-load", "use_skill", json!({ "skill_name": "sign" })),
        ),
        s(
            "plan-sign",
            "SIGN INSTRUCTIONS",
            tool_call(
                "plan-sign",
                shell,
                json!({ "command": "printf -- '-- petri\\n' >> notes.txt && echo SIGNED" }),
            ),
        ),
        s("plan-done", "SIGNED", text("PLANNED: a signed note")),
        // `write`: the MCP tool, blocked once by the hook, then allowed.
        s(
            "write-protected",
            "Write the release note",
            tool_call(
                "write-protected",
                "mcp__notes__write_file",
                json!({ "path": "protected.txt", "content": "overwrite" }),
            ),
        ),
        s(
            "write-note",
            "protected files are off limits",
            tool_call(
                "write-note",
                "mcp__notes__write_file",
                json!({ "path": "note.txt", "content": "release note\n" }),
            ),
        ),
        s(
            "write-done",
            "wrote 13 bytes to note.txt",
            text("WROTE the note"),
        ),
        // `delegate`: a child under the same hooks writes the changelog.
        s(
            "delegate",
            "Delegate the changelog",
            spawn_and_wait(&["child: write CHANGELOG.md"]),
        ),
        s(
            "child-destroy",
            "child: write CHANGELOG.md",
            one_call(
                "child-destroy",
                shell,
                json!({ "command": "rm -f protected.txt && echo REMOVED" }),
            ),
        ),
        s(
            "child-second",
            "destructive commands are not allowed",
            child_second,
        ),
        s("child-done", "CHANGED", text("Wrote CHANGELOG.md.")),
        s(
            "synthesize",
            "Wrote CHANGELOG.md.",
            text("DELEGATED the changelog"),
        ),
        // The fan-out.
        finding("alpha"),
        finding("beta"),
        // `polish`: five tool rounds, the fifth crossing the trigger, the
        // summary call, then the answer on the compacted thread.
        s(
            "r1",
            "Polish the note",
            tool_call(
                "r1",
                shell,
                json!({ "command": "printf one > f1.txt && echo OUT1" }),
            ),
        ),
        s(
            "r2",
            "OUT1",
            tool_call(
                "r2",
                shell,
                json!({ "command": "printf two > f2.txt && echo OUT2" }),
            ),
        ),
        s(
            "r3",
            "OUT2",
            tool_call(
                "r3",
                shell,
                json!({ "command": "printf three > f3.txt && echo OUT3" }),
            ),
        ),
        s(
            "r4",
            "OUT3",
            tool_call(
                "r4",
                shell,
                json!({ "command": "printf four > f4.txt && echo OUT4" }),
            ),
        ),
        heavy(s(
            "r5",
            "OUT4",
            tool_call(
                "r5",
                shell,
                json!({ "command": "printf five > f5.txt && echo OUT5" }),
            ),
        )),
        s(
            "summary",
            "Here is the conversation to summarize",
            text("HANDOFF: the note is signed, written and delegated; f1..f5 written."),
        ),
        s("polish-done", "OUT5", text("POLISHED")),
        // `review` continues the compacted thread.
        // `review` continues the compacted thread and still has its MCP tool.
        s(
            "review-note",
            "Review the polished note",
            tool_call(
                "review-note",
                "mcp__notes__write_file",
                json!({ "path": "review.txt", "content": "reviewed\n" }),
            ),
        ),
        s("review", "wrote 9 bytes to review.txt", text("REVIEWED")),
        // `draft_docs`: the primary is down, the chain moves the thread.
        s(
            "docs-down",
            "Draft the docs page",
            error(503, "server_error", "service_unavailable", "gone away"),
        ),
    ]
}

/// The Anthropic twin's scripts: the `docs` thread on the fallback route.
/// In the failure case the fallback is down too and the chain is exhausted.
fn anthropic_scripts(case: &Case, fallback_down: bool) -> Vec<Value> {
    let model = model(ANTHROPIC);
    if fallback_down {
        return vec![scenario(
            ANTHROPIC,
            &case.credential,
            "docs-down-too",
            model,
            "Draft the docs page",
            error(503, "server_error", "overloaded_error", "also gone away"),
        )];
    }
    vec![
        scenario(
            ANTHROPIC,
            &case.credential,
            "docs-draft",
            model,
            "Draft the docs page",
            text("DRAFT docs on the fallback"),
        ),
        scenario(
            ANTHROPIC,
            &case.credential,
            "docs-finish",
            model,
            "Finish the docs page",
            text("FINISHED docs"),
        ),
    ]
}

/// One request per route: the client's own retries are off, so the twin
/// request logs show exactly the fallback sequence.
fn launch() -> Launch {
    Launch {
        env: vec![("PETRI_LLM_RETRY_ATTEMPTS".into(), "1".into())],
        ..Launch::default()
    }
}

fn ship_script(case: &Case) -> PathBuf {
    interview::write(&case.root, "ship", &[interview::entry_matching(
        "ship-it",
        json!({ "node": "gate", "kind": "yes_no", "options": ["Y", "N"] }),
        1,
        interview::choice("Y"),
    )])
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// `git <args>` in `dir`, stdout trimmed.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = GitCommand::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Everything a case has in common, once the run has finished.
struct Setup {
    case:    Case,
    openai:  Twin,
    anthrop: Twin,
    remote:  PathBuf,
    mcp_log: PathBuf,
}

async fn setup(label: &str, child: &Child, fallback_down: bool) -> Setup {
    let mut case = Case::new(label);
    let remote = case.root.join("remote.git");
    let mcp_log = case.root.join("mcp.log");
    let openai = Twin::start(
        OPENAI,
        &case.root.join("twin-openai"),
        openai_scripts(&case, child),
    )
    .await;
    let anthrop = Twin::start(
        ANTHROPIC,
        &case.root.join("twin-anthropic"),
        anthropic_scripts(&case, fallback_down),
    )
    .await;
    case.redirect(&openai);
    case.redirect(&anthrop);
    Setup {
        case,
        openai,
        anthrop,
        remote,
        mcp_log,
    }
}

impl Setup {
    fn workflow(&self, tail: &str) -> PathBuf {
        self.case.workflow(
            &workflow(model(OPENAI), tail),
            Some(&workflow_toml(&self.case, &self.remote, &self.mcp_log)),
        )
    }

    fn stop(self) {
        self.openai.stop();
        self.anthrop.stop();
    }
}

/// The OpenAI scripts every case spends before the fan-out, in order.
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

/// The OpenAI scripts every complete case spends after the fan-out.
const AFTER_FAN_OUT: [&str; 11] = [
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
    // A placeholder so the two arrays have the same shape in assertions.
    "",
];

/// The phases every complete case shares: the skill drove a hooked tool on
/// the thread, the memory reached the model, the hook blocked the MCP write
/// to the protected file and let the note through, the child worked under
/// the hooks, the branches reported, the thread compacted and was reused.
fn assert_common_phases(setup: &Setup, finished: &Finished) {
    let case = &setup.case;
    let consumed = setup.openai.consumed();
    assert_eq!(consumed[..11], BEFORE_FAN_OUT, "{}", finished.stderr);
    let mut branches = consumed[11..13].to_vec();
    branches.sort();
    assert_eq!(branches, ["job-alpha", "job-beta"], "{}", finished.stderr);
    assert_eq!(consumed[13..], AFTER_FAN_OUT[..10], "{}", finished.stderr);
    assert_eq!(
        setup.openai.unmatched(),
        0,
        "{:?}",
        setup.openai.request_log()
    );
    assert_eq!(
        setup.anthrop.unmatched(),
        0,
        "{:?}",
        setup.anthrop.request_log()
    );

    let requests: Vec<String> = setup
        .openai
        .requests_for(&case.credential)
        .iter()
        .map(|r| serde_json::to_string(r).expect("json"))
        .collect();
    // Skills: the discovered skill is offered on the first request, and its
    // body came back as the tool result the next request carries.
    assert!(
        requests[0].contains("Sign the release note") && requests[0].contains("\"use_skill\""),
        "the sign skill and the skill tool are offered: {}",
        requests[0]
    );
    assert!(
        requests[1].contains("SIGN INSTRUCTIONS"),
        "the skill body reached the model: {}",
        requests[1]
    );
    // Project memory written by `[run.prepare]` reaches the first request.
    assert!(requests[0].contains(MEMORY_RULE), "{}", requests[0]);
    // The thread: the write continues the plan's conversation.
    assert!(
        requests[3].contains("PLANNED: a signed note")
            && requests[3].contains("Write the release note"),
        "{}",
        requests[3]
    );
    // MCP under the hook: the model saw the block reason, then the result.
    assert!(
        requests[4].contains("protected files are off limits"),
        "{}",
        requests[4]
    );
    assert!(
        requests[5].contains("wrote 13 bytes to note.txt"),
        "{}",
        requests[5]
    );
    // The compaction: the request after the cut carries the summary and the
    // preserved tail, not the discarded head; the later node reuses it.
    let after_cut = requests
        .iter()
        .find(|body| body.contains("HANDOFF: the note is signed"))
        .expect("a request carried the summary");
    assert!(after_cut.contains("OUT5"), "the preserved tail stays");
    assert!(
        !after_cut.contains("OUT1"),
        "the discarded head is gone: {after_cut}"
    );
    let review = requests
        .iter()
        .find(|body| body.contains("Review the polished note"))
        .expect("the review request");
    assert!(review.contains("HANDOFF: the note is signed"), "{review}");
    assert!(review.contains("POLISHED"), "{review}");
    assert!(
        !review.contains("SIGN INSTRUCTIONS"),
        "the compacted head is gone: {review}"
    );

    // The workspace: every effect, and none of the blocked ones.
    let workspace = case.workspace();
    assert_eq!(
        read(&workspace.join("notes.txt")),
        "draft\n-- petri\n",
        "the skill-driven append ran"
    );
    assert_eq!(
        read(&workspace.join("protected.txt")),
        "keep\n",
        "neither blocked write ran"
    );
    assert_eq!(
        read(&workspace.join("note.txt")),
        "release note\n",
        "the allowed MCP write ran"
    );
    assert_eq!(
        read(&workspace.join("CHANGELOG.md")),
        "changelog\n",
        "the child wrote the changelog"
    );
    assert_eq!(read(&workspace.join("f5.txt")), "five");
    assert_eq!(
        read(&workspace.join("tool-hooks.log")),
        "ran:plan\nran:plan\nran:write\nran:delegate\nran:polish\nran:polish\nran:polish\nran:polish\nran:polish\nran:review\n",
        "the post hook saw every tool that ran, the child's under the parent stage, and none that was blocked"
    );
    assert_eq!(
        read(&setup.mcp_log).matches("call write_file").count(),
        2,
        "the servers saw the allowed write and the review's note; the blocked one never reached them: {}",
        read(&setup.mcp_log)
    );
    let results: Vec<Value> =
        serde_json::from_str(&read(&workspace.join("results.json"))).expect("results.json");
    assert_eq!(results.len(), 2, "{results:?}");
    for (index, (envelope, item)) in results.iter().zip(["alpha", "beta"]).enumerate() {
        assert_eq!(envelope["index"], json!(index), "{envelope}");
        assert_eq!(envelope["item_label"], json!(item), "{envelope}");
        assert_eq!(envelope["status"], json!("succeeded"), "{envelope}");
    }

    // Output attribution: each stage's lines carry its stage; a branch's
    // lines carry its invocation.
    let echoed = finished.echoed();
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "write" && line.contains("WROTE")),
        "{echoed:?}"
    );
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "review" && line.contains("REVIEWED")),
        "{echoed:?}"
    );
    let tags = finished.echoed_tags();
    let branch_tags: Vec<&String> = tags
        .iter()
        .filter(|(tag, _)| tag.contains("/job#"))
        .map(|(tag, _)| tag)
        .collect();
    assert!(
        !branch_tags.is_empty() && branch_tags.iter().all(|tag| tag.starts_with("invocation-")),
        "{tags:?}"
    );

    // The human decision routed the run into the fan-out.
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    assert_eq!(receipt["questions"][0]["node"], "gate");
    assert_eq!(receipt["questions"][0]["reply"]["choice"], "Y");
    assert!(
        !workspace.join("decision.txt").exists(),
        "the refused route never ran"
    );

    // The fallback: the `docs` thread's plan, its failover, and the reused
    // route on the second node.
    let records = failures::records(&finished.run_dir);
    let plan = failures::of_node(&records, "draft_docs", "attractor.fallback.plan");
    assert_eq!(plan.len(), 1, "{records:?}");
    // The move is Pebble's own event, attributed to the node.
    let failover = failures::pebble_events(&finished.run_dir, "draft_docs", "RouteFailover");
    assert_eq!(failover.len(), 1, "{records:?}");
    assert_eq!(failover[0]["from"], json!("openai/gpt-5.6-sol"));
    assert_eq!(failover[0]["to"], json!("anthropic/claude-sonnet-5"));
    // The notes thread has one plan, on its first node; every later node
    // reuses the thread on the primary and none fails over.
    assert_eq!(
        failures::of_node(&records, "plan", "attractor.fallback.plan").len(),
        1,
        "{records:?}"
    );
    for node in ["plan", "write", "delegate", "polish", "review"] {
        assert!(
            failures::pebble_events(&finished.run_dir, node, "RouteFailover").is_empty(),
            "the notes thread never left its primary: {records:?}"
        );
        assert_eq!(
            failures::of_node(&records, node, "attractor.fallback.plan").len(),
            usize::from(node == "plan"),
            "a reused thread has no plan of its own: {node}: {records:?}"
        );
        let thread = failures::of_node(&records, node, "attractor.thread");
        assert_eq!(thread.len(), 1, "{node}: {records:?}");
        assert_eq!(
            thread[0]["reused"],
            json!(node != "plan"),
            "{node}: {}",
            thread[0]
        );
    }

    // The workspace the run reported is the one the files are in.
    assert_eq!(
        finished.reported_workspaces(),
        slice::from_ref(&workspace),
        "{}",
        finished.stderr
    );
}

/// No platform Git operation happened: the repository `[run.prepare]` built
/// still has its one commit and a dirty working tree, no tag, and the local
/// remote received nothing.
fn assert_no_platform_git_operations(setup: &Setup) {
    let workspace = setup.case.workspace();
    assert_eq!(
        git(&workspace, &["rev-list", "--count", "HEAD"]),
        "1",
        "no commit was added"
    );
    assert_eq!(git(&workspace, &["log", "-1", "--format=%s"]), "base");
    assert_eq!(
        git(&workspace, &["tag", "--list"]),
        "",
        "no tag was created"
    );
    assert_eq!(
        git(&workspace, &["stash", "list"]),
        "",
        "nothing was stashed"
    );
    assert!(
        !git(&workspace, &["status", "--porcelain"]).is_empty(),
        "the run's changes stay uncommitted in the working tree"
    );
    assert_eq!(
        git(&setup.remote, &["for-each-ref"]),
        "",
        "nothing was pushed to the local remote"
    );
    assert!(
        !workspace
            .join(".git")
            .join("refs")
            .join("heads")
            .join("petri")
            .exists(),
        "no run branch was created"
    );
}

/// What a host rebuilds of the item 9 facilities from the public events:
/// the `step_custom` kinds per stage, the sub-agent lifecycle, the hook
/// notes, and the cancel reason.
#[derive(Debug, Default)]
struct Projected {
    kinds:           BTreeMap<(String, String), usize>,
    hook_notes:      BTreeMap<String, usize>,
    questions:       Vec<String>,
    forks:           usize,
    joins:           usize,
    expansions:      usize,
    branch_children: usize,
    cancel_reason:   Option<CancelReason>,
    run_status:      Option<String>,
}

fn project(events: &[RunEvent]) -> Projected {
    let mut out = Projected::default();
    for event in events {
        let node = event
            .subject
            .as_ref()
            .map(|s| s.node.name.to_string())
            .unwrap_or_default();
        if let Some(value) = event.custom() {
            match backend_event(value) {
                Some(activity) if activity.backend == "pebble" => {
                    for kind in pebble_kinds(&activity.envelope) {
                        *out.kinds.entry((node.clone(), kind)).or_default() += 1;
                    }
                }
                _ => {
                    if let Some(kind) = value["kind"].as_str() {
                        *out.kinds
                            .entry((node.clone(), kind.to_owned()))
                            .or_default() += 1;
                    }
                }
            }
        }
        match event.parsed() {
            Some(Parsed::Note { note, .. }) if note.kind == "hook" => {
                *out.hook_notes.entry(node.clone()).or_default() += 1;
            }
            Some(Parsed::Question { .. }) => out.questions.push(node.clone()),
            _ => {}
        }
        match event.view() {
            Some(ViewEvent::ForkStarted { .. }) => out.forks += 1,
            Some(ViewEvent::ForkCompleted { .. }) => out.joins += 1,
            _ => {}
        }
        if let Some(Event::NodeExpanded { .. }) = event.engine() {
            out.expansions += 1;
        }
        match event.coordinator() {
            Some(CoordinatorEvent::InvocationDeclared { .. }) if event.context.parent.is_some() => {
                out.branch_children += 1;
            }
            Some(CoordinatorEvent::InvocationCancelRequested { reason, .. }) => {
                out.cancel_reason.clone_from(reason);
            }
            Some(CoordinatorEvent::RunFinished { status, .. }) => {
                out.run_status = Some(format!("{status:?}"));
            }
            _ => {}
        }
    }
    out
}

fn count(projected: &Projected, node: &str, kind: &str) -> usize {
    projected
        .kinds
        .get(&(node.to_owned(), kind.to_owned()))
        .copied()
        .unwrap_or(0)
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

/// The item 9 facilities as the public stream carries them, every family
/// attributed to its stage: skill discovery (C3), MCP server and tool
/// lifecycle (C2), the sub-agent lifecycle under the parent's session (C4),
/// the compaction with its summary usage (C5), the fallback plan, routes,
/// failover and usage (C1), the hook decisions, the question, and the fork.
fn assert_public_projection(events: &[RunEvent]) {
    let projected = project(events);
    // C3: one resolution per native session; the plan's names the dirs.
    assert!(
        count(&projected, "plan", "attractor.skills") >= 1,
        "{projected:#?}"
    );
    // C2: the write's server was ready and one proxied call completed, on
    // Pebble's own events.
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
    // C4: the child's lifecycle rides `agent_activity` under the delegate.
    let agent = activities(events);
    let delegate: Vec<&Activity> = agent.iter().filter(|a| a.node == "delegate").collect();
    let parent_session = delegate
        .first()
        .map(|a| a.session.clone())
        .expect("the delegate's session");
    for variant in ["SubAgentSpawned", "SubAgentCompleted", "SubAgentClosed"] {
        let found: Vec<&&Activity> = delegate.iter().filter(|a| a.variant() == variant).collect();
        assert_eq!(
            found.len(),
            1,
            "{variant}: {:?}",
            delegate.iter().map(|a| a.variant()).collect::<Vec<_>>()
        );
        assert_eq!(
            found[0].session, parent_session,
            "{variant} is under the parent's session"
        );
    }
    assert!(
        delegate
            .iter()
            .any(|a| a.parent_session.as_deref() == Some(parent_session.as_str())),
        "the child's own events name the parent session"
    );
    // C5: the polish compacted once, with the summary call's usage.
    assert_eq!(
        count(&projected, "polish", "attractor.compaction"),
        1,
        "{projected:#?}"
    );
    let compaction = events
        .iter()
        .find_map(|e| match e.custom() {
            Some(value) if value["kind"] == "attractor.compaction" => Some(value.clone()),
            _ => None,
        })
        .expect("the compaction event");
    assert!(compaction["usage"].is_object(), "{compaction}");
    // C1: the docs thread's plan (Petri's) and its failover (Pebble's); the
    // second docs node reuses the thread with no plan of its own.
    assert_eq!(
        count(&projected, "draft_docs", "attractor.fallback.plan"),
        1,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "draft_docs", "pebble:RouteFailover"),
        1,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "finish_docs", "attractor.fallback.plan"),
        0,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "finish_docs", "attractor.thread"),
        1,
        "{projected:#?}"
    );
    // Hooks: every tool hook that ran is an `attractor.hook` report on its stage
    // (a child's under the parent stage). An allowed call with a matching
    // pre hook reports twice (pre, post), a blocked call once, a call no pre
    // hook matches once (post): the plan's skill load and shell append (3),
    // the write's blocked and allowed MCP calls (3), the child's blocked and
    // allowed shell calls (3), the polish's five shell rounds (10).
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
    let blocks = events
        .iter()
        .filter(|e| {
            e.custom().is_some_and(|value| {
                value["kind"] == "attractor.hook"
                    && value["report"]["decision"]["decision"] == "block"
            })
        })
        .count();
    assert_eq!(blocks, 2, "the two blocked tool calls: {projected:#?}");
    // The workflow-point hooks had nothing configured, so their reports are
    // silent and leave no `hook` note under any node; the tool-boundary
    // decisions above are the stages' hook facts. The two run-level reports
    // (`run_complete`, then `sandbox_cleanup`) are `hook` notes with no
    // subject, from the coordinator log.
    assert_eq!(
        projected.hook_notes,
        BTreeMap::from([(String::new(), 2)]),
        "{projected:#?}"
    );
    // The interaction and the fan-out. A `for_each` fan-out is an expansion:
    // the stream carries `node_expanded` with the clones and one
    // `invocation_declared` per branch child with its parent link and
    // `meta.branch_role`; the expansion is a fork on the typed stream too:
    // one `fork_started` on the parallel node, one `fork_completed` at the
    // fan-in (`EVENTS.md`).
    assert_eq!(projected.questions, ["gate"]);
    assert_eq!(projected.expansions, 1, "{projected:#?}");
    assert_eq!(projected.branch_children, 2, "{projected:#?}");
    assert_eq!(projected.forks, 1, "{projected:#?}");
    assert_eq!(projected.joins, 1, "{projected:#?}");
    assert!(projected.cancel_reason.is_none());
}

/// The complete combined run: every facility in one workflow, a success.
#[tokio::test]
async fn the_combined_workflow_runs_end_to_end_through_the_binary() {
    let cell = CellRecord::start("readiness/combined@host/openai");
    let setup = setup("readiness-success", &Child::Writes, false).await;
    let workflow = setup.workflow(
        r#"check [shape=parallelogram, script="cat notes.txt note.txt CHANGELOG.md results.json f5.txt tool-hooks.log"]
    finish_docs -> check -> exit"#,
    );
    let script = ship_script(&setup.case);
    let finished = setup
        .case
        .run_with(
            &workflow,
            &["--interview-script", script.to_str().expect("utf-8")],
            launch(),
        )
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_common_phases(&setup, &finished);
    assert_eq!(setup.anthrop.consumed(), ["docs-draft", "docs-finish"]);
    let docs = setup.anthrop.requests_for(&setup.case.credential);
    let second = serde_json::to_string(&docs[1]).expect("json");
    assert!(
        second.contains("DRAFT docs on the fallback") && second.contains("Finish the docs page"),
        "the docs thread continues on the fallback route: {second}"
    );
    let records = failures::records(&finished.run_dir);
    let reused = &failures::of_node(&records, "finish_docs", "attractor.thread")[0];
    assert_eq!(reused["reused"], json!(true), "{reused}");
    // The reused thread's session starts on the route the thread reached.
    let sessions = failures::pebble_events(&finished.run_dir, "finish_docs", "SessionStarted");
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(failures::route(&sessions[0]), "anthropic/claude-sonnet-5");

    // Final status, context and the invocations through the inspection.
    let document = finished.inspect();
    assert_eq!(document["status"], json!("success"));
    assert_eq!(document["complete"], json!(true));
    assert_eq!(
        document["invocations"].as_array().map(Vec::len),
        Some(3),
        "the root and two branch children"
    );
    let context = finished.final_context();
    assert_eq!(context["human.gate.selected"], json!("Y"));
    assert_eq!(context["parallel.branch_count"], json!(2));
    assert_eq!(context["response.review"], json!("REVIEWED"));
    assert_eq!(context["response.finish_docs"], json!("FINISHED docs"));
    assert!(
        !context.contains_key("output.finder"),
        "branch changes never merge into the parent"
    );

    assert_no_platform_git_operations(&setup);
    assert_eq!(
        read(&setup.case.workspace().join("run-end.log")),
        "run_complete\nsandbox_cleanup\n"
    );
    let events = public_events(&setup.case.run_dir).await;
    assert_public_projection(&events);
    let projected = project(&events);
    assert_eq!(projected.run_status.as_deref(), Some("Success"));
    finished.assert_no_leaked_processes().await;
    setup.stop();
    cell.pass();
}

/// The same run with the fallback route down too: the `docs` thread's chain
/// is exhausted, the stage fails with the last error, `on_failure="exit"`
/// ends the run as failed, `run_failed` (not `run_complete`) runs before
/// `sandbox_cleanup`, and everything produced before stays accessible.
#[tokio::test]
async fn the_combined_workflow_reports_an_exhausted_chain_and_keeps_its_work() {
    let cell = CellRecord::start("readiness/combined-failure@host/openai");
    let setup = setup("readiness-failure", &Child::Writes, true).await;
    let workflow = setup.workflow("finish_docs -> exit");
    let script = ship_script(&setup.case);
    let finished = setup
        .case
        .run_with(
            &workflow,
            &["--interview-script", script.to_str().expect("utf-8")],
            launch(),
        )
        .await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert_common_phases(&setup, &finished);
    assert_eq!(setup.anthrop.consumed(), ["docs-down-too"]);
    let records = failures::records(&finished.run_dir);
    let stop = failures::pebble_events(&finished.run_dir, "draft_docs", "RouteFailoverStopped");
    assert_eq!(stop.len(), 1, "{records:?}");
    assert_eq!(stop[0]["reason"], json!("exhausted"), "{}", stop[0]);
    assert!(
        failures::of_node(&records, "finish_docs", "attractor.thread").is_empty(),
        "the second docs node never ran"
    );
    let nodes = finished.finished_nodes();
    assert!(
        nodes
            .iter()
            .any(|(status, node)| node == "draft_docs" && status == "failure"),
        "{nodes:?}"
    );
    let document = finished.inspect();
    assert_eq!(document["status"], json!("failed"));
    assert_eq!(document["complete"], json!(true));
    assert_no_platform_git_operations(&setup);
    assert_eq!(
        read(&setup.case.workspace().join("run-end.log")),
        "run_failed\nsandbox_cleanup\n"
    );
    let events = public_events(&setup.case.run_dir).await;
    let projected = project(&events);
    assert_eq!(
        count(&projected, "draft_docs", "pebble:RouteFailoverStopped"),
        1,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "draft_docs", "pebble:RouteFailover"),
        1,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "polish", "attractor.compaction"),
        1,
        "{projected:#?}"
    );
    assert_eq!(projected.run_status.as_deref(), Some("Failed"));
    finished.assert_no_leaked_processes().await;
    setup.stop();
    cell.pass();
}

/// The same run interrupted while the sub-agent's tool runs under the hook:
/// the run is cancelled with the interrupt as its reason, the child is closed
/// and its shell loop stopped, neither run-end outcome hook fires while
/// `sandbox_cleanup` does, and the work done before stays accessible.
#[tokio::test]
async fn the_combined_workflow_is_cancelled_during_a_childs_tool_and_leaks_nothing() {
    let cell = CellRecord::start("readiness/combined-cancellation@host/openai");
    let marker = format!("petri-readiness-cancel-{}", testkit::unique_id());
    let setup = setup(
        "readiness-cancel",
        &Child::Blocks {
            marker: marker.clone(),
        },
        false,
    )
    .await;
    let workflow = setup.workflow("finish_docs -> exit");
    // The interrupt lands before the gate, so no interview script: a
    // required entry the run never reaches would fail the verification.
    let finished = setup
        .case
        .run_with(&workflow, &["--auto-approve"], Launch {
            interrupt_when: Some(setup.case.workspace().join("waiting.txt")),
            ..launch()
        })
        .await;
    assert!(
        !finished.timed_out,
        "the interrupt ended the run: {}",
        finished.stderr
    );
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    assert_eq!(
        setup.openai.consumed(),
        BEFORE_FAN_OUT[..9],
        "{}",
        finished.stderr
    );
    assert!(setup.anthrop.consumed().is_empty());
    let workspace = setup.case.workspace();
    assert_eq!(
        read(&workspace.join("notes.txt")),
        "draft\n-- petri\n",
        "the plan's work stays"
    );
    assert_eq!(
        read(&workspace.join("note.txt")),
        "release note\n",
        "the write's work stays"
    );
    assert_eq!(
        read(&workspace.join("protected.txt")),
        "keep\n",
        "the child's blocked rm never ran"
    );
    assert!(
        !workspace.join("CHANGELOG.md").exists(),
        "the interrupted child never finished"
    );
    assert_eq!(
        read(&workspace.join("run-end.log")),
        "sandbox_cleanup\n",
        "no run_complete or run_failed on a cancelled run"
    );
    assert_no_platform_git_operations(&setup);

    // The public events: the child was spawned and closed under the
    // delegate's session, and the cancel names the interrupt.
    let events = public_events(&setup.case.run_dir).await;
    let agent = activities(&events);
    assert_eq!(of_kind(&agent, "SubAgentSpawned").len(), 1);
    assert_eq!(
        of_kind(&agent, "SubAgentClosed").len(),
        1,
        "{:?}",
        agent.iter().map(Activity::variant).collect::<Vec<_>>()
    );
    let projected = project(&events);
    assert_eq!(
        projected.cancel_reason,
        Some(CancelReason::Interrupt),
        "{projected:#?}"
    );
    assert_eq!(projected.run_status.as_deref(), Some("Cancelled"));
    assert!(
        count(&projected, "plan", "attractor.skills") >= 1,
        "{projected:#?}"
    );
    assert_eq!(
        count(&projected, "write", "pebble:McpToolCallCompleted"),
        1,
        "{projected:#?}"
    );
    let document = finished.inspect();
    assert_eq!(document["status"], json!("cancelled"));
    assert_eq!(
        document["invocations"][0]["cancel_reason"],
        json!({ "kind": "interrupt" }),
        "{}",
        document["invocations"][0]
    );
    assert_eq!(finished.reported_workspaces(), [workspace]);

    // The child's shell loop is gone, and so is everything else the run
    // started.
    let mut alive = true;
    for _ in 0..50 {
        let output = Command::new("pgrep")
            .args(["-f", &marker])
            .stdin(Stdio::null())
            .output()
            .await
            .expect("pgrep runs");
        alive = output.status.success();
        if !alive {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive, "the child's shell loop was stopped");
    finished.assert_no_leaked_processes().await;
    setup.stop();
    cell.pass();
}
