//! Readiness item 8, the intermediate workflow execution milestone, through
//! the shipped binary: one workflow combining setup commands, real commands,
//! a scripted native agent that edits a file, a retained thread across two
//! `full` fidelity agent nodes, project memory from the workspace, a
//! configured tool hook that blocks one effect and lets another through, a
//! human decision from an interview script, a bounded `for_each` fan-out whose
//! branch results a later command consumes, run-end hooks, and final file
//! checks. Provider twins on loopback; no Fabro, no database, no server, no
//! platform adapters. Failure and cancellation are separate cases.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use petri::execution::ExecutionId;
use petri::execution::events::replay_run_dir;
use serde_json::{Value, json};
use support::fabro::interview;
use support::fabro::launch::{Case, Finished, Launch};
use support::fabro::twins::{Provider, Twin, model, scenario, shell_tool, text, tool_call};

const MEMORY_RULE: &str = "Always sign release notes with -- petri";

/// The workflow. `tail` is what follows the fan-in's `report` node: the
/// success case checks the files, the failure case fails a check, the
/// cancellation case waits to be interrupted.
fn workflow(model: &str, tail: &str) -> String {
    format!(
        r#"digraph Milestone {{
    graph [backend="api", goal="Add a release note"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    plan [prompt="Plan the release note.", model="{model}", provider="openai", fidelity="full", thread_id="notes"]
    write [prompt="Write the release note.", model="{model}", provider="openai", fidelity="full", thread_id="notes", on_failure="exit"]
    gate [shape=hexagon, label="Ship it?", question_type="yes_no"]
    jobs [shape=parallelogram, output_schema="routing", script="printf '%s' '{{\"context_updates\":{{\"jobs\":[{{\"name\":\"alpha\"}},{{\"name\":\"beta\"}}]}}}}'"]
    fan [shape=component, for_each="context.jobs", max_parallel=2]
    job [prompt="Review the item and report one finding.", model="{model}", provider="openai", output_schema="routing"]
    join [shape=tripleoctagon]
    report [shape=parallelogram, script="cat > results.json", stdin_source="context.parallel.results"]
    hold [shape=parallelogram, script="echo held > decision.txt"]
    {tail}
    initialize [shape=parallelogram, script="printf 'draft\n' > notes.txt && printf 'keep\n' > protected.txt", timeout="30s", on_failure="exit"]
    memory [shape=parallelogram, script="printf 'Always sign release notes with -- petri\n' > AGENTS.md", timeout="30s", on_failure="exit"]
    start -> initialize -> memory -> plan -> write -> gate
    gate -> jobs [label="[Y] Yes"]
    gate -> hold [label="[N] No"]
    jobs -> fan -> job -> join -> report
    hold -> exit
}}"#
    )
}

const WORKFLOW_TOML: &str = r#"
[[run.hooks]]
name = "no-destruction"
event = "pre_tool_use"
matcher = "shell|Bash"
script = "if grep -q 'rm ' \"$FABRO_HOOK_CONTEXT\"; then echo '{\"decision\":\"block\",\"reason\":\"destructive commands are not allowed\"}'; exit 2; fi"

[[run.hooks]]
name = "log-tools"
event = "post_tool_use"
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
"#;

