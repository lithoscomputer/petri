//! The Fabro black box battery: the shipped `petri` binary runs complete
//! Fabro workflows against provider twins on loopback, with real shell and
//! file tools in the sandbox workspace, a scripted interviewer for human
//! gates and agent questions, and an isolated environment that cannot reach
//! a live provider.
//!
//! The parallel regression from the readiness assessment lives in this file
//! too: two command branches write distinct findings under the same context
//! key, a fan-in joins them, and the pinned Conveyor `code_review.py` merges
//! them. The `contract_*` tests state the contract in
//! `crates/fabro/acceptance/scenarios/parallel-results/CONTRACT.md` and
//! compare the envelopes with the capture from the pinned Fabro; the
//! `for_each_*` and `a_failed_branch_*` tests take the same contract through
//! dynamic branches answered by a twin.

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use petri::execution::events::{ViewEvent, replay_run_dir};
use serde_json::{Value, json};
use support::fabro::interview;
use support::fabro::launch::{Case, Launch, sanitized_path};
use support::fabro::twins::{
    Provider, Twin, model, question_tool, requested_effort, scenario, shell_tool, text, tool_call,
    wire_model,
};

/// The edit-and-verify workflow: a command prepares a file, a native agent
/// edits it through real tools, a command verifies the edit, and a human
/// gate decides what happens next.
fn edit_and_verify(provider: Provider) -> String {
    format!(
        r#"digraph EditAndVerify {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="printf 'draft\n' > notes.txt && echo prepared"]
    agent [prompt="Append the word reviewed to notes.txt, then read it back and say APPENDED.", model="{model}", provider="{provider}", reasoning_effort="high", on_failure="exit"]
    verify [shape=parallelogram, script="cat notes.txt"]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no"]
    ship [shape=parallelogram, script="printf 'shipped\n' > decision.txt && cat decision.txt"]
    hold [shape=parallelogram, script="printf 'held\n' > decision.txt && cat decision.txt"]
    start -> prepare -> agent -> verify -> gate
    gate -> ship [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    ship -> exit
    hold -> exit
}}"#,
        model = model(provider),
        provider = provider.id(),
    )
}

/// The twin's side of [`edit_and_verify`]: the model appends through the
/// shell tool, reads the file back, and answers.
fn edit_and_verify_scripts(provider: Provider, namespace: &str) -> Vec<Value> {
    edit_and_verify_scripts_prefixed(provider, namespace, "")
}

/// [`edit_and_verify_scripts`] with scenario ids prefixed, so two cases can
/// share one twin fixture file (ids are unique per file).
fn edit_and_verify_scripts_prefixed(
    provider: Provider,
    namespace: &str,
    prefix: &str,
) -> Vec<Value> {
    let shell = shell_tool(provider);
    let model = model(provider);
    vec![
        scenario(
            provider,
            namespace,
            &format!("{prefix}append"),
            model,
            "Append the word reviewed",
            tool_call(
                "append",
                shell,
                json!({ "command": "printf 'reviewed\\n' >> notes.txt && echo APPEND_DONE" }),
            ),
        ),
        scenario(
            provider,
            namespace,
            &format!("{prefix}read-back"),
            model,
            "APPEND_DONE",
            tool_call(
                "read",
                shell,
                json!({ "command": "cat notes.txt && echo READ_DONE" }),
            ),
        ),
        scenario(
            provider,
            namespace,
            &format!("{prefix}answer"),
            model,
            "READ_DONE",
            text("APPENDED: notes.txt now ends with reviewed."),
        ),
    ]
}

/// Run [`edit_and_verify`] through one twin and check everything the plan's
/// acceptance names: real tools in the workspace, the edited file, the
/// twin's request boundary, the interview, and the final context.
async fn edit_and_verify_case(provider: Provider, label: &str) {
    let mut case = Case::new(label);
    let twin = Twin::start(
        provider,
        &case.root.join("twins"),
        edit_and_verify_scripts(provider, &case.credential),
    )
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(&edit_and_verify(provider), None);
    let script = interview::write(&case.root, "gate", &[interview::entry(
        "hold-it",
        "gate",
        interview::negative(),
    )]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8 path"),
        ])
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );

    // Files: the agent's edit and the gate's decision, in the workspace the
    // run reported and retained.
    let workspaces = finished.reported_workspaces();
    assert_eq!(workspaces, vec![case.workspace()], "{}", finished.stderr);
    assert_eq!(
        fs::read_to_string(case.workspace().join("notes.txt")).expect("notes.txt"),
        "draft\nreviewed\n"
    );
    assert_eq!(
        fs::read_to_string(case.workspace().join("decision.txt")).expect("decision.txt"),
        "held\n"
    );

    // Provider requests: every scripted call consumed, in order, nothing
    // unmatched, the node's model and reasoning on the wire.
    assert_eq!(twin.consumed(), ["append", "read-back", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 3, "{requests:?}");
    for request in &requests {
        assert_eq!(request["model"], model(provider));
        assert_eq!(requested_effort(provider, request), Some("high"));
    }
    let last = serde_json::to_string(&requests[2]).expect("request");
    assert!(
        last.contains("READ_DONE"),
        "the tool result returned to the model: {last}"
    );
    assert!(last.contains("reviewed"), "{last}");

    // Interviews: the one gate, answered negatively through the real step.
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]));
    assert_eq!(receipt["questions"].as_array().map(Vec::len), Some(1));
    assert_eq!(receipt["questions"][0]["node"], "gate");
    assert_eq!(receipt["questions"][0]["reply"]["choice"], "N");
    assert_eq!(receipt["questions"][0]["delivery"], "delivered");
    assert_eq!(receipt["script"]["entries"][0]["consumed"], 1);

    // Final context: the agent's response, the last stage, the gate's choice.
    let context = finished.final_context();
    assert_eq!(
        context["response.agent"],
        json!("APPENDED: notes.txt now ends with reviewed.")
    );
    // `last_stage` is the agent handler's key, as in Fabro; commands and gates
    // leave it alone.
    assert_eq!(context["last_stage"], json!("agent"));
    assert_eq!(context["human.gate.selected"], json!("N"));
    assert_eq!(context["command.output"], json!("held\n"));
    assert!(!context.contains_key("response.verify"), "{context:?}");

    // Process and output: every stage finished, attributable output on stderr.
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    for node in ["prepare", "agent", "verify", "gate", "hold"] {
        assert!(nodes.contains(&node.to_owned()), "{node} in {nodes:?}");
    }
    assert!(!nodes.contains(&"ship".to_owned()), "{nodes:?}");
    let echoed = finished.echoed();
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "verify" && line == "reviewed"),
        "{echoed:?}"
    );
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "prepare" && line == "prepared"),
        "{echoed:?}"
    );
    assert!(
        !finished.stderr.contains(&case.credential),
        "the fake credential never reaches the terminal"
    );
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

#[tokio::test]
async fn edit_and_verify_through_native_openai() {
    edit_and_verify_case(Provider::OpenAi, "openai").await;
}

#[tokio::test]
async fn edit_and_verify_through_native_anthropic() {
    edit_and_verify_case(Provider::Anthropic, "anthropic").await;
}

/// Two cases against one twin, at the same time, with their own credential
/// namespaces: neither consumes the other's scripts or interview answers.
#[tokio::test]
async fn concurrent_cases_keep_their_scripts_apart() {
    let mut first = Case::new("concurrent-a");
    let mut second = Case::new("concurrent-b");
    let mut scripts = edit_and_verify_scripts_prefixed(Provider::OpenAi, &first.credential, "a/");
    scripts.extend(edit_and_verify_scripts_prefixed(
        Provider::OpenAi,
        &second.credential,
        "b/",
    ));
    let twin = Twin::start(Provider::OpenAi, &first.root.join("twins"), scripts).await;
    first.redirect(&twin);
    second.redirect(&twin);
    let workflow_a = first.workflow(&edit_and_verify(Provider::OpenAi), None);
    let workflow_b = second.workflow(&edit_and_verify(Provider::OpenAi), None);
    let script_a = interview::write(&first.root, "gate", &[interview::entry(
        "a-holds",
        "gate",
        interview::negative(),
    )]);
    let script_b = interview::write(&second.root, "gate", &[interview::entry(
        "b-ships",
        "gate",
        interview::choice("Y"),
    )]);
    let args_a = ["--interview-script", script_a.to_str().expect("utf-8")];
    let args_b = ["--interview-script", script_b.to_str().expect("utf-8")];
    let (a, b) = tokio::join!(
        first.run(&workflow_a, &args_a),
        second.run(&workflow_b, &args_b),
    );
    a.assert_code(0);
    b.assert_code(0);
    assert_eq!(twin.unmatched(), 0);
    let consumed = twin.consumed();
    assert_eq!(consumed.len(), 6, "{consumed:?}");
    for id in [
        "a/append",
        "a/read-back",
        "a/answer",
        "b/append",
        "b/read-back",
        "b/answer",
    ] {
        assert!(consumed.contains(&id.to_owned()), "{id} in {consumed:?}");
    }
    assert_eq!(twin.requests_for(&first.credential).len(), 3);
    assert_eq!(twin.requests_for(&second.credential).len(), 3);
    assert_eq!(
        fs::read_to_string(first.workspace().join("decision.txt")).expect("a"),
        "held\n"
    );
    assert_eq!(
        fs::read_to_string(second.workspace().join("decision.txt")).expect("b"),
        "shipped\n"
    );
    assert_eq!(a.receipt()["script"]["entries"][0]["id"], "a-holds");
    assert_eq!(b.receipt()["script"]["entries"][0]["id"], "b-ships");
    a.assert_no_leaked_processes().await;
    b.assert_no_leaked_processes().await;
}

/// An agent question and a workflow human gate answered by one scripted
/// interviewer: the agent's question rides Pebble's question tool into the
/// same receipt, with its tool call and session in the question identity.
#[tokio::test]
async fn an_agent_question_and_a_human_gate_share_the_scripted_interviewer() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("agent-question");
    let model = model(provider);
    let scripts = vec![
        scenario(
            provider,
            &case.credential,
            "ask",
            model,
            "Ask me which file",
            tool_call(
                "ask",
                question_tool(provider),
                json!({ "questions": [{
                    "id": "which",
                    "header": "File",
                    "question": "Which file should carry the note?",
                    "options": [{ "label": "README" }, { "label": "CHANGELOG" }]
                }] }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "write",
            model,
            "option_2",
            tool_call(
                "write",
                shell_tool(provider),
                json!({ "command": "printf 'CHANGELOG\\n' > chosen.txt && echo WROTE_CHOICE" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "done",
            model,
            "WROTE_CHOICE",
            text("Recorded the choice."),
        ),
    ];
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Ask {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Ask me which file should carry the note, then write my answer to chosen.txt.", model="{model}", provider="openai"]
    gate [shape=hexagon, label="Keep going?", question_type="confirmation"]
    finish [shape=parallelogram, script="cat chosen.txt"]
    start -> agent -> gate
    gate -> finish [label="[Y] Yes"]
    gate -> exit [label="[N] No"]
    finish -> exit
}}"#
        ),
        None,
    );
    let script = interview::write(&case.root, "both", &[
        interview::entry_matching(
            "pick-changelog",
            json!({
                "node": "agent",
                "kind": "multiple_choice",
                "text_contains": "Which file should carry the note?",
                "options": ["option_1", "option_2"],
                "freeform": true,
            }),
            1,
            interview::choice("option_2"),
        ),
        interview::entry_matching(
            "keep-going",
            json!({ "node": "gate", "kind": "confirmation", "options": ["Y", "N"] }),
            1,
            interview::choice("Y"),
        ),
    ]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    assert_eq!(twin.consumed(), ["ask", "write", "done"]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("chosen.txt")).expect("chosen.txt"),
        "CHANGELOG\n"
    );
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]));
    let questions = receipt["questions"].as_array().expect("questions");
    assert_eq!(questions.len(), 2, "{receipt}");
    let agent = questions
        .iter()
        .find(|q| q["node"] == "agent")
        .expect("the agent's question");
    let id = agent["question"].as_str().expect("id");
    assert!(id.contains("/agent/"), "{id}");
    assert!(id.contains("/ask/0"), "tool call id and index: {id}");
    assert_eq!(agent["reply"]["choice"], "option_2");
    assert_eq!(agent["kind"], "multiple_choice");
    let gate = questions
        .iter()
        .find(|q| q["node"] == "gate")
        .expect("the gate's question");
    assert_eq!(gate["reply"]["choice"], "Y");
    assert_eq!(
        finished.final_context()["command.output"],
        json!("CHANGELOG\n")
    );
    // The answer reached the model as the tool's result.
    let requests = twin.requests_for(&case.credential);
    let after_answer = serde_json::to_string(&requests[1]).expect("request");
    assert!(after_answer.contains("option_2"), "{after_answer}");
    finished.assert_no_leaked_processes().await;
}

const GATE_ONLY: &str = r#"digraph Gate {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no"]
    ship [shape=parallelogram, script="echo shipped"]
    hold [shape=parallelogram, script="echo held"]
    start -> gate
    gate -> ship [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    ship -> exit
    hold -> exit
}"#;

#[tokio::test]
async fn an_unexpected_question_fails_the_interview_and_the_run() {
    let case = Case::new("unexpected-question");
    let workflow = case.workflow(GATE_ONLY, None);
    let script = interview::write(&case.root, "wrong-node", &[interview::entry(
        "other",
        "not-the-gate",
        interview::choice("Y"),
    )]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(4);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert!(
        finished.stderr.contains("interview verification failed"),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    let errors = receipt["errors"].as_array().expect("errors");
    assert!(
        errors.iter().any(|e| e
            .as_str()
            .is_some_and(|e| e.contains("no script entry matches"))),
        "{errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.as_str().is_some_and(|e| e.contains("unused required"))),
        "{errors:?}"
    );
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(!nodes.contains(&"ship".to_owned()) && !nodes.contains(&"hold".to_owned()));
    finished.assert_no_leaked_processes().await;
}

#[tokio::test]
async fn an_unused_required_entry_fails_verification_without_rewriting_the_run_status() {
    let case = Case::new("unused-entry");
    let workflow = case.workflow(GATE_ONLY, None);
    let script = interview::write(&case.root, "extra", &[
        interview::entry("ship-it", "gate", interview::choice("Y")),
        interview::entry("never-asked", "review", interview::choice("Y")),
    ]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(4);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    assert_eq!(receipt["questions"][0]["reply"]["choice"], "Y");
    // The unused entry is an error, and the per-entry counts stay in the
    // receipt beside it: the report is complete exactly when it matters.
    assert!(
        receipt["errors"][0]
            .as_str()
            .is_some_and(|e| e.contains("`never-asked` answered 0 of 1")),
        "{receipt}"
    );
    let entries = receipt["script"]["entries"]
        .as_array()
        .expect("the entry counts are reported on a failed verification");
    let unused = entries
        .iter()
        .find(|entry| entry["id"] == "never-asked")
        .expect("the unused entry is listed");
    assert_eq!(unused["consumed"], 0, "{receipt}");
    assert_eq!(unused["remaining"], 1, "{receipt}");
    // The persisted run stands as the engine reported it.
    let coordinator = fs::read_to_string(case.run_dir.join("coordinator.jsonl")).expect("log");
    assert!(
        coordinator.contains(r#""event":"run.finished","status":"success"}"#),
        "{coordinator}"
    );
}

#[tokio::test]
async fn the_wrong_model_reaches_no_script_and_fails_the_agent() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("wrong-model");
    let twin = Twin::start(
        provider,
        &case.root.join("twins"),
        edit_and_verify_scripts(provider, &case.credential),
    )
    .await;
    case.redirect(&twin);
    // The scripts expect gpt-5.6-sol; the workflow asks for terra.
    let dot = edit_and_verify(provider).replace("gpt-5.6-sol", "gpt-5.6-terra");
    let workflow = case.workflow(&dot, None);
    let finished = case.run(&workflow, &["--auto-approve"]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert!(twin.unmatched() >= 1, "{:?}", twin.request_log());
    assert!(twin.consumed().is_empty(), "{:?}", twin.consumed());
    assert!(
        twin.requests_for(&case.credential)
            .iter()
            .all(|r| r["model"] == "gpt-5.6-terra")
    );
    assert!(!case.workspace().join("decision.txt").exists());
    finished.assert_no_leaked_processes().await;
}

#[tokio::test]
async fn a_scripted_call_that_never_arrives_is_visible_as_unconsumed() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("missing-call");
    let mut scripts = edit_and_verify_scripts(provider, &case.credential);
    scripts.push(scenario(
        provider,
        &case.credential,
        "never-requested",
        model(provider),
        "NO_SUCH_MARKER",
        text("unreachable"),
    ));
    let twin = Twin::start(provider, &case.root.join("twins"), scripts).await;
    case.redirect(&twin);
    let workflow = case.workflow(&edit_and_verify(provider), None);
    let finished = case.run(&workflow, &["--auto-approve"]).await;
    finished.assert_code(0);
    let consumed = twin.consumed();
    assert_eq!(consumed, ["append", "read-back", "answer"]);
    assert!(
        !consumed.contains(&"never-requested".to_owned()),
        "the harness sees the missing call: {consumed:?}"
    );
}

#[tokio::test]
async fn malformed_agent_output_fails_the_stage_routably() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("malformed-output");
    let twin = Twin::start(provider, &case.root.join("twins"), vec![scenario(
        provider,
        &case.credential,
        "not-json",
        model(provider),
        "Reply with routing JSON",
        text("I would rather not."),
    )])
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph Malformed {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    agent [prompt="Reply with routing JSON.", model="{}", provider="openai", output_schema="routing", output_retries=0, on_failure="exit"]
    start -> agent -> exit
}}"#,
            model(provider)
        ),
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["not-json"]);
    let context = finished.final_context();
    assert_eq!(context["failure_class"], json!("bad_output"), "{context:?}");
    finished.assert_no_leaked_processes().await;
}

#[tokio::test]
async fn a_provider_without_a_redirect_is_unreachable() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("no-redirect");
    // Only OpenAI is redirected and enabled; the workflow names Anthropic.
    let twin = Twin::start(provider, &case.root.join("twins"), Vec::new()).await;
    case.redirect(&twin);
    let workflow = case.workflow(&edit_and_verify(Provider::Anthropic), None);
    let finished = case.run(&workflow, &["--auto-approve"]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert!(twin.requests().is_empty(), "{:?}", twin.requests());
    assert!(
        !case.workspace().join("notes.txt").exists() || {
            // `prepare` ran before the agent failed; the agent's edit did not.
            fs::read_to_string(case.workspace().join("notes.txt")).expect("notes") == "draft\n"
        }
    );
    finished.assert_no_leaked_processes().await;
}

/// A catalog layer that points at a closed port: the client is built, the
/// request fails at connect, and nothing leaves the loopback.
#[tokio::test]
async fn a_redirect_to_a_closed_port_fails_locally() {
    let mut case = Case::new("closed-port");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    case.redirect_to_nothing(Provider::OpenAi, &format!("http://127.0.0.1:{port}"));
    let workflow = case.workflow(&edit_and_verify(Provider::OpenAi), None);
    let finished = case.run(&workflow, &["--auto-approve"]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    finished.assert_no_leaked_processes().await;
}

const WORKSPACE_PROBE: &str = r#"digraph Probe {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    write [shape=parallelogram, script="printf 'kept\n' > kept.txt && cat kept.txt"]
    start -> write -> exit
}"#;

#[tokio::test]
async fn fabro_runs_keep_the_workspace_after_success_by_default() {
    let case = Case::new("retain-default");
    let workflow = case.workflow(WORKSPACE_PROBE, None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(finished.reported_workspaces(), vec![case.workspace()]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("kept.txt")).expect("kept.txt"),
        "kept\n"
    );
    finished.assert_no_leaked_processes().await;
}

#[tokio::test]
async fn retain_never_deletes_the_workspace() {
    let case = Case::new("retain-never");
    let workflow = case.workflow(WORKSPACE_PROBE, None);
    let finished = case.run(&workflow, &["--retain", "never"]).await;
    finished.assert_code(0);
    assert!(
        !case.workspace().exists(),
        "{} still exists",
        case.workspace().display()
    );
    assert!(
        finished.reported_workspaces().is_empty(),
        "{}",
        finished.stderr
    );
}

#[tokio::test]
async fn a_failed_run_keeps_its_workspace() {
    let case = Case::new("retain-failure");
    let workflow = case.workflow(
        r#"digraph Fail {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    write [shape=parallelogram, script="printf 'partial\n' > partial.txt && exit 3", on_failure="exit"]
    start -> write -> exit
}"#,
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(1);
    assert_eq!(finished.reported_workspaces(), vec![case.workspace()]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("partial.txt")).expect("partial.txt"),
        "partial\n"
    );
}