/// The twin's scripts for the agent phases: the plan, the write's blocked
/// `rm`, its allowed append, its final text, and one finding per item.
fn scripts(case: &Case) -> Vec<Value> {
    let provider = Provider::OpenAi;
    let model = model(provider);
    let shell = shell_tool(provider);
    let finding = |item: &str| {
        scenario(
            provider,
            &case.credential,
            &format!("job-{item}"),
            model,
            // The fenced item is pretty JSON (`"name": "alpha"`); the `jobs`
            // stage's compact output in the preamble is not.
            &format!("\"name\": \"{item}\""),
            text(&format!(
                r#"{{"outcome":"succeeded","context_updates":{{"output.finder":{{"found":"{item}"}}}}}}"#
            )),
        )
    };
    vec![
        scenario(
            provider,
            &case.credential,
            "plan",
            model,
            "Plan the release note",
            text("PLAN: one line, signed"),
        ),
        scenario(
            provider,
            &case.credential,
            "write-rm",
            model,
            "Write the release note",
            tool_call(
                "rm-call",
                shell,
                json!({ "command": "rm -f protected.txt && echo REMOVED" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "write-append",
            model,
            "destructive commands are not allowed",
            tool_call(
                "append-call",
                shell,
                json!({ "command": "printf 'reviewed -- petri\\n' >> notes.txt && echo APPENDED" }),
            ),
        ),
        scenario(
            provider,
            &case.credential,
            "write-done",
            model,
            "APPENDED",
            text("WROTE the note"),
        ),
        finding("alpha"),
        finding("beta"),
    ]
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

/// The agent phases as every case sees them: the plan and the write share
/// one conversation, the memory file reaches the model, the hook blocked the
/// `rm` and the append ran, the branches reported their findings.
fn assert_agent_phases(case: &Case, twin: &Twin, finished: &Finished) {
    let consumed = twin.consumed();
    assert_eq!(
        consumed[..4],
        ["plan", "write-rm", "write-append", "write-done"],
        "{}",
        finished.stderr
    );
    let mut branches = consumed[4..].to_vec();
    branches.sort();
    assert_eq!(branches, ["job-alpha", "job-beta"], "{}", finished.stderr);
    assert_eq!(twin.unmatched(), 0);
    let requests = twin.requests_for(&case.credential);
    assert_eq!(requests.len(), 6, "{requests:#?}");
    let first = serde_json::to_string(&requests[0]).expect("json");
    assert!(
        first.contains(MEMORY_RULE),
        "project memory (AGENTS.md written by setup) reaches the first request: {first}"
    );
    let second = serde_json::to_string(&requests[1]).expect("json");
    assert!(
        second.contains("Plan the release note") && second.contains("PLAN: one line, signed"),
        "the write node continues the plan's thread: {second}"
    );
    let third = serde_json::to_string(&requests[2]).expect("json");
    assert!(
        third.contains("destructive commands are not allowed"),
        "the model saw the block reason: {third}"
    );
    let workspace = case.workspace();
    assert_eq!(
        read(&workspace.join("protected.txt")),
        "keep\n",
        "the blocked rm never ran"
    );
    assert_eq!(
        read(&workspace.join("notes.txt")),
        "draft\nreviewed -- petri\n",
        "setup wrote the draft, the agent appended through the allowed tool"
    );
    assert_eq!(
        read(&workspace.join("tool-hooks.log")),
        "ran:write\n",
        "the post hook saw the one tool call that ran"
    );
    let echoed = finished.echoed();
    assert!(
        echoed
            .iter()
            .any(|(node, line)| node == "write" && line.contains("WROTE")),
        "agent output is attributed to its stage: {echoed:?}"
    );
}

/// The branch results a downstream command consumed, from the workspace.
fn results(case: &Case) -> Vec<Value> {
    let text = read(&case.workspace().join("results.json"));
    serde_json::from_str::<Value>(&text)
        .unwrap_or_else(|e| panic!("results.json is JSON: {e}\n{text}"))
        .as_array()
        .cloned()
        .expect("a list")
}

#[tokio::test]
async fn the_milestone_workflow_runs_end_to_end_through_the_binary() {
    let mut case = Case::new("milestone-success");
    let twin = Twin::start(Provider::OpenAi, &case.root.join("twins"), scripts(&case)).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &workflow(
            model(Provider::OpenAi),
            r#"check [shape=parallelogram, script="cat notes.txt results.json AGENTS.md protected.txt tool-hooks.log"]
    report -> check -> exit"#,
        ),
        Some(WORKFLOW_TOML),
    );
    let script = ship_script(&case);
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
    assert_agent_phases(&case, &twin, &finished);

    // The human decision routed the run into the fan-out.
    let receipt = finished.receipt();
    assert_eq!(receipt["errors"], json!([]), "{receipt}");
    assert_eq!(receipt["questions"][0]["node"], "gate");
    assert_eq!(receipt["questions"][0]["reply"]["choice"], "Y");
    let nodes: Vec<String> = finished
        .finished_nodes()
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert!(
        nodes.contains(&"jobs".to_owned()) && !nodes.contains(&"hold".to_owned()),
        "{nodes:?}"
    );
    assert!(
        nodes.contains(&"initialize".to_owned()) && nodes.contains(&"memory".to_owned()),
        "the preparation steps ran as stages: {nodes:?}"
    );

    // The bounded fan-out: two branches, each its own child invocation, their
    // results consumed by the report command in item order.
    let results = results(&case);
    assert_eq!(results.len(), 2, "{results:?}");
    for (index, (envelope, item)) in results.iter().zip(["alpha", "beta"]).enumerate() {
        assert_eq!(envelope["index"], json!(index), "{envelope}");
        assert_eq!(envelope["item_label"], json!(item), "{envelope}");
        assert_eq!(envelope["status"], json!("succeeded"), "{envelope}");
        assert_eq!(
            envelope["context_updates"]["output.finder"]["found"],
            json!(item)
        );
    }
    let tags = finished.echoed_tags();
    let branch_tags: Vec<&String> = tags
        .iter()
        .filter(|(tag, _)| tag.contains("/job#"))
        .map(|(tag, _)| tag)
        .collect();
    assert!(
        branch_tags.iter().all(|tag| tag.starts_with("invocation-")),
        "branch output is attributed to its invocation: {tags:?}"
    );

    // Status, context and branch results through the public inspection.
    let document = finished.inspect();
    assert_eq!(document["status"], json!("success"));
    assert_eq!(document["complete"], json!(true));
    let context = finished.final_context();
    assert_eq!(context["human.gate.selected"], json!("Y"));
    assert_eq!(context["parallel.branch_count"], json!(2));
    assert_eq!(context["response.plan"], json!("PLAN: one line, signed"));
    assert_eq!(context["response.write"], json!("WROTE the note"));
    assert!(
        !context.contains_key("output.finder"),
        "branch changes never merge into the parent: {context:?}"
    );
    assert_eq!(
        document["invocations"].as_array().map(Vec::len),
        Some(3),
        "the root and two branch children"
    );

    // The workspace the run reported is accessible after teardown, with
    // every file the run produced; the run-end hooks ran in Fabro's order.
    let reported = finished.reported_workspaces();
    assert_eq!(reported, [case.workspace()], "{}", finished.stderr);
    assert!(reported[0].join("results.json").exists());
    assert_eq!(
        read(&case.workspace().join("run-end.log")),
        "run_complete\nsandbox_cleanup\n"
    );
    assert_run_level_notes(&finished, &["run_finished", "scope_released"]).await;
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// The run-level hook reports are durable: `replay_run` carries one
/// `host_note {kind: "hook"}` per run-level point that ran a hook, with no
/// subject (no firing owns it) and the root execution named, in the order
/// the points ran; `petri inspect` lists the same reports under `notes`.
async fn assert_run_level_notes(finished: &Finished, points: &[&str]) {
    let events = replay_run_dir(&finished.run_dir)
        .await
        .expect("the run replays");
    let reports: Vec<&Value> = events
        .iter()
        .filter(|event| event.subject.is_none())
        .filter_map(|event| match event.note() {
            Some(note) if note.kind == "hook" => {
                assert_eq!(
                    event.context.execution.map(ExecutionId::raw),
                    Some(0),
                    "{event:?}"
                );
                Some(&note.payload)
            }
            _ => None,
        })
        .collect();
    let replayed: Vec<&str> = reports
        .iter()
        .filter_map(|report| report["point"].as_str())
        .collect();
    assert_eq!(replayed, points, "{reports:#?}");
    for report in &reports {
        assert_eq!(
            report["hooks"].as_array().map(Vec::len),
            Some(1),
            "{report}"
        );
        assert_eq!(report["hooks"][0]["state"], json!("executed"), "{report}");
    }
    let document = finished.inspect();
    let notes = document["notes"]
        .as_array()
        .expect("inspect lists the run-level notes");
    let inspected: Vec<&str> = notes
        .iter()
        .map(|note| {
            assert_eq!(note["kind"], json!("hook"), "{note}");
            assert_eq!(note["execution"], json!(0), "{note}");
            note["payload"]["point"].as_str().expect("a point")
        })
        .collect();
    assert_eq!(inspected, points, "{notes:#?}");
}

/// The same workflow with a failing final check: the run fails, `run_failed`
/// (not `run_complete`) runs before `sandbox_cleanup`, and everything the
/// run produced stays accessible.
#[tokio::test]
async fn the_milestone_workflow_reports_a_failure_and_keeps_its_work() {
    let mut case = Case::new("milestone-failure");
    let twin = Twin::start(Provider::OpenAi, &case.root.join("twins"), scripts(&case)).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &workflow(
            model(Provider::OpenAi),
            r#"check [shape=parallelogram, script="echo 'the release note is unsigned' >&2; exit 3", on_failure="exit"]
    report -> check -> exit"#,
        ),
        Some(WORKFLOW_TOML),
    );
    let script = ship_script(&case);
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
    assert_agent_phases(&case, &twin, &finished);
    assert_eq!(results(&case).len(), 2);
    let document = finished.inspect();
    assert_eq!(document["status"], json!("failed"));
    let nodes = finished.finished_nodes();
    assert!(
        nodes
            .iter()
            .any(|(status, node)| node == "check" && status == "failure"),
        "{nodes:?}"
    );
    assert!(
        finished
            .echoed()
            .iter()
            .any(|(node, line)| node == "check" && line == "the release note is unsigned"),
        "{}",
        finished.stderr
    );
    assert_eq!(
        read(&case.workspace().join("run-end.log")),
        "run_failed\nsandbox_cleanup\n"
    );
    assert_run_level_notes(&finished, &["run_finished", "scope_released"]).await;
    assert_eq!(finished.reported_workspaces(), [case.workspace()]);
    finished.assert_no_leaked_processes().await;
    twin.stop();
}

/// The same workflow interrupted while its last stage runs: the run is
/// cancelled, the running work stops, neither run-end outcome hook fires
/// (Fabro fires none on a cancelled run) while `sandbox_cleanup` still does,
/// and the work done so far stays accessible.
#[tokio::test]
async fn the_milestone_workflow_is_cancelled_and_keeps_its_work() {
    let mut case = Case::new("milestone-cancel");
    let twin = Twin::start(Provider::OpenAi, &case.root.join("twins"), scripts(&case)).await;
    case.redirect(&twin);
    let workflow = case.workflow(
        &workflow(
            model(Provider::OpenAi),
            r#"slow [shape=parallelogram, script="echo waiting > waiting.txt; sleep 60; echo finished > finished.txt"]
    report -> slow -> exit"#,
        ),
        Some(WORKFLOW_TOML),
    );
    let script = ship_script(&case);
    let finished = case
        .run_with(
            &workflow,
            &["--interview-script", script.to_str().expect("utf-8")],
            Launch {
                interrupt_when: Some(case.workspace().join("waiting.txt")),
                ..Launch::default()
            },
        )
        .await;
    finished.assert_code(1);
    assert_eq!(
        finished.status_line(),
        Some("cancelled"),
        "{}",
        finished.stderr
    );
    assert_agent_phases(&case, &twin, &finished);
    assert_eq!(results(&case).len(), 2);
    assert!(
        !case.workspace().join("finished.txt").exists(),
        "the cancelled stage never finished"
    );
    assert_eq!(
        read(&case.workspace().join("run-end.log")),
        "sandbox_cleanup\n",
        "no run_complete or run_failed on a cancelled run"
    );
    assert_run_level_notes(&finished, &["scope_released"]).await;
    let document = finished.inspect();
    assert_eq!(document["status"], json!("cancelled"));
    assert_eq!(finished.reported_workspaces(), [case.workspace()]);
    finished.assert_no_leaked_processes().await;
    twin.stop();
}