#[tokio::test]
async fn a_cancelled_run_keeps_its_workspace_and_stops_its_work() {
    let case = Case::new("retain-cancel");
    let workflow = case.workflow(
        r#"digraph Cancel {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=parallelogram, script="printf 'started\n' > started.txt; sleep 60; printf 'finished\n' > finished.txt"]
    start -> work -> exit
}"#,
        None,
    );
    let finished = case
        .run_with(&workflow, &[], Launch {
            interrupt_when: Some(case.workspace().join("started.txt")),
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
    assert_eq!(finished.reported_workspaces(), vec![case.workspace()]);
    assert!(case.workspace().join("started.txt").exists());
    assert!(!case.workspace().join("finished.txt").exists());
    finished.assert_no_leaked_processes().await;
}

/// `--interactive` with piped input: the prompt shows the question in the
/// gate's presentation and one typed line decides the route.
#[tokio::test]
async fn interactive_input_answers_a_gate_from_the_terminal() {
    let case = Case::new("interactive");
    let workflow = case.workflow(GATE_ONLY, None);
    let finished = case
        .run_with(&workflow, &["--interactive"], Launch {
            stdin: Some("no\n".into()),
            ..Launch::default()
        })
        .await;
    finished.assert_code(0);
    assert!(
        finished.stderr.contains("question [gate]: Ship it?"),
        "{}",
        finished.stderr
    );
    assert!(
        finished.stderr.contains("[Y] Yes  [N] No"),
        "{}",
        finished.stderr
    );
    assert!(
        finished.stderr.contains("(Enter takes [Y])"),
        "{}",
        finished.stderr
    );
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(nodes.contains(&"hold".to_owned()), "{nodes:?}");
    assert_eq!(finished.receipt()["questions"][0]["reply"]["choice"], "N");
}

#[tokio::test]
async fn interactive_eof_fails_the_gate_closed() {
    let case = Case::new("interactive-eof");
    let workflow = case.workflow(GATE_ONLY, None);
    let finished = case
        .run_with(&workflow, &["--interactive"], Launch {
            stdin: Some(String::new()),
            close_stdin: true,
            ..Launch::default()
        })
        .await;
    finished.assert_code(4);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    assert_eq!(receipt["questions"][0]["reply"]["kind"], "failed");
    assert!(
        receipt["errors"][0]
            .as_str()
            .is_some_and(|e| e.contains("EOF") || e.contains("closed")),
        "{receipt}"
    );
}

/// The flags exclude one another.
#[tokio::test]
async fn interview_options_are_mutually_exclusive() {
    let case = Case::new("exclusive");
    let workflow = case.workflow(GATE_ONLY, None);
    let script = interview::write(&case.root, "x", &[]);
    let finished = case
        .run(&workflow, &[
            "--auto-approve",
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    assert_eq!(finished.code, Some(2), "{}", finished.stderr);
    assert!(
        finished.stderr.contains("cannot be used with"),
        "{}",
        finished.stderr
    );
}

/// `workflow.toml` beside the workflow: `[run.inputs]` binds an input, and a
/// section the standalone runner does not act on is reported, not silently
/// dropped.
#[tokio::test]
async fn workflow_toml_inputs_bind_and_unsupported_sections_are_reported() {
    let case = Case::new("workflow-toml");
    let workflow = case.workflow(
        r#"digraph Inputs {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    say [shape=parallelogram, script="echo {{ inputs.word }}"]
    start -> say -> exit
}"#,
        Some(
            "[run.inputs]\nword = \"bound\"\n\n[run.model.fallbacks]\n\"gpt-5.6-sol\" = [\"claude-sonnet-5\"]\n\n[run.pull_request]\nenabled = false\n\n[run.environment]\nid = \"review\"\n\n[environments.review]\nprovider = \"local\"\n\n[environments.review.network]\nmode = \"none\"\n",
        ),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(finished.final_context()["command.output"], json!("bound\n"));
    // `[run.model.fallbacks]` is read, not warned about (task 12): the
    // chain waits for an LLM node and this workflow has none.
    assert!(
        !finished
            .stderr
            .contains("ignored.workflow_toml.run.model.fallbacks"),
        "{}",
        finished.stderr
    );
    assert!(
        finished
            .stderr
            .contains("ignored.workflow_toml.run.pull_request"),
        "{}",
        finished.stderr
    );
    // `network` is the Fabro platform's environment key: known, and silent.
    assert!(
        !finished
            .stderr
            .contains("ignored.workflow_toml.environments.review.network"),
        "{}",
        finished.stderr
    );
}

/// Readiness milestone A, the terminal smoke run as a test: the shipped
/// binary, with no `fabro` reachable on `PATH` and nothing else from the
/// developer's environment, performs a command, drives a scripted native
/// agent through real tools, accepts a scripted human answer, and leaves the
/// workspace files where the run said they are. Everything the run recorded
/// is then read back through `petri inspect`, the public inspection surface.
#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr"
)]
async fn milestone_a_smoke_run_without_fabro_on_path() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("milestone-a");
    let Some(path) = sanitized_path(&case.root) else {
        eprintln!("skipping: a fabro executable lives in a system bin directory");
        return;
    };
    // `fabro` really is unreachable under this PATH.
    let probe = Command::new("fabro")
        .env_clear()
        .env("PATH", &path)
        .arg("--version")
        .output();
    assert!(probe.is_err(), "fabro resolved under the sanitized PATH");

    let twin = Twin::start(
        provider,
        &case.root.join("twins"),
        edit_and_verify_scripts(provider, &case.credential),
    )
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(&edit_and_verify(provider), None);
    let script = interview::write(&case.root, "gate", &[interview::entry(
        "hold-it",
        "gate",
        interview::negative(),
    )]);
    let finished = case
        .run_with(
            &workflow,
            &["--interview-script", script.to_str().expect("utf-8 path")],
            Launch {
                path: Some(path),
                ..Launch::default()
            },
        )
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );

    // A command ran, an agent edited through real tools, a human answered,
    // and the files are where the run said.
    assert_eq!(finished.reported_workspaces(), vec![case.workspace()]);
    assert_eq!(
        fs::read_to_string(case.workspace().join("notes.txt")).expect("notes.txt"),
        "draft\nreviewed\n"
    );
    assert_eq!(
        fs::read_to_string(case.workspace().join("decision.txt")).expect("decision.txt"),
        "held\n"
    );
    assert_eq!(twin.consumed(), ["append", "read-back", "answer"]);
    assert_eq!(twin.unmatched(), 0);
    let echoed = finished.echoed();
    for (node, line) in [
        ("prepare", "prepared"),
        ("verify", "reviewed"),
        ("hold", "held"),
    ] {
        assert!(
            echoed.iter().any(|(n, l)| n == node && l == line),
            "{node}: {echoed:?}"
        );
    }

    // The public inspection surface carries the run, its context, and the
    // interview receipt.
    let document = finished.inspect();
    assert_eq!(
        document["complete"],
        json!(true),
        "{}",
        document["incomplete"]
    );
    assert_eq!(document["status"], json!("success"));
    let context = finished.final_context();
    assert_eq!(context["human.gate.selected"], json!("N"));
    assert_eq!(context["command.output"], json!("held\n"));
    assert_eq!(
        context["response.agent"],
        json!("APPENDED: notes.txt now ends with reviewed.")
    );
    let receipt = &document["interviews"];
    assert_eq!(receipt["version"], json!(1), "{receipt}");
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    assert_eq!(receipt["questions"][0]["node"], json!("gate"));
    assert_eq!(receipt["questions"][0]["reply"]["choice"], json!("N"));
    assert_eq!(receipt["questions"][0]["delivery"], json!("delivered"));
    assert_eq!(receipt["script"]["entries"][0]["id"], json!("hold-it"));
    assert_eq!(*receipt, finished.receipt(), "the document is the file");
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Task 7: a `tab` prompt node is one model call with no tools, through the
/// same client the agent nodes use. The twin sees exactly one request that
/// offers no tools, the node's reasoning effort is on the wire, and the
/// response lands under `response.<node>` and `last_response`.
#[tokio::test]
async fn a_prompt_node_makes_one_tool_free_model_call() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("prompt-node");
    let twin = Twin::start(provider, &case.root.join("twins"), vec![scenario(
        provider,
        &case.credential,
        "summary",
        model(provider),
        "Summarize what the command printed",
        text("Summary: the command printed hello."),
    )])
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &format!(
            r#"digraph PromptNode {{
    start [shape=Mdiamond]
    exit [shape=Msquare]
    say [shape=parallelogram, script="echo hello"]
    summarize [shape=tab, prompt="Summarize what the command printed.", model="{}", provider="openai", reasoning_effort="low"]
    start -> say -> summarize -> exit
}}"#,
            model(provider)
        ),
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["summary"]);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 1, "one call, no agent loop: {requests:?}");
    assert_eq!(requests[0]["model"], model(provider));
    assert_eq!(requested_effort(provider, &requests[0]), Some("low"));
    assert!(
        requests[0]
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "a prompt node offers no tools: {}",
        requests[0]
    );
    let sent = serde_json::to_string(&requests[0]).expect("request");
    assert!(
        sent.contains("hello"),
        "the preamble carries the command output: {sent}"
    );
    let context = finished.final_context();
    assert_eq!(
        context["response.summarize"],
        json!("Summary: the command printed hello.")
    );
    assert_eq!(
        context["last_response"],
        json!("Summary: the command printed hello.")
    );
    assert_eq!(context["last_stage"], json!("summarize"));
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Task 7: `[run.model]` in `workflow.toml` is the default model for a node
/// that names none, and `provider = "openrouter"` reaches OpenRouter's chat
/// completions protocol through the redirected catalog.
#[tokio::test]
async fn run_model_defaults_reach_openrouter_through_chat_completions() {
    let provider = Provider::OpenRouter;
    let mut case = Case::new("openrouter");
    let twin = Twin::start(provider, &case.root.join("twins"), vec![scenario(
        provider,
        &case.credential,
        "route",
        wire_model(provider),
        "Say routed",
        text("routed"),
    )])
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(
        r#"digraph Routed {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    ask [shape=tab, prompt="Say routed."]
    start -> ask -> exit
}"#,
        Some(&format!(
            "[run.model]\nprovider = \"openrouter\"\nname = \"{}\"\n\n[run.model.controls]\nreasoning_effort = \"high\"\n",
            model(provider)
        )),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["route"]);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["model"], wire_model(provider));
    assert_eq!(requested_effort(provider, &requests[0]), Some("high"));
    assert_eq!(finished.final_context()["response.ask"], json!("routed"));
    twin.stop();
}

/// Task 7: `[run.prepare]` steps run in the selected environment before any
/// node, in order, with their `env`; a node then sees their effect. A failing
/// step ends the run before the first node runs.
#[tokio::test]
async fn run_prepare_steps_run_before_the_nodes_and_a_failure_stops_the_run() {
    let case = Case::new("prepare");
    let workflow = case.workflow(
        r#"digraph Prepared {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    read [shape=parallelogram, script="cat prepared.txt"]
    start -> read -> exit
}"#,
        Some(
            "[run.prepare]\ntimeout = \"30s\"\n\n[[run.prepare.steps]]\nscript = \"printf 'one\\\\n' > prepared.txt\"\n\n[[run.prepare.steps]]\ncommand = [\"sh\", \"-c\", \"printf \\\"$TAG\\\\n\\\" >> prepared.txt\"]\nenv = { TAG = \"two\" }\n",
        ),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(
        fs::read_to_string(case.workspace().join("prepared.txt")).expect("prepared.txt"),
        "one\ntwo\n"
    );
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    let position = |name: &str| {
        nodes
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| panic!("{name} in {nodes:?}"))
    };
    assert!(position("run_prepare_1") < position("run_prepare_2"));
    assert!(position("run_prepare_2") < position("read"));
    assert_eq!(
        finished.final_context()["command.output"],
        json!("one\ntwo\n")
    );

    let failing = Case::new("prepare-fails");
    let workflow = failing.workflow(
        r#"digraph Prepared {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=parallelogram, script="printf 'ran\n' > ran.txt"]
    start -> work -> exit
}"#,
        Some("[[run.prepare.steps]]\nscript = \"echo setup broke >&2; exit 3\"\n"),
    );
    let finished = failing.run(&workflow, &[]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    assert!(
        !failing.workspace().join("ran.txt").exists(),
        "no node ran after the failed preparation"
    );
    let nodes: Vec<(String, String)> = finished.finished_nodes();
    assert!(
        nodes
            .iter()
            .any(|(status, node)| node == "run_prepare_1" && status == "failure"),
        "{nodes:?}"
    );
    assert!(!nodes.iter().any(|(_, node)| node == "work"), "{nodes:?}");
    assert_eq!(
        finished.final_context()["failure_class"],
        json!("exit_status:3")
    );
}

/// Task 7: a cancel during `[run.prepare]` stops the preparation and the run
/// ends cancelled with no node run.
#[tokio::test]
async fn a_cancel_during_run_prepare_stops_the_run_before_any_node() {
    let case = Case::new("prepare-cancel");
    let workflow = case.workflow(
        r#"digraph Prepared {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=parallelogram, script="printf 'ran\n' > ran.txt"]
    start -> work -> exit
}"#,
        Some("[[run.prepare.steps]]\nscript = \"printf 'started\\\\n' > started.txt; sleep 60; printf 'finished\\\\n' > finished.txt\"\n"),
    );
    let finished = case
        .run_with(&workflow, &[], Launch {
            interrupt_when: Some(case.workspace().join("started.txt")),
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
    assert!(case.workspace().join("started.txt").exists());
    assert!(!case.workspace().join("finished.txt").exists());
    assert!(!case.workspace().join("ran.txt").exists(), "no node ran");
    finished.assert_no_leaked_processes().await;
}

/// Task 7: a validation error anywhere in `workflow.toml` stops the run
/// before the preparation steps start.
#[tokio::test]
async fn the_complete_configuration_is_validated_before_preparation_starts() {
    let case = Case::new("prepare-validate");
    let workflow = case.workflow(
        r#"digraph Prepared {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=parallelogram, script="true"]
    start -> work -> exit
}"#,
        Some(
            "[[run.prepare.steps]]\nscript = \"printf 'prepared\\\\n' > prepared.txt\"\n\n[run.environment]\nid = \"missing\"\n",
        ),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(1);
    assert!(
        finished
            .stderr
            .contains("unsupported.workflow_toml.run.environment"),
        "{}",
        finished.stderr
    );
    assert!(
        !case.workspace().join("prepared.txt").exists(),
        "nothing ran before validation passed"
    );
}

/// Task 7: `[run.environment]` maps `provider = "local"` to the host and
/// carries its `env` into every command; a `{{ secrets.NAME }}` value is
/// resolved from `PETRI_SECRET_NAME` at spawn and masked in the log.
#[tokio::test]
async fn run_environment_env_and_secrets_reach_the_commands_and_stay_masked() {
    let case = Case::new("environment-secrets");
    let workflow = case.workflow(
        r#"digraph Env {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    show [shape=parallelogram, script="echo \"lang=$LANG token=$TOKEN\""]
    start -> show -> exit
}"#,
        Some(
            "[run.environment]\nid = \"review\"\n\n[environments.review]\nprovider = \"local\"\n\n[environments.review.env]\nLANG = \"C.UTF-8\"\nTOKEN = \"{{ secrets.REVIEW_TOKEN }}\"\n",
        ),
    );
    let finished = case
        .run_with(&workflow, &[], Launch {
            env: vec![(
                "PETRI_SECRET_REVIEW_TOKEN".into(),
                "s3cr3t-value-1234".into(),
            )],
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
    let context = finished.final_context();
    let output = context["command.output"].as_str().expect("output");
    assert!(output.contains("lang=C.UTF-8"), "{output}");
    assert!(
        !output.contains("s3cr3t-value-1234"),
        "the secret is masked in the recorded output: {output}"
    );
    assert!(output.contains("token="), "{output}");
    assert!(
        !finished.stderr.contains("s3cr3t-value-1234"),
        "the secret never reaches the terminal"
    );
    assert!(
        finished.stderr.contains("[show#"),
        "the command ran on the host: {}",
        finished.stderr
    );

    // Without the variable the secret is unavailable and the command fails
    // routably, never silently empty.
    let missing = Case::new("environment-secret-missing");
    let workflow = missing.workflow(
        r#"digraph Env {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    show [shape=parallelogram, script="echo $TOKEN", on_failure="exit"]
    start -> show -> exit
}"#,
        Some(
            "[run.environment]\nid = \"review\"\n\n[environments.review]\nprovider = \"local\"\n\n[environments.review.env]\nTOKEN = \"{{ secrets.REVIEW_TOKEN }}\"\n",
        ),
    );
    let finished = missing.run(&workflow, &[]).await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    let nodes = finished.finished_nodes();
    assert!(
        nodes
            .iter()
            .any(|(status, node)| node == "show" && status == "failure"),
        "{nodes:?}"
    );
    let document = finished.inspect();
    let record = &support::fabro::inspect::root_nodes(&document)["show"];
    assert_eq!(record["status"], json!("failure"), "{record}");
    assert!(
        serde_json::to_string(record)
            .expect("record")
            .contains("secret_unavailable"),
        "the failure names the missing secret: {record}"
    );
}

/// Task 7: `[run.execution]` supplies the launch defaults: `approval = "auto"`
/// answers a gate with its first choice and `mode = "dry_run"` simulates the
/// stages, with the explicit options still winning.
#[tokio::test]
async fn run_execution_settings_are_the_launch_defaults() {
    let case = Case::new("execution-auto");
    let workflow = case.workflow(
        r#"digraph Gate {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Ship it?"]
    yes [shape=parallelogram, script="echo shipped"]
    no [shape=parallelogram, script="echo held"]
    start -> gate
    gate -> yes [label="[Y] Yes"]
    gate -> no [label="[N] No"]
    yes -> exit
    no -> exit
}"#,
        Some("[run.execution]\napproval = \"auto\"\n"),
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.final_context()["command.output"],
        json!("shipped\n")
    );

    let dry = Case::new("execution-dry-run");
    let workflow = dry.workflow(
        r#"digraph Dry {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    work [shape=parallelogram, script="printf 'ran\n' > ran.txt"]
    start -> work -> exit
}"#,
        Some("[run.execution]\nmode = \"dry_run\"\n"),
    );
    let finished = dry.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert!(
        !dry.workspace().join("ran.txt").exists(),
        "the stage was simulated"
    );
    assert!(
        !finished.final_context().contains_key("command.output"),
        "a simulated command prints nothing"
    );
}

/// Task 7: an `import` placeholder is expanded at load. The imported nodes run
/// under the placeholder's prefix, its incoming and outgoing edges are
/// spliced, and the persisted graph carries the expansion.
#[tokio::test]
async fn an_import_is_expanded_at_load_and_its_nodes_run_under_the_prefix() {
    let case = Case::new("import");
    fs::write(
        case.root.join("workflow").join("checks.fabro"),
        r#"digraph Checks {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    lint [shape=parallelogram, script="printf 'lint\n' >> log.txt"]
    test [shape=parallelogram, script="printf 'test\n' >> log.txt"]
    start -> lint -> test -> exit
}"#,
    )
    .expect("write the imported workflow");
    let workflow = case.workflow(
        r#"digraph Main {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    build [shape=parallelogram, script="printf 'build\n' > log.txt"]
    checks [import="checks.fabro"]
    ship [shape=parallelogram, script="cat log.txt"]
    start -> build -> checks -> ship -> exit
}"#,
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(
        finished.final_context()["command.output"],
        json!("build\nlint\ntest\n")
    );
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    assert_eq!(nodes, [
        "start",
        "build",
        "checks.lint",
        "checks.test",
        "ship",
        "exit"
    ]);
    let document = finished.inspect();
    let recorded = support::fabro::inspect::root_nodes(&document);
    assert!(recorded.get("checks.lint").is_some(), "{recorded}");
    assert!(recorded.get("checks").is_none(), "the placeholder is gone");
}

/// Keep the workspace path formula in one place the tests can see.
#[allow(dead_code, reason = "documents the layout the cases assert against")]
fn workspace_of(run_dir: &Path) -> PathBuf {
    run_dir
        .join("scopes")
        .join("invocation-0-scope-0")
        .join("work")
}

#[allow(dead_code, reason = "a bound every case shares")]
const _: Duration = support::fabro::launch::RUN_DEADLINE;

// ── Task 3: the parallel-result regression ──────────────────────────────────

use support::fabro::{BranchEnvelope, Petri, RunObservation, RunOutput, Scenario};

const SCENARIO: &str = "parallel-results";
const FINDERS: [&str; 2] = ["finder_a", "finder_b"];
const CONTRACT: &str = "parallel result contract:";

/// The report the pinned helper writes when both findings survive, as Fabro
/// produced it (`fabro-reference/raw/report.md`).
fn expected_report() -> String {
    fs::read_to_string(Scenario::source_file(
        SCENARIO,
        "fabro-reference/raw/report.md",
    ))
    .expect("the Fabro reference report is tracked")
}

/// The branch envelopes Fabro produced for the finder fan-out, from the
/// normalized capture, without `command.output` (Petri's command output ends
/// with a newline; the value is compared on its own).
fn fabro_finder_envelopes() -> Vec<BranchEnvelope> {
    let text = fs::read_to_string(Scenario::source_file(
        SCENARIO,
        "fabro-reference/normalized.json",
    ))
    .expect("the normalized Fabro capture is tracked");
    let capture: Value = serde_json::from_str(&text).expect("normalized.json is JSON");
    let group = capture["parallel_groups"]
        .as_array()
        .and_then(|groups| groups.iter().find(|g| g["node"] == "find"))
        .expect("the capture has the `find` group");
    group["results_from_dump"]
        .as_array()
        .expect("the group has dumped results")
        .iter()
        .map(|value| {
            let mut value = value.clone();
            if let Some(updates) = value["context_updates"].as_object_mut() {
                updates.remove("command.output");
            }
            BranchEnvelope::from_value(&value)
        })
        .collect()
}

fn run_scenario(label: &str) -> (Scenario, RunOutput) {
    let scenario = Scenario::stage(SCENARIO);
    let helper = scenario.file("helper/code_review.py");
    let output = Petri::run_workflow(&scenario.file("workflow.fabro"), &scenario.run_dir(label))
        .input("helper", helper.to_string_lossy())
        .input("level", "high")
        .input("target", "review-fixture")
        .run();
    assert!(
        output.success(),
        "the scenario runs to completion today:\n{}",
        output.stderr()
    );
    assert_eq!(output.run_status().as_deref(), Some("success"));
    (scenario, output)
}

fn finder_envelopes(observation: &RunObservation) -> Vec<BranchEnvelope> {
    let envelopes = observation
        .fan_in_output("find_join")
        .expect("the finder fan-in produced a list of envelopes");
    let ids: Vec<&str> = envelopes.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, FINDERS, "one envelope per branch, in branch order");
    envelopes
}

/// Strip `command.output` so an envelope compares on the finding it carries.
fn without_command_output(mut envelope: BranchEnvelope) -> BranchEnvelope {
    if let Some(updates) = envelope
        .context_updates
        .as_mut()
        .and_then(Value::as_object_mut)
    {
        updates.remove("command.output");
    }
    envelope
}

#[test]
fn contract_branch_envelopes_carry_index_status_and_context_updates() {
    let (_scenario, output) = run_scenario("contract-envelopes");
    let observation = RunObservation::load(&output.run_dir);
    let envelopes = finder_envelopes(&observation);
    assert_eq!(envelopes.len(), 2, "{CONTRACT} two branches, two envelopes");

    let expected = fabro_finder_envelopes();
    for (position, (actual, fabro)) in envelopes.iter().zip(&expected).enumerate() {
        assert_eq!(actual.id, fabro.id, "{CONTRACT} id at {position}");
        assert_eq!(
            actual.index,
            Some(position as u64),
            "{CONTRACT} index is the edge position at {position}: {actual:?}"
        );
        assert_eq!(
            actual.item_label, None,
            "{CONTRACT} static branches have no item_label: {actual:?}"
        );
        assert_eq!(
            actual.status.as_deref(),
            Some("succeeded"),
            "{CONTRACT} status uses Fabro's vocabulary at {position}: {actual:?}"
        );
        let updates = actual.context_updates.clone().unwrap_or_else(|| {
            panic!("{CONTRACT} context_updates is present at {position}: {actual:?}")
        });
        assert_eq!(
            updates["output.finder"],
            fabro.context_updates.as_ref().expect("fabro has updates")["output.finder"],
            "{CONTRACT} each branch keeps its own output.finder at {position}"
        );
        assert_eq!(
            without_command_output(actual.clone()),
            *fabro,
            "{CONTRACT} envelope {position} matches the Fabro capture"
        );
    }
    // The parent context does not absorb branch-local keys.
    let context = observation.final_context();
    assert!(
        context.get("output.finder").is_none(),
        "{CONTRACT} output.finder stays branch-local: {context}"
    );
}

#[test]
fn contract_helper_merges_both_findings_into_the_report() {
    let (_scenario, output) = run_scenario("contract-report");
    let observation = RunObservation::load(&output.run_dir);

    let merge_find = output.echoed("merge_find");
    assert!(
        merge_find.contains("Pooled 2 candidates into 2 locations"),
        "{CONTRACT} merge_find pools both candidates:\n{merge_find}"
    );
    let context = observation.final_context();
    assert_eq!(
        context["candidate_count"],
        json!(2),
        "{CONTRACT} candidate_count"
    );
    assert_eq!(context["run_verify"], json!(true), "{CONTRACT} run_verify");
    assert_eq!(
        context["verified_count"],
        json!(2),
        "{CONTRACT} verified_count"
    );
    assert_eq!(context["reported"], json!(2), "{CONTRACT} reported");
    assert_eq!(
        context["parallel.branch_count"],
        json!(2),
        "{CONTRACT} parallel.branch_count is published"
    );
    assert_eq!(
        output.echoed("report"),
        expected_report(),
        "{CONTRACT} the report is byte-identical to Fabro's"
    );
    assert_eq!(observation.final_status(), Some("success"));
    let history: Vec<String> = output
        .node_history()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    for node in [
        "verify",
        "verifier_a",
        "verifier_b",
        "verify_join",
        "merge_verify",
    ] {
        assert!(
            history.iter().any(|n| n == node),
            "{CONTRACT} {node} ran: {history:?}"
        );
    }
}

// ── Readiness item 6 through the binary (task 9) ────────────────────────────

/// A scripted `invalid` answer is rejected by the gate and the re-ask
/// (`ask: 2`) is answered by its own entry; both land in the receipt.
#[tokio::test]
async fn an_invalid_scripted_answer_is_re_asked_and_the_second_entry_routes() {
    let case = Case::new("reask");
    let workflow = case.workflow(GATE_ONLY, None);
    let script = interview::write(&case.root, "reask", &[
        interview::entry_matching(
            "first-try",
            json!({ "node": "gate", "ask": 1 }),
            1,
            json!({ "kind": "invalid", "value": "maybe" }),
        ),
        interview::entry_matching(
            "second-try",
            json!({ "node": "gate", "ask": 2 }),
            1,
            interview::negative(),
        ),
    ]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    let questions = receipt["questions"].as_array().expect("questions");
    assert_eq!(questions.len(), 2, "{receipt}");
    assert_eq!(questions[0]["ask"], json!(1));
    assert_eq!(questions[0]["occurrence"], json!(1));
    assert_eq!(questions[0]["reply"]["choice"], json!("maybe"));
    assert_eq!(questions[1]["ask"], json!(2));
    assert_eq!(questions[1]["occurrence"], json!(1));
    assert_eq!(questions[1]["reply"]["choice"], json!("N"));
    assert_eq!(questions[0]["question"], questions[1]["question"]);
    assert_eq!(finished.final_context()["human.gate.selected"], json!("N"));
    assert!(
        finished
            .echoed()
            .iter()
            .any(|(node, line)| node == "gate" && line.contains("names no choice")),
        "{}",
        finished.stderr
    );
    finished.assert_no_leaked_processes().await;
}

/// A delayed reply lands on its gate after the delay.
#[tokio::test]
async fn a_delayed_reply_lands_on_its_gate() {
    let case = Case::new("delayed");
    let workflow = case.workflow(GATE_ONLY, None);
    let script = interview::write(&case.root, "delayed", &[json!({
        "id": "later",
        "match": { "node": "gate" },
        "delay_ms": 800,
        "action": { "kind": "choice", "value": "Y" }
    })]);
    let started = Instant::now();
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    assert!(started.elapsed() >= Duration::from_millis(800));
    assert_eq!(
        finished.receipt()["questions"][0]["reply"]["choice"],
        json!("Y")
    );
    assert_eq!(finished.final_context()["human.gate.selected"], json!("Y"));
    finished.assert_no_leaked_processes().await;
}

const TIMED_GATE: &str = r#"digraph Gate {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no", timeout="1s", human.default_choice="hold"]
    ship [shape=parallelogram, script="echo shipped"]
    hold [shape=parallelogram, script="echo held"]
    start -> gate
    gate -> ship [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    ship -> exit
    hold -> exit
}"#;

/// A withheld reply lets the gate's answer deadline expire; the gate takes
/// `human.default_choice`, and the receipt records the question as timed
/// out with the default the gate took, nothing delivered.
#[tokio::test]
async fn a_withheld_reply_expires_into_the_default_choice() {
    let case = Case::new("withheld-default");
    let workflow = case.workflow(TIMED_GATE, None);
    let script = interview::write(&case.root, "withhold", &[json!({
        "id": "never",
        "match": { "node": "gate" },
        "action": { "kind": "withhold" }
    })]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(nodes.contains(&"hold".to_owned()), "{nodes:?}");
    assert!(!nodes.contains(&"ship".to_owned()), "{nodes:?}");
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    assert_eq!(
        receipt["questions"][0]["reply"],
        json!({ "kind": "timed_out", "default": "N" }),
        "{receipt}"
    );
    assert_eq!(receipt["questions"][0]["delivery"], json!("expired"));
    assert_eq!(receipt["questions"][0]["timeout_ms"], json!(1000));
    let context = finished.final_context();
    assert_eq!(context["human.gate.selected"], json!("N"));
    assert_eq!(context["human.gate.gate.answer"], json!("timeout"));
    assert!(
        finished
            .echoed()
            .iter()
            .any(|(node, line)| node == "gate" && line.contains("the question expired")),
        "{}",
        finished.stderr
    );
    finished.assert_no_leaked_processes().await;
}

/// Without a default, an expired gate fails with Fabro's retry outcome and
/// the run ends failed; the receipt records the question as timed out with
/// no default, and the withheld reply is not an interview error.
#[tokio::test]
async fn a_withheld_reply_without_a_default_fails_with_the_retry_outcome() {
    let case = Case::new("withheld-retry");
    let workflow = case.workflow(
        r#"digraph Gate {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no", timeout="500ms"]
    ship [shape=parallelogram, script="echo shipped"]
    start -> gate
    gate -> ship [label="[Y] Yes"]
    ship -> exit
}"#,
        None,
    );
    let script = interview::write(&case.root, "withhold", &[json!({
        "id": "never",
        "match": { "node": "gate" },
        "action": { "kind": "withhold" }
    })]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("failed"),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    assert_eq!(
        receipt["questions"][0]["reply"],
        json!({ "kind": "timed_out" }),
        "{receipt}"
    );
    assert_eq!(receipt["questions"][0]["delivery"], json!("expired"));
    let document = finished.inspect();
    let gate = &document["executions"][0]["engine"]["context"]["nodes"]["gate"];
    assert_eq!(
        gate["failure"]["class"],
        json!("retry_requested"),
        "{document}"
    );
    finished.assert_no_leaked_processes().await;
}

/// The probe of review finding G05: a gate whose question expires with no
/// default fails with Fabro's retry outcome; with `max_retries=0` that is
/// the exhausted retry. Under `on_failure="succeed"` the explicit
/// `outcome=failed` edge is checked first, as Fabro's executor checks it,
/// so the run recovers instead of promoting the gate past that edge.
#[tokio::test]
async fn an_expired_gate_under_succeed_takes_its_explicit_failure_edge() {
    let case = Case::new("expired-gate-succeed");
    let workflow = case.workflow(
        r#"digraph Probe {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    gate [shape=hexagon, label="Continue?", timeout="200ms", max_retries=0, on_failure="succeed"]
    recover [shape=parallelogram, script="echo RECOVERY"]
    fallthrough [shape=parallelogram, script="echo FALLTHROUGH"]
    start -> gate
    gate -> recover [condition="outcome=failed", label="[R] Recover"]
    gate -> fallthrough [label="[Y] Continue"]
    recover -> exit
    fallthrough -> exit
}"#,
        None,
    );
    let script = interview::write(&case.root, "withhold", &[json!({
        "id": "never",
        "match": { "node": "gate" },
        "action": { "kind": "withhold" }
    })]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let nodes: Vec<(String, String)> = finished.finished_nodes();
    assert!(
        nodes.contains(&("failure".to_owned(), "gate".to_owned())),
        "the exhausted gate stays failed: {nodes:?}"
    );
    assert!(
        nodes.iter().any(|(_, node)| node == "recover"),
        "the explicit failure edge is taken: {nodes:?}"
    );
    assert!(
        !nodes.iter().any(|(_, node)| node == "fallthrough"),
        "the failure is not promoted past its explicit route: {nodes:?}"
    );
    assert!(
        finished
            .echoed()
            .iter()
            .any(|(node, line)| node == "recover" && line.contains("RECOVERY")),
        "{}",
        finished.stderr
    );
    let document = finished.inspect();
    let gate = &document["executions"][0]["engine"]["context"]["nodes"]["gate"];
    assert_eq!(
        gate["failure"]["class"],
        json!("retry_requested"),
        "the original failure evidence is kept: {document}"
    );
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    finished.assert_no_leaked_processes().await;
}

/// Two gates in parallel branches: each answer is bound to its own
/// question by node, the late one lands after the early one, and neither
/// branch consumes the other's entry.
#[tokio::test]
async fn parallel_gates_bind_each_answer_to_its_own_branch() {
    // Two human gates as the branches of one parallel node. A branch runs
    // its target and returns to the join, as Fabro runs it, so each gate's
    // answer is read from its branch result; the join's successor writes the
    // results to a file the test reads back.
    let case = Case::new("parallel-gates");
    let workflow = case.workflow(
        r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    fan [shape=component]
    a [shape=hexagon, label="A?", question_type="yes_no"]
    b [shape=hexagon, label="B?", question_type="yes_no"]
    join [shape=tripleoctagon]
    report [shape=parallelogram, script="cat > results.json", stdin_source="context.parallel.results"]
    start -> fan
    fan -> a
    fan -> b
    a -> join [label="[Y] Yes"]
    a -> join [label="[N] No"]
    b -> join [label="[Y] Yes"]
    b -> join [label="[N] No"]
    join -> report -> exit
}"#,
        None,
    );
    let script = interview::write(&case.root, "parallel", &[
        json!({
            "id": "a-late",
            "match": { "node": "a" },
            "delay_ms": 600,
            "action": { "kind": "negative" }
        }),
        interview::entry("b-now", "b", interview::choice("Y")),
    ]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    let results: Value = serde_json::from_str(
        &fs::read_to_string(case.workspace().join("results.json")).expect("results.json"),
    )
    .expect("the fan-in's results are JSON");
    let results = results.as_array().expect("one envelope per branch");
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0]["id"], json!("a"));
    assert_eq!(results[0]["index"], json!(0));
    assert_eq!(
        results[0]["context_updates"]["human.gate.selected"],
        json!("N"),
        "{results:?}"
    );
    assert_eq!(results[1]["id"], json!("b"));
    assert_eq!(results[1]["index"], json!(1));
    assert_eq!(
        results[1]["context_updates"]["human.gate.selected"],
        json!("Y"),
        "{results:?}"
    );
    // The answers stay in their branches: the parent context has no
    // `human.gate.selected`.
    let context = finished.final_context();
    assert!(!context.contains_key("human.gate.selected"), "{context:?}");
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    let questions = receipt["questions"].as_array().expect("questions");
    assert_eq!(questions.len(), 2);
    for question in questions {
        assert_eq!(question["delivery"], json!("delivered"));
        let expected = if question["node"] == "a" { "N" } else { "Y" };
        assert_eq!(question["reply"]["choice"], json!(expected), "{question}");
        // The path is the branch child's call slot: the fork, the fork's
        // firing (its occurrence), the branch index and the target.
        let index = i32::from(question["node"] != "a");
        let path = question["invocation_path"].as_str().expect("a path");
        assert!(path.starts_with("/branch:fan@"), "{question}");
        assert!(
            path.ends_with(&format!(
                ":{index}:{}",
                question["node"].as_str().expect("node")
            )),
            "{question}"
        );
    }
    // Each gate fired in its own branch execution; the identity a reply
    // binds to is the execution and the firing together.
    assert_ne!(
        (&questions[0]["execution"], &questions[0]["firing"]),
        (&questions[1]["execution"], &questions[1]["firing"])
    );
    for entry in receipt["script"]["entries"].as_array().expect("entries") {
        assert_eq!(entry["consumed"], json!(1), "{entry}");
    }
    finished.assert_no_leaked_processes().await;
}

/// A `multi_select` gate takes several keys: the first routes, and every
/// selected key and label is recorded, as Fabro records them.
#[tokio::test]
async fn a_multi_select_answer_routes_on_the_first_key_and_records_all() {
    let case = Case::new("multi-select");
    let workflow = case.workflow(
        r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    pick [shape=hexagon, label="Which?", question_type="multi_select"]
    apply [shape=parallelogram, script="echo apply"]
    review [shape=parallelogram, script="echo review"]
    start -> pick
    pick -> apply [label="[A] Apply"]
    pick -> review [label="[R] Review"]
    apply -> exit
    review -> exit
}"#,
        None,
    );
    let script = interview::write(&case.root, "multi", &[interview::entry_matching(
        "both",
        json!({ "node": "pick", "kind": "multi_select", "options": ["A", "R"] }),
        1,
        json!({ "kind": "choices", "values": ["A", "R"] }),
    )]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    let context = finished.final_context();
    assert_eq!(context["human.gate.selected"], json!("A,R"));
    assert_eq!(context["human.gate.label"], json!("[A] Apply, [R] Review"));
    let receipt = finished.receipt();
    assert_eq!(
        receipt["questions"][0]["reply"]["choices"],
        json!(["A", "R"])
    );
    assert_eq!(receipt["questions"][0]["reply"]["choice"], json!("A"));
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(nodes.contains(&"apply".to_owned()) && !nodes.contains(&"review".to_owned()));
    finished.assert_no_leaked_processes().await;
}

/// A `review_target` gate asks Fabro's review question, shows the URL in the
/// terminal, and the script matches on the reference.
#[tokio::test]
async fn a_review_target_gate_shows_its_reference_in_the_terminal() {
    let case = Case::new("review-target");
    let target = r#"{\"context_updates\":{\"review_target\":{\"label\":\"the plan\",\"url\":\"https://quarry.lithos.computer/tmp/abc\",\"kind\":\"document\"}}}"#;
    let workflow = case.workflow(
        &format!(
            r#"digraph G {{
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prep [shape=parallelogram, output_schema="routing", script="echo '{target}'"]
    gate [shape=hexagon, label="Ship it?", review_target=true]
    ship [shape=parallelogram, script="echo shipped"]
    hold [shape=parallelogram, script="echo held"]
    start -> prep -> gate
    gate -> ship [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    ship -> exit
    hold -> exit
}}"#
        ),
        None,
    );
    let script = interview::write(&case.root, "review", &[interview::entry_matching(
        "reviewed",
        json!({
            "node": "gate",
            "text_contains": "Review the the plan document",
            "reference_url_contains": "quarry.lithos.computer/tmp/abc"
        }),
        1,
        interview::choice("Y"),
    )]);
    let finished = case
        .run(&workflow, &[
            "--interview-script",
            script.to_str().expect("utf-8"),
        ])
        .await;
    finished.assert_code(0);
    assert!(
        finished.echoed().iter().any(|(node, line)| {
            node == "gate" && line == "review: the plan <https://quarry.lithos.computer/tmp/abc>"
        }),
        "{}",
        finished.stderr
    );
    let receipt = finished.receipt();
    assert_eq!(
        receipt["questions"][0]["reference"]["label"],
        json!("the plan")
    );
    assert_eq!(
        receipt["questions"][0]["reference"]["url"],
        json!("https://quarry.lithos.computer/tmp/abc")
    );
    assert_eq!(
        receipt["questions"][0]["text"],
        json!("Review the the plan document, then choose the next action.")
    );
    finished.assert_no_leaked_processes().await;
}

/// `--control`: a paused run admits nothing until `unpause`; a `steer` while
/// a gate waits reaches the stage and does not answer it.
#[tokio::test]
async fn the_control_file_pauses_unpauses_and_steers_without_answering() {
    let case = Case::new("control-file");
    let workflow = case.workflow(
        r#"digraph G {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="echo started > started.txt; echo prepared"]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no"]
    ship [shape=parallelogram, script="echo shipped"]
    hold [shape=parallelogram, script="echo held"]
    start -> prepare -> gate
    gate -> ship [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    ship -> exit
    hold -> exit
}"#,
        None,
    );
    let control = case.root.join("controls.txt");
    fs::write(&control, "").expect("control file");
    // The gate's answer waits for the steer to have landed.
    let script = interview::write(&case.root, "gate", &[json!({
        "id": "hold-it",
        "match": { "node": "gate" },
        "delay_ms": 1500,
        "action": { "kind": "negative" }
    })]);
    let started = case.workspace().join("started.txt");
    let control_arg = control.to_str().expect("utf-8").to_owned();
    // Each control waits for the run to report the previous one, not for a
    // guessed delay: the steer has to reach the gate before the script's
    // answer does, and a loaded machine makes any margin a coin toss.
    let paused = case.root.join("saw-paused");
    let unpaused = case.root.join("saw-unpaused");
    let finished = case
        .run_with(
            &workflow,
            &[
                "--interview-script",
                script.to_str().expect("utf-8"),
                "--control",
                &control_arg,
            ],
            Launch {
                mark_when_stderr: vec![
                    ("control: paused".to_owned(), paused.clone()),
                    ("control: unpaused".to_owned(), unpaused.clone()),
                ],
                append_when: vec![
                    // Pause right after `prepare` starts: `gate` is held.
                    (
                        started,
                        control.clone(),
                        "pause\n".into(),
                        Duration::from_millis(0),
                    ),
                    (
                        paused,
                        control.clone(),
                        "unpause\n".into(),
                        Duration::from_millis(0),
                    ),
                    // The gate is now waiting on its (delayed) answer.
                    (
                        unpaused,
                        control,
                        "steer gate please decide\nsteer nobody hi\ndance\n".into(),
                        Duration::from_millis(0),
                    ),
                ],
                ..Launch::default()
            },
        )
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    let mut position = 0;
    for expected in [
        "control: paused",
        "control: unpaused",
        "control: steered gate",
        "control: no stage named `nobody` is running",
        "control: `dance` is not a control",
    ] {
        let found = finished.stderr[position..]
            .find(expected)
            .unwrap_or_else(|| panic!("{expected} in order\n{}", finished.stderr));
        position += found + expected.len();
    }
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    assert_eq!(receipt["questions"][0]["reply"]["choice"], json!("N"));
    assert_eq!(finished.final_context()["human.gate.selected"], json!("N"));
    // The steer and the answer both reached the gate's firing.
    let document = finished.inspect();
    let deliveries = document["executions"][0]["engine"]["deliveries"]
        .as_array()
        .expect("deliveries");
    assert_eq!(deliveries.len(), 2, "{deliveries:?}");
    assert!(
        deliveries[0]["payload"].get("$steer").is_some(),
        "{deliveries:?}"
    );
    assert!(
        deliveries[1]["payload"].get("$answer").is_some(),
        "{deliveries:?}"
    );
    finished.assert_no_leaked_processes().await;
}

/// `stall_timeout`: a run with no execution activity for the budget is
/// cancelled and the terminal says why.
#[tokio::test]
async fn a_stalled_run_is_cancelled_by_the_watchdog() {
    let case = Case::new("stall");
    let workflow = case.workflow(
        r#"digraph G {
    graph [stall_timeout="500ms"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    long [shape=parallelogram, script="sleep 30"]
    start -> long -> exit
}"#,
        None,
    );
    let started = Instant::now();
    let finished = case.run(&workflow, &["--auto-approve"]).await;
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    assert!(
        finished
            .stderr
            .contains("stall watchdog: no execution activity"),
        "{}",
        finished.stderr
    );
    finished.assert_no_leaked_processes().await;
}

// ── Dynamic branches through the twin ────────────────────────────────────────

/// A `for_each` fan-out whose template is an API agent answered by the twin:
/// each item's review reports a finding under the same context key.
fn for_each_workflow(jobs: &str) -> String {
    format!(
        r#"digraph Dynamic {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    plan [shape=parallelogram, output_schema="routing", script="printf '%s' '{{\"context_updates\":{{\"jobs\":{jobs}}}}}'"]
    fan [shape=component, for_each="context.jobs", max_parallel=3]
    job [prompt="Review the item and report one finding.", model="{model}", provider="openai", output_schema="routing"]
    join [shape=tripleoctagon]
    report [shape=parallelogram, script="cat > results.json", stdin_source="context.parallel.results"]
    start -> plan -> fan -> job -> join -> report -> exit
}}"#,
        model = model(Provider::OpenAi),
    )
}

/// The twin's answer for one item: a routing directive that writes the item's
/// name under `output.finder`. The item reaches the model as fenced JSON, so
/// its `name` line is what the scenario matches. `delay_ms` holds the answer
/// back, so the branch finishes after its siblings.
fn finding_for(namespace: &str, item: &str, delay_ms: u64) -> Value {
    let mut script = text(&format!(
        r#"{{"outcome":"succeeded","context_updates":{{"output.finder":{{"found":"{item}"}}}}}}"#
    ));
    if delay_ms > 0 {
        script["delay_before_headers_ms"] = json!(delay_ms);
    }
    scenario(
        Provider::OpenAi,
        namespace,
        item,
        model(Provider::OpenAi),
        &format!(r#""name": "{item}""#),
        script,
    )
}

/// The typed fork lifecycle of a `for_each` fan-out, read back through the
/// public event stream: one `fork_started` on the parallel node `fan` with
/// one branch per item, one `branch_completed` per clone in item order, and
/// one `fork_completed` at the fan-in with the results in the same order.
/// Zero items is a fork with zero branches and no `branch_completed`.
async fn assert_for_each_fork_events(run_dir: &Path, items: u32) {
    let events = replay_run_dir(run_dir).await.expect("the run replays");
    let mut forks = Vec::new();
    let mut branches = Vec::new();
    let mut joins = Vec::new();
    for event in &events {
        let node = event
            .subject
            .as_ref()
            .map(|subject| subject.node.name.to_string())
            .unwrap_or_default();
        let kind = event
            .subject
            .as_ref()
            .and_then(|subject| subject.node.meta["kind"].as_str())
            .unwrap_or_default()
            .to_owned();
        match event.view() {
            Some(ViewEvent::ForkStarted { branches: refs, .. }) => {
                forks.push((node, kind, refs.iter().map(|b| b.index).collect::<Vec<_>>()));
            }
            Some(ViewEvent::BranchCompleted { result, .. }) => {
                branches.push((
                    result.branch.index,
                    result.node.name.to_string(),
                    result.status.tag().to_owned(),
                ));
            }
            Some(ViewEvent::ForkCompleted { fork, results, .. }) => {
                joins.push((
                    node,
                    kind,
                    fork.name.to_string(),
                    results.iter().map(|r| r.branch.index).collect::<Vec<_>>(),
                ));
            }
            _ => {}
        }
    }
    let indices: Vec<u32> = (0..items).collect();
    assert_eq!(
        forks,
        vec![("fan".to_owned(), "parallel".to_owned(), indices.clone())],
        "{events:#?}"
    );
    let expected: Vec<(u32, String, String)> = indices
        .iter()
        .map(|index| (*index, format!("job#{index}"), "success".to_owned()))
        .collect();
    assert_eq!(branches, expected);
    assert_eq!(joins, vec![(
        "join".to_owned(),
        "parallel.fan_in".to_owned(),
        "fan".to_owned(),
        indices
    )]);
}

fn results_file(case: &Case) -> Vec<Value> {
    let text = fs::read_to_string(case.workspace().join("results.json")).expect("results.json");
    serde_json::from_str::<Value>(&text)
        .expect("results.json is JSON")
        .as_array()
        .cloned()
        .expect("one envelope per branch")
}

/// Three items under one key: every branch keeps its own value, the list
/// follows item order even though the first item's answer arrives last, and
/// nothing reaches the parent context.
#[tokio::test]
async fn for_each_branches_keep_distinct_values_under_one_key_in_item_order() {
    let mut case = Case::new("for-each-many");
    let twin = Twin::start(Provider::OpenAi, &case.root.join("twins"), vec![
        finding_for(&case.credential, "alpha", 800),
        finding_for(&case.credential, "beta", 0),
        finding_for(&case.credential, "gamma", 0),
    ])
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &for_each_workflow(r#"[{\"name\":\"alpha\"},{\"name\":\"beta\"},{\"name\":\"gamma\"}]"#),
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_for_each_fork_events(&finished.run_dir, 3).await;
    let results = results_file(&case);
    assert_eq!(results.len(), 3, "{results:?}");
    for (index, (envelope, item)) in results.iter().zip(["alpha", "beta", "gamma"]).enumerate() {
        assert_eq!(envelope["id"], json!("job"), "{envelope}");
        assert_eq!(envelope["index"], json!(index), "{envelope}");
        assert_eq!(envelope["item_label"], json!(item), "{envelope}");
        assert_eq!(envelope["status"], json!("succeeded"), "{envelope}");
        assert_eq!(
            envelope["context_updates"]["output.finder"]["found"],
            json!(item),
            "each branch keeps its own value: {envelope}"
        );
    }
    let context = finished.final_context();
    assert!(!context.contains_key("output.finder"), "{context:?}");
    assert_eq!(context["parallel.branch_count"], json!(3));
    // The delayed first item finished last; the list still follows item order.
    let finished_jobs: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, node)| node)
        .filter(|node| node.starts_with("job#"))
        .collect();
    assert_eq!(
        finished_jobs.last().map(String::as_str),
        Some("job#0"),
        "{finished_jobs:?}"
    );
    let mut consumed = twin.consumed();
    consumed.sort();
    assert_eq!(consumed, ["alpha", "beta", "gamma"]);
    assert_eq!(twin.unmatched(), 0);
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Fifty items, each a name and a 100-character brief: every branch child is
/// declared from a reference to the list, not a copy; the model's prompt in
/// each branch still shows the list (the preamble restores what the fork
/// offloaded); the joined results are published as a reference that the
/// downstream command reads back whole and the inspect document shows.
#[tokio::test]
async fn a_fifty_item_fork_declares_small_children_and_prompts_still_see_the_list() {
    let mut case = Case::new("for-each-fifty");
    let items: Vec<String> = (0..50).map(|i| format!("job-{i}")).collect();
    let twin = Twin::start(
        Provider::OpenAi,
        &case.root.join("twins"),
        items
            .iter()
            .map(|item| finding_for(&case.credential, item, 0))
            .collect(),
    )
    .await;
    case.redirect(&twin);
    let brief = "x".repeat(100);
    let jobs = items
        .iter()
        .map(|item| format!(r#"{{\"name\":\"{item}\",\"brief\":\"{brief}\"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let workflow = case.workflow(&for_each_workflow(&format!("[{jobs}]")), None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    let results = results_file(&case);
    assert_eq!(results.len(), 50, "the report read the whole list on stdin");
    for (index, (envelope, item)) in results.iter().zip(&items).enumerate() {
        assert_eq!(envelope["index"], json!(index), "{envelope}");
        assert_eq!(envelope["item_label"], json!(item), "{envelope}");
        assert_eq!(
            envelope["context_updates"]["output.finder"]["found"],
            json!(item),
            "{envelope}"
        );
    }
    // The inspect document: every child declared from the same reference to
    // the list, none from a copy; the joined results a reference too.
    let document = finished.inspect();
    let children: Vec<&Value> = document["invocations"]
        .as_array()
        .expect("invocations")
        .iter()
        .filter(|i| {
            i["parent"]["slot"]
                .as_str()
                .is_some_and(|slot| slot.starts_with("branch:fan@"))
        })
        .collect();
    assert_eq!(children.len(), 50);
    let mut list_refs = BTreeSet::new();
    for child in &children {
        let declared = serde_json::to_string(&child["context"]).expect("json");
        assert!(
            declared.len() < 2 * 1024,
            "a child's declared context is small: {declared}"
        );
        assert_eq!(
            declared.matches("job-").count(),
            1,
            "the only item in a child's context is its own: {declared}"
        );
        let jobs = child["context"]["jobs"]
            .as_str()
            .expect("the list is a reference");
        assert!(jobs.starts_with("blob://sha256/"), "{jobs}");
        list_refs.insert(jobs.to_owned());
    }
    assert_eq!(list_refs.len(), 1, "one blob, shared: {list_refs:?}");
    let context = finished.final_context();
    let stored = context["parallel.results"]
        .as_str()
        .expect("the joined results are a reference");
    assert!(stored.starts_with("blob://sha256/"), "{stored}");
    assert_eq!(context["parallel.branch_count"], json!(50));
    // The model saw the list in every branch: the preamble's context row is
    // the restored value, not the reference the child's context holds.
    let bodies = twin.requests_for(&case.credential);
    assert_eq!(bodies.len(), 50);
    for body in &bodies {
        let text = serde_json::to_string(body).expect("json");
        assert!(
            text.contains("- jobs: [{"),
            "the prompt shows the list: {text}"
        );
        assert!(
            !text.contains("- jobs: blob://"),
            "the prompt never shows the reference: {text}"
        );
    }
    let mut consumed = twin.consumed();
    consumed.sort();
    let mut expected = items.clone();
    expected.sort();
    assert_eq!(consumed, expected);
    assert_eq!(twin.unmatched(), 0);
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// One item is one branch.
#[tokio::test]
async fn a_for_each_over_one_item_is_one_branch() {
    let mut case = Case::new("for-each-one");
    let twin = Twin::start(Provider::OpenAi, &case.root.join("twins"), vec![
        finding_for(&case.credential, "solo", 0),
    ])
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(&for_each_workflow(r#"[{\"name\":\"solo\"}]"#), None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    let results = results_file(&case);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["item_label"], json!("solo"));
    assert_eq!(results[0]["index"], json!(0));
    assert_eq!(
        results[0]["context_updates"]["output.finder"]["found"],
        json!("solo")
    );
    assert_eq!(finished.final_context()["parallel.branch_count"], json!(1));
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// An empty list joins with no branches and calls no model.
#[tokio::test]
async fn an_empty_for_each_list_joins_without_calling_the_model() {
    let mut case = Case::new("for-each-empty");
    let twin = Twin::start(Provider::OpenAi, &case.root.join("twins"), Vec::new()).await;
    case.redirect(&twin);
    let workflow = case.workflow(&for_each_workflow("[]"), None);
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    // The fork starts and closes with zero branches: the placeholder clone
    // the lowering fires to reach the fan-in is no branch in the public
    // stream.
    assert_for_each_fork_events(&finished.run_dir, 0).await;
    assert_eq!(results_file(&case), Vec::<Value>::new());
    let context = finished.final_context();
    assert_eq!(context["parallel.branch_count"], json!(0));
    assert_eq!(context["parallel.results"], json!([]));
    assert!(twin.requests().is_empty(), "no model call for no items");
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// A branch whose model call fails keeps its identity and reports a failed
/// status with no stale success data; its sibling is unaffected and the
/// fan-in joins partially.
#[tokio::test]
async fn a_failed_branch_keeps_its_identity_without_success_data() {
    let mut case = Case::new("for-each-failed");
    // No scenario for `beta`: its call is unmatched and its agent fails.
    let twin = Twin::start(Provider::OpenAi, &case.root.join("twins"), vec![
        finding_for(&case.credential, "alpha", 0),
    ])
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &for_each_workflow(r#"[{\"name\":\"alpha\"},{\"name\":\"beta\"}]"#),
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    let results = results_file(&case);
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0]["status"], json!("succeeded"));
    assert_eq!(
        results[0]["context_updates"]["output.finder"]["found"],
        json!("alpha")
    );
    let failed = &results[1];
    assert_eq!(failed["id"], json!("job"));
    assert_eq!(failed["index"], json!(1));
    assert_eq!(failed["item_label"], json!("beta"));
    assert_eq!(failed["status"], json!("failed"), "{failed}");
    assert!(
        failed["context_updates"].get("output.finder").is_none(),
        "no success data on a failed branch: {failed}"
    );
    let joins: Vec<(String, String)> = finished
        .finished_nodes()
        .into_iter()
        .filter(|(_, node)| node == "join")
        .collect();
    assert_eq!(joins, vec![(
        "partial_success".to_owned(),
        "join".to_owned()
    )]);
    assert!(twin.unmatched() >= 1);
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// Nested joins: an outer fork whose branch is an inner fork reports the
/// inner results inside its own envelope, and the outer join sees exactly
/// its two branches.
#[tokio::test]
async fn nested_joins_report_the_inner_results_inside_the_outer_envelope() {
    let case = Case::new("nested-joins");
    let workflow = case.workflow(
        r#"digraph Nested {
    start [shape=Mdiamond]
    exit [shape=Msquare]
    outer [shape=component]
    x [shape=parallelogram, output_schema="routing", script="printf '%s' '{\"context_updates\":{\"output.x\":\"x\"}}'"]
    inner [shape=component]
    p [shape=parallelogram, output_schema="routing", script="printf '%s' '{\"context_updates\":{\"output.p\":\"p\"}}'"]
    q [shape=parallelogram, output_schema="routing", script="printf '%s' '{\"context_updates\":{\"output.q\":\"q\"}}'"]
    inner_join [shape=tripleoctagon]
    outer_join [shape=tripleoctagon]
    report [shape=parallelogram, script="cat > results.json", stdin_source="context.parallel.results"]
    start -> outer
    outer -> x
    outer -> inner
    inner -> p
    inner -> q
    p -> inner_join
    q -> inner_join
    x -> outer_join
    inner_join -> outer_join
    outer_join -> report -> exit
}"#,
        None,
    );
    let finished = case.run(&workflow, &[]).await;
    finished.assert_code(0);
    let results = results_file(&case);
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0]["id"], json!("x"));
    assert_eq!(results[0]["context_updates"]["output.x"], json!("x"));
    assert_eq!(results[1]["id"], json!("inner"));
    assert_eq!(
        results[1]["context_updates"]["parallel.branch_count"],
        json!(2)
    );
    let inner = results[1]["context_updates"]["parallel.results"]
        .as_array()
        .cloned()
        .expect("inner results");
    assert_eq!(inner.len(), 2);
    assert_eq!(inner[0]["id"], json!("p"));
    assert_eq!(inner[0]["context_updates"]["output.p"], json!("p"));
    assert_eq!(inner[1]["id"], json!("q"));
    let context = finished.final_context();
    for key in ["output.x", "output.p", "output.q"] {
        assert!(
            !context.contains_key(key),
            "{key} stays in its branch: {context:?}"
        );
    }
    assert_eq!(context["parallel.branch_count"], json!(2));
    finished.assert_no_leaked_processes().await;
}

/// The pinned interview bundle names no model, and Fabro runs it with a
/// launch-level default (`fabro run --provider openai` picks the provider's
/// default model). `petri run --provider openai` does the same through the
/// runner's catalog: the `summarize` prompt runs on `gpt-5.6-sol`, and the
/// persisted root graph's `fabro.launch` parameter records the launch. With
/// no launch default the prompt node fails and names the options.
#[tokio::test]
async fn the_unchanged_interview_bundle_runs_with_a_launch_provider() {
    use support::fabro::bundle::Scenario;

    let bundle = Scenario::stage("interview");
    let workflow = bundle.file(".fabro/workflows/interview/workflow.fabro");
    let workflow_toml = fs::read_to_string(bundle.file(".fabro/workflows/interview/workflow.toml"))
        .expect("workflow.toml");
    assert_eq!(
        workflow_toml.trim(),
        "_version = 1",
        "the pinned bundle names no model"
    );
    let provider = Provider::OpenAi;
    let entries = [
        interview::entry("easy", "yes_no", interview::choice("Y")),
        interview::entry("continue", "confirmation", interview::choice("Y")),
        interview::entry("risks", "multiple_choice", interview::choice("R")),
        interview::entry(
            "blockers",
            "multi_select",
            json!({ "kind": "choices", "values": ["B"] }),
        ),
        interview::entry("nuance", "freeform", interview::text("ship on Friday")),
    ];
    let summarize = |namespace: &str| {
        vec![scenario(
            provider,
            namespace,
            "summarize",
            model(provider),
            "Summarize the full human interview",
            text("SUMMARY: ship on Friday."),
        )]
    };

    // `--provider openai`: the provider's default model.
    let mut case = Case::new("launch-provider");
    let twin = Twin::start(
        provider,
        &case.root.join("twins"),
        summarize(&case.credential),
    )
    .await;
    case.redirect(&twin);
    let script = interview::write(&case.root, "interview", &entries);
    // `--environment local`, as the pinned Fabro's harness launches every
    // run: the bundle's `.fabro/project.toml` selects a Daytona environment.
    let finished = case
        .run(&workflow, &[
            "--provider",
            "openai",
            "--environment",
            "local",
            "--interview-script",
            script.to_str().expect("utf-8 path"),
        ])
        .await;
    finished.assert_code(0);
    assert_eq!(
        finished.status_line(),
        Some("success"),
        "{}",
        finished.stderr
    );
    assert_eq!(twin.consumed(), ["summarize"]);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["model"], json!("gpt-5.6-sol"));
    let launch = launch_param(&finished.run_dir);
    assert_eq!(launch["provider"], json!("openai"));
    assert_eq!(launch["model"], json!(null));
    twin.stop();

    // `--provider` with `--model`: the named model, recorded as given.
    let mut case = Case::new("launch-model");
    let twin = Twin::start(
        provider,
        &case.root.join("twins"),
        summarize(&case.credential),
    )
    .await;
    case.redirect(&twin);
    let script = interview::write(&case.root, "interview", &entries);
    let finished = case
        .run(&workflow, &[
            "--provider",
            "openai",
            "--model",
            "gpt-5.6-sol",
            "--environment",
            "local",
            "--interview-script",
            script.to_str().expect("utf-8 path"),
        ])
        .await;
    finished.assert_code(0);
    assert_eq!(twin.consumed(), ["summarize"]);
    let launch = launch_param(&finished.run_dir);
    assert_eq!(launch["provider"], json!("openai"));
    assert_eq!(launch["model"], json!("gpt-5.6-sol"));
    twin.stop();

    // No launch default: the prompt node fails before any model call and
    // names the launch options. The bundle routes the failed `summarize` to
    // `exit`, so the run itself still finishes.
    let mut case = Case::new("launch-none");
    let twin = Twin::start(
        provider,
        &case.root.join("twins"),
        summarize(&case.credential),
    )
    .await;
    case.redirect(&twin);
    let script = interview::write(&case.root, "interview", &entries);
    let finished = case
        .run(&workflow, &[
            "--environment",
            "local",
            "--interview-script",
            script.to_str().expect("utf-8 path"),
        ])
        .await;
    assert!(!finished.timed_out, "the run exceeded its deadline");
    assert!(
        finished
            .finished_nodes()
            .contains(&("failure".to_owned(), "summarize".to_owned())),
        "{}",
        finished.stderr
    );
    let document = finished.inspect();
    let failure =
        document["executions"][0]["engine"]["context"]["nodes"]["summarize"]["failure"].to_string();
    assert!(
        failure.contains("names no model") && failure.contains("--provider"),
        "{failure}"
    );
    assert_eq!(twin.consumed(), Vec::<String>::new());
    let launch = launch_param(&finished.run_dir);
    assert_eq!(launch["provider"], json!(null));
    assert_eq!(launch["model"], json!(null));
    twin.stop();
}

/// The `fabro.launch` parameter of the run's persisted root graph
/// (`<run_dir>/graphs/<digest>.json`): the graph that carries it.
fn launch_param(run_dir: &Path) -> Value {
    let graphs = fs::read_dir(run_dir.join("graphs")).expect("the run's graphs directory");
    let mut found = Vec::new();
    for entry in graphs {
        let path = entry.expect("graph entry").path();
        let graph: Value =
            serde_json::from_slice(&fs::read(&path).expect("graph file")).expect("graph JSON");
        if let Some(launch) = graph["params"].get("fabro.launch") {
            found.push(launch.clone());
        }
    }
    assert_eq!(
        found.len(),
        1,
        "one root graph carries the launch: {found:?}"
    );
    found.remove(0)
}
