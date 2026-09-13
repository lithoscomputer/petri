//! The differential comparison matrix (black box phase 5): every scenario
//! runs through the shipped `petri` binary and through the pinned Fabro
//! binary with the same bundle, inputs, twins and interview script, each
//! engine in its own workspace, server, home and twin namespace.
//!
//! Order of checks in every cell:
//!
//! 1. Petri runs; its projection must pass the scenario's independent
//!    expectation.
//! 2. The pinned Fabro runs (when `scripts/fabro-provision.sh` built it); its
//!    projection must pass the same independent expectation. A violation is a
//!    baseline defect: recorded in the evidence and reported, never permission
//!    for Petri to match it.
//! 3. The live Fabro projection must equal the committed reference
//!    (`fabro-reference/reference.json`); `PETRI_FABRO_REFERENCE_RECORD=1`
//!    refreshes it so the change is a reviewable diff.
//! 4. The two projections are compared under `support::fabro::compare`; a
//!    difference no committed decision accepts fails the cell.
//!
//! Without the Fabro binary the cell compares Petri against the committed
//! reference instead and says so in its evidence.
//! `PETRI_REQUIRE_FABRO_BINARY=1` makes a missing binary a failure (CI's
//! compatibility job sets it).
//!
//! Scenarios are declared in code here until task 17's scenario schema
//! lands; each declaration carries the same fields the schema will (bundle,
//! inputs, rules, script, expectation).

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::fabro::compare::{self, Projection, Rules};
use support::fabro::evidence::{self, Record};
use support::fabro::fabro_adapter::{
    FabroBinary, FabroLaunch, FabroServer, RUN_DEADLINE, copy_tree, init_repository, repo_root,
};
use support::fabro::launch::Case;
use support::fabro::twins::{Provider, Twin};

/// Where the tracked scenarios live.
fn scenarios_dir() -> PathBuf {
    repo_root().join("crates/fabro/acceptance/scenarios")
}

/// One independent assertion on a projection: name, passed, detail.
type Check = (String, bool, Value);

/// A twin's scripts for one namespace.
type Scripts = fn(&str) -> Vec<Value>;

/// An engine-specific request matcher over raw request bodies.
type Probe = fn(&[Value]) -> Vec<Check>;

fn check(name: &str, passed: bool, detail: impl Into<Value>) -> Check {
    (name.to_owned(), passed, detail.into())
}

/// One cell of the matrix: a scenario as both engines run it.
struct Cell {
    scenario:    &'static str,
    /// The workflow file inside the bundle.
    workflow:    &'static str,
    /// The bundle's files, relative to the scenario directory: what both
    /// engines run, and what the bundle digest covers.
    bundle:      Vec<&'static str>,
    /// Concrete inputs; `{bundle}` is replaced by the staged bundle's path.
    inputs:      Vec<(&'static str, String)>,
    rules:       Rules,
    /// The shared interview script, as `cli::answer` entries.
    script:      Option<Vec<Value>>,
    /// Provider twins both engines need, with their scenario scripts built
    /// per namespace.
    twins:       Vec<(Provider, Scripts)>,
    /// Skill directories seeded under each engine's `$FABRO_HOME/skills`
    /// (a fixture path relative to the repository root).
    home_skills: Option<&'static str>,
    /// The independent expectation, asserted on each engine's projection.
    expect:      fn(&Projection) -> Vec<Check>,
    /// An engine-specific request matcher over the raw request bodies the
    /// workflow sent (platform requests left out): the same item
    /// assignments, tool actions and call obligations asserted on each
    /// engine where prompt assembly differs.
    probe:       Option<Probe>,
}

/// A staged bundle for one engine.
struct Staged {
    dir: PathBuf,
}

fn stage(scenario: &str, dest: &Path, repository: bool) -> Staged {
    let source = scenarios_dir().join(scenario);
    assert!(
        source.is_dir(),
        "scenario `{scenario}` is not at {}",
        source.display()
    );
    copy_tree(&source, dest).expect("stage the bundle");
    // The capture beside the bundle is not part of it.
    let _ = fs::remove_dir_all(dest.join("fabro-reference"));
    if repository {
        init_repository(dest);
    }
    Staged {
        dir: dest.to_path_buf(),
    }
}

fn resolve_inputs(inputs: &[(&str, String)], bundle: &Path) -> Vec<(String, String)> {
    inputs
        .iter()
        .map(|(key, value)| {
            (
                (*key).to_owned(),
                value.replace("{bundle}", &bundle.to_string_lossy()),
            )
        })
        .collect()
}

async fn start_twins(cell: &Cell, dir: &Path, namespace: &str) -> Vec<Twin> {
    let mut twins = Vec::new();
    for (provider, scripts) in &cell.twins {
        twins.push(Twin::start(*provider, dir, scripts(namespace)).await);
    }
    twins
}

fn twin_pins(twins: &[Twin]) -> Value {
    json!(
        twins
            .iter()
            .map(|twin| json!({ "provider": twin.provider.id(), "base_url": twin.base_url }))
            .collect::<Vec<_>>()
    )
}

/// Apply the expectation and the request probe, record every check, and
/// return the failures as (assertion name, detail).
fn expect(
    record: &mut Record,
    projection: &Projection,
    bodies: &[Value],
    cell: &Cell,
) -> Vec<(String, String)> {
    let mut failures = Vec::new();
    let mut checks = (cell.expect)(projection);
    if let Some(probe) = cell.probe {
        checks.extend(probe(bodies));
    }
    for (name, passed, detail) in checks {
        record.assert(&name, passed, detail.clone());
        if !passed {
            failures.push((name, detail.to_string()));
        }
    }
    failures
}

/// Render failures as `name: detail` lines.
fn render_failures(failures: &[(String, String)]) -> Vec<String> {
    failures
        .iter()
        .map(|(name, detail)| format!("{name}: {detail}"))
        .collect()
}

/// Seed a fixture's skill directories under `home/.fabro/skills`.
fn seed_home_skills(cell: &Cell, home: &Path) {
    if let Some(fixture) = cell.home_skills {
        let dest = home.join(".fabro").join("skills");
        copy_tree(&repo_root().join(fixture), &dest).expect("seed the home skills");
    }
}

/// Run one cell end to end.
#[expect(
    clippy::print_stderr,
    reason = "known baseline defects are reported on the test's stderr"
)]
async fn run_cell(cell: Cell) {
    let scenario = cell.scenario;
    let fabro = FabroBinary::provisioned();
    let source_digest = evidence::bundle_digest(&scenarios_dir().join(scenario), &cell.bundle);
    let decisions = compare::Decisions::load();

    // Petri.
    let mut case = Case::new(&format!("diff-{scenario}"));
    seed_home_skills(&cell, &case.root.join("home"));
    let petri_bundle = stage(scenario, &case.root.join("petri-bundle"), false);
    let petri_twins = start_twins(&cell, &case.root.join("twins-petri"), &case.credential).await;
    for twin in &petri_twins {
        case.redirect(twin);
    }
    let script_path = cell
        .script
        .as_ref()
        .map(|entries| support::fabro::interview::write(&case.root, scenario, entries));
    let inputs = resolve_inputs(&cell.inputs, &petri_bundle.dir);
    // The launch-level model default, the same on both engines: Fabro gets
    // `--provider` below, and a bundle that names no model (the pinned
    // interview workflow) runs on the provider's default model.
    let launch_provider = cell
        .twins
        .first()
        .map_or("openai", |(provider, _)| provider.id());
    let mut args: Vec<String> = vec!["--provider".into(), launch_provider.into()];
    for (key, value) in &inputs {
        args.push("--input".into());
        args.push(format!("{key}={value}"));
    }
    if let Some(path) = &script_path {
        args.push("--interview-script".into());
        args.push(path.to_string_lossy().into_owned());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let finished = case
        .run(&petri_bundle.dir.join(cell.workflow), &arg_refs)
        .await;
    let petri_twin_refs: Vec<&Twin> = petri_twins.iter().collect();
    let mut petri_record = Record::new(scenario, "petri", fabro.as_ref());
    petri_record.set("bundle_digest", json!(source_digest));
    petri_record.set(
        "launch",
        json!({
            "workflow": cell.workflow,
            "inputs": inputs,
            "args": args,
            "run_dir": finished.run_dir,
            "twins": twin_pins(&petri_twins),
            "credential_namespace": case.credential,
        }),
    );
    petri_record.set(
        "process",
        json!({
            "exit_code": finished.code,
            "timed_out": finished.timed_out,
            "status_line": finished.status_line(),
        }),
    );
    let petri_projection = if finished.timed_out || finished.code.is_none() {
        None
    } else {
        Some(compare::project_petri(
            &finished,
            &case.workspace(),
            &petri_twin_refs,
            &case.credential,
            &cell.rules,
        ))
    };
    let petri_projection = petri_projection.unwrap_or_else(|| {
        petri_record.set("stderr", json!(finished.stderr));
        petri_record.write();
        panic!(
            "petri did not finish {scenario}: code {:?}, timed out {}\n{}",
            finished.code, finished.timed_out, finished.stderr
        )
    });
    petri_record.set("projection", compare::to_value(&petri_projection));
    petri_record.set(
        "raw",
        json!({
            "run_dir": finished.run_dir,
            "inspect": "petri inspect --run-dir <run_dir> --json",
            "stderr_bytes": finished.stderr.len(),
        }),
    );
    let petri_bodies = compare::request_bodies(&petri_twin_refs, &case.credential);
    let petri_failures = expect(&mut petri_record, &petri_projection, &petri_bodies, &cell);
    petri_record.set("cleanup", json!({ "leaked_processes_checked": true }));
    let petri_record_path = petri_record.write();
    fs::write(
        petri_record_path.with_file_name("petri-stderr.txt"),
        &finished.stderr,
    )
    .expect("write stderr");
    finished.assert_no_leaked_processes().await;
    assert!(
        petri_failures.is_empty(),
        "{scenario}: petri violates the independent expectation:\n{}",
        render_failures(&petri_failures).join("\n")
    );

    // Fabro.
    let fabro_source;
    let fabro_projection = if let Some(binary) = &fabro {
        let fabro_root = case.root.join("fabro");
        fs::create_dir_all(&fabro_root).expect("fabro root");
        let namespace = format!("{}-fabro", case.credential);
        let fabro_twins = start_twins(&cell, &fabro_root.join("twins"), &namespace).await;
        let fabro_twin_refs: Vec<&Twin> = fabro_twins.iter().collect();
        seed_home_skills(&cell, &fabro_root.join("server").join("home"));
        let server = FabroServer::start(
            binary.clone(),
            &fabro_root.join("server"),
            &namespace,
            &fabro_twin_refs,
        )
        .await;
        let fabro_bundle = stage(scenario, &fabro_root.join("bundle"), true);
        let inputs = resolve_inputs(&cell.inputs, &fabro_bundle.dir);
        let input_refs: Vec<(&str, String)> = inputs
            .iter()
            .map(|(key, value)| (key.as_str(), value.clone()))
            .collect();
        let launch = FabroLaunch {
            dir:      &fabro_bundle.dir,
            workflow: cell.workflow,
            inputs:   &input_refs,
            provider: launch_provider,
            script:   script_path.as_deref(),
            deadline: RUN_DEADLINE,
        };
        let run = server.run(&launch).await;
        let projection = compare::project_fabro(&run, &fabro_twin_refs, &namespace, &cell.rules);
        let mut record = Record::new(scenario, "fabro", Some(binary));
        record.set("bundle_digest", json!(source_digest));
        record.set(
            "launch",
            json!({
                "workflow": cell.workflow,
                "inputs": inputs,
                "server": server.url,
                "run_id": run.run_id,
                "twins": twin_pins(&fabro_twins),
                "credential_namespace": namespace,
                "validation": run.validation,
            }),
        );
        record.set(
            "process",
            json!({
                "status": run.status,
                "timed_out": run.timed_out,
                "launch_stdout": run.launch_stdout,
                "launch_stderr": run.launch_stderr,
            }),
        );
        record.set("projection", compare::to_value(&projection));
        record.set("receipt", run.receipt.clone());
        let raw_events = evidence::keep_raw(
            scenario,
            "fabro",
            "events.jsonl",
            &server.root.join("events.jsonl"),
        );
        let raw_state = evidence::keep_raw(
            scenario,
            "fabro",
            "state.json",
            &server.root.join("state.json"),
        );
        let raw_dump = evidence::keep_raw(scenario, "fabro", "dump", &run.dump_dir);
        record.set(
            "raw",
            json!({ "events": raw_events, "state": raw_state, "dump": raw_dump }),
        );
        let bodies = compare::request_bodies(&fabro_twin_refs, &namespace);
        let failures = expect(&mut record, &projection, &bodies, &cell);
        // The decision records name the assertions the pinned Fabro is
        // known to fail here; the coverage report reads the same list.
        let known_defects = decisions.known_defects(scenario);
        let mut known: Vec<String> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        for (name, detail) in &failures {
            match known_defects.iter().find(|(defect, _)| defect == name) {
                Some((_, decision)) => {
                    known.push(format!("{name}: {detail} (known defect {decision})"));
                }
                None => unknown.push(format!("{name}: {detail}")),
            }
        }
        record.set(
            "baseline_defects",
            json!({ "known": known, "new": unknown }),
        );
        server.stop().await;
        record.set("cleanup", json!({ "server_stopped": true }));
        record.write();
        assert!(
            run.receipt["errors"].as_array().is_some_and(Vec::is_empty),
            "{scenario}: the Fabro interview receipt has errors: {}",
            run.receipt["errors"]
        );
        assert!(
            unknown.is_empty(),
            "{scenario}: the pinned Fabro violates the independent expectation (a baseline defect; \
             record its assertion name under `known_defects` in a decision record with the \
             evidence):\n{}",
            unknown.join("\n")
        );
        if !known.is_empty() {
            eprintln!(
                "{scenario}: known baseline defects of the pinned Fabro:\n{}",
                known.join("\n")
            );
        }
        if let Err(message) = evidence::check_or_record_reference(
            scenario,
            binary,
            &source_digest,
            &projection,
            &json!(
                fabro_twins
                    .iter()
                    .map(|twin| twin.provider.id())
                    .collect::<Vec<_>>()
            ),
        ) {
            panic!("{scenario}: {message}");
        }
        fabro_source = "live pinned binary".to_owned();
        projection
    } else {
        let Some((document, projection)) = evidence::load_reference(scenario) else {
            panic!(
                "{scenario}: no pinned Fabro binary and no committed reference at {}",
                evidence::reference_path(scenario).display()
            );
        };
        assert_eq!(
            document["bundle_digest"].as_str(),
            Some(source_digest.as_str()),
            "{scenario}: the committed reference was captured from a different bundle; rerun with \
             the binary and PETRI_FABRO_REFERENCE_RECORD=1"
        );
        fabro_source = format!(
            "committed reference {}",
            evidence::reference_path(scenario).display()
        );
        projection
    };

    // Compare.
    let differences = compare::compare(&petri_projection, &fabro_projection, scenario, &decisions);
    let comparison = evidence::write_comparison(scenario, &differences, &fabro_source);
    let unresolved = compare::unresolved(&differences);
    assert!(
        unresolved.is_empty(),
        "{scenario}: {} unresolved difference(s) between petri and fabro ({fabro_source}); see {}:\n{}",
        unresolved.len(),
        comparison.display(),
        compare::render(&unresolved)
    );
}

// ---------------------------------------------------------------------------
// Scenario: parallel-results (command-only, the first regression of phase 2)
// ---------------------------------------------------------------------------

/// The Conveyor helper's report, byte-exact, from the phase 2 capture.
fn parallel_results_report() -> String {
    fs::read_to_string(scenarios_dir().join("parallel-results/fabro-reference/raw/report.md"))
        .expect("the reference report is tracked")
}

fn expect_parallel_results(p: &Projection) -> Vec<Check> {
    let mut checks = vec![check(
        "status is succeeded",
        p.status == "succeeded",
        json!(p.status),
    )];
    let context = &p.context;
    for (key, want) in [
        ("candidate_count", json!(2)),
        ("run_verify", json!(true)),
        ("verified_count", json!(2)),
        ("reported", json!(2)),
    ] {
        let got = context.get(key).cloned().unwrap_or(Value::Null);
        checks.push(check(
            &format!("context.{key} == {want}"),
            got == want,
            json!({ "got": got }),
        ));
    }
    let branch_count = context
        .get("parallel.branch_count")
        .or_else(|| p.bookkeeping.get("parallel.branch_count"))
        .cloned();
    checks.push(check(
        "parallel.branch_count == 2",
        branch_count == Some(json!(2)),
        json!({ "got": branch_count }),
    ));
    for key in ["output.finder", "output.verifier"] {
        checks.push(check(
            &format!("{key} never reaches the parent context"),
            !context.contains_key(key) && !p.bookkeeping.contains_key(key),
            json!(context.get(key)),
        ));
    }
    let report = p
        .artifacts
        .get(".fabro/workflows/code-review/runtime/report.md")
        .cloned()
        .flatten();
    checks.push(check(
        "the report is byte-identical to the reference report",
        report.as_deref() == Some(parallel_results_report().as_str()),
        json!({ "got": report }),
    ));
    let forks: Vec<&str> = p.forks.iter().map(|f| f.node.as_str()).collect();
    checks.push(check(
        "two forks: find, verify",
        forks == ["find", "verify"],
        json!(forks),
    ));
    for (fork, ids, key) in [
        ("find", ["finder_a", "finder_b"], "output.finder"),
        ("verify", ["verifier_a", "verifier_b"], "output.verifier"),
    ] {
        let Some(f) = p.forks.iter().find(|f| f.node == fork) else {
            checks.push(check(&format!("fork {fork} exists"), false, Value::Null));
            continue;
        };
        let got_ids: Vec<&str> = f.branches.iter().map(|b| b.id.as_str()).collect();
        checks.push(check(
            &format!("{fork}: two envelopes in edge order"),
            got_ids == ids,
            json!(got_ids),
        ));
        for (index, branch) in f.branches.iter().enumerate() {
            checks.push(check(
                &format!("{fork}[{index}]: index is the edge position"),
                branch.index == Some(index as u64),
                json!(branch.index),
            ));
            checks.push(check(
                &format!("{fork}[{index}]: no item_label on a static branch"),
                branch.item_label.is_none(),
                json!(branch.item_label),
            ));
            checks.push(check(
                &format!("{fork}[{index}]: status succeeded"),
                branch.status == "succeeded",
                json!(branch.status),
            ));
            let own = branch
                .context_updates
                .get(key)
                .cloned()
                .unwrap_or(Value::Null);
            checks.push(check(
                &format!("{fork}[{index}]: carries its own {key}"),
                own.is_object(),
                own,
            ));
        }
    }
    checks
}

#[tokio::test]
async fn parallel_results_matches_the_pinned_fabro() {
    run_cell(Cell {
        scenario:    "parallel-results",
        workflow:    "workflow.fabro",
        bundle:      vec!["workflow.fabro", "helper/code_review.py"],
        inputs:      vec![
            ("helper", "{bundle}/helper/code_review.py".to_owned()),
            ("level", "high".to_owned()),
            ("target", "review-fixture".to_owned()),
        ],
        rules:       compare::rules(
            // Named bookkeeping of each engine, kept beside the compared
            // context. Fabro: its engine-internal keys. Petri: its
            // format-internal keys. Both: the last command's output and the
            // last join's results, which the workflow-owned values below
            // are derived from.
            &[
                "internal.",
                "graph.",
                "current.",
                "current_node",
                "response.",
                "thread.",
                "human.gate.",
                "outcome",
                "failure_class",
                "failure_signature",
                "preferred_label",
                "last_stage",
                "last_response",
                "command.output",
                "parallel.results",
                "parallel.branch_count",
            ],
            &[".fabro/workflows/code-review/runtime/report.md"],
        ),
        script:      None,
        twins:       Vec::new(),
        expect:      expect_parallel_results,
        home_skills: None,
        probe:       None,
    })
    .await;
}

// ---------------------------------------------------------------------------
// Scenario: interview (the required bundle, scripted-choices path)
// ---------------------------------------------------------------------------

/// The twin's one script: the `summarize` prompt node's single tool-free
/// call. Both engines put the node's prompt text in the request.
fn interview_scripts(namespace: &str) -> Vec<Value> {
    use support::fabro::twins::{model, scenario, text};
    let provider = Provider::OpenAi;
    vec![scenario(
        provider,
        namespace,
        "summarize",
        model(provider),
        "Summarize the full human interview",
        text("SUMMARY: easy to follow; continue; risks; blockers; ship on Friday."),
    )]
}

/// Engine bookkeeping named for every cell: Fabro's engine-internal
/// prefixes and Petri's format-internal keys, plus the last command output
/// and the last join's results.
const COMMON_BOOKKEEPING: &[&str] = &[
    "internal.",
    "graph.",
    "current.",
    "current_node",
    "response.",
    "thread.",
    "outcome",
    "failure_class",
    "failure_signature",
    "preferred_label",
    "last_stage",
    "last_response",
    "command.output",
    "parallel.results",
    "parallel.branch_count",
];

fn interview_entry(id: &str, node: &str, kind: &str, action: &Value) -> Value {
    json!({
        "id": id,
        "match": { "node": node, "kind": kind },
        "count": 1,
        "action": action,
    })
}

fn expect_interview(p: &Projection) -> Vec<Check> {
    let mut checks = vec![check(
        "status is succeeded",
        p.status == "succeeded",
        json!(p.status),
    )];
    let nodes: Vec<&str> = p.interviews.iter().map(|i| i.node.as_str()).collect();
    checks.push(check(
        "five gates asked in graph order",
        nodes
            == [
                "yes_no",
                "confirmation",
                "multiple_choice",
                "multi_select",
                "freeform",
            ],
        json!(nodes),
    ));
    let kinds: Vec<&str> = p.interviews.iter().map(|i| i.kind.as_str()).collect();
    checks.push(check(
        "each gate has its declared kind",
        kinds
            == [
                "yes_no",
                "confirmation",
                "multiple_choice",
                "multi_select",
                "freeform",
            ],
        json!(kinds),
    ));
    let replies: Vec<Value> = p.interviews.iter().map(|i| i.reply.clone()).collect();
    checks.push(check(
        "the scripted answers were delivered in order",
        replies
            == vec![
                json!({ "kind": "answered", "choice": "Y" }),
                json!({ "kind": "answered", "choice": "Y" }),
                json!({ "kind": "answered", "choice": "R" }),
                json!({ "kind": "answered", "choice": "B", "choices": ["B"] }),
                json!({ "kind": "answered", "text": "ship on Friday" }),
            ],
        json!(replies),
    ));
    checks.push(check(
        "every answer was delivered",
        p.interviews.iter().all(|i| i.delivery == "delivered"),
        json!(p.interviews.iter().map(|i| &i.delivery).collect::<Vec<_>>()),
    ));
    checks.push(check(
        "the freeform answer reached the context",
        p.context.get("human.gate.freeform.answer") == Some(&json!("ship on Friday")),
        json!(p.context.get("human.gate.freeform.answer")),
    ));
    for node in ["yes_no", "confirmation", "multiple_choice", "multi_select"] {
        let key = format!("human.gate.{node}.answer");
        checks.push(check(
            &format!("{key} is recorded"),
            p.context.get(&key).is_some_and(|v| !v.is_null()),
            json!(p.context.get(&key)),
        ));
    }
    let scenarios: Vec<Option<&str>> = p.requests.iter().map(|r| r.scenario.as_deref()).collect();
    checks.push(check(
        "exactly one model call, the summary",
        scenarios == [Some("summarize")],
        json!(scenarios),
    ));
    checks.push(check(
        "the summary went to gpt-5.6-sol on openai",
        p.requests
            .iter()
            .all(|r| r.provider == "openai" && r.model == "gpt-5.6-sol"),
        json!(p.requests),
    ));
    checks
}

#[tokio::test]
async fn interview_scripted_choices_match_the_pinned_fabro() {
    run_cell(Cell {
        scenario:    "interview",
        workflow:    ".fabro/workflows/interview/workflow.fabro",
        bundle:      vec![
            ".fabro/project.toml",
            ".fabro/Dockerfile",
            ".fabro/workflows/interview/workflow.fabro",
            ".fabro/workflows/interview/workflow.toml",
        ],
        inputs:      Vec::new(),
        rules:       compare::rules(COMMON_BOOKKEEPING, &[]),
        script:      Some(vec![
            interview_entry(
                "easy",
                "yes_no",
                "yes_no",
                &json!({ "kind": "choice", "value": "Y" }),
            ),
            interview_entry(
                "continue",
                "confirmation",
                "confirmation",
                &json!({ "kind": "choice", "value": "Y" }),
            ),
            interview_entry(
                "risks",
                "multiple_choice",
                "multiple_choice",
                &json!({ "kind": "choice", "value": "R" }),
            ),
            interview_entry(
                "blockers",
                "multi_select",
                "multi_select",
                &json!({ "kind": "choices", "values": ["B"] }),
            ),
            interview_entry(
                "nuance",
                "freeform",
                "freeform",
                &json!({ "kind": "text", "value": "ship on Friday" }),
            ),
        ]),
        twins:       vec![(Provider::OpenAi, interview_scripts)],
        expect:      expect_interview,
        home_skills: None,
        probe:       None,
    })
    .await;
}

// ---------------------------------------------------------------------------
// Scenario: edit-and-verify (a native agent with a real shell tool, a gate)
// ---------------------------------------------------------------------------

fn edit_and_verify_scripts(namespace: &str) -> Vec<Value> {
    use support::fabro::twins::{model, scenario, shell_tool, text, tool_call};
    let provider = Provider::OpenAi;
    let shell = shell_tool(provider);
    let model = model(provider);
    vec![
        scenario(
            provider,
            namespace,
            "append",
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
            "read-back",
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
            "answer",
            model,
            "READ_DONE",
            text("APPENDED: notes.txt now ends with reviewed."),
        ),
    ]
}

fn expect_edit_and_verify(p: &Projection) -> Vec<Check> {
    let mut checks = vec![check(
        "status is succeeded",
        p.status == "succeeded",
        json!(p.status),
    )];
    let notes = p.artifacts.get("notes.txt").cloned().flatten();
    checks.push(check(
        "notes.txt holds the draft and the appended word",
        notes.as_deref() == Some("draft\nreviewed\n"),
        json!(notes),
    ));
    let decision = p.artifacts.get("decision.txt").cloned().flatten();
    checks.push(check(
        "decision.txt says shipped",
        decision.as_deref() == Some("shipped\n"),
        json!(decision),
    ));
    let path: Vec<&str> = p.path.iter().map(|s| s.node.as_str()).collect();
    checks.push(check(
        "the ship branch ran and hold did not",
        path.contains(&"ship") && !path.contains(&"hold"),
        json!(path),
    ));
    checks.push(check(
        "one yes_no gate answered Y",
        p.interviews.len() == 1
            && p.interviews[0].node == "gate"
            && p.interviews[0].kind == "yes_no"
            && p.interviews[0].reply == json!({ "kind": "answered", "choice": "Y" })
            && p.interviews[0].delivery == "delivered",
        json!(p.interviews),
    ));
    let scenarios: Vec<Option<&str>> = p.requests.iter().map(|r| r.scenario.as_deref()).collect();
    checks.push(check(
        "three model calls: append, read-back, answer",
        scenarios == [Some("append"), Some("read-back"), Some("answer")],
        json!(scenarios),
    ));
    checks.push(check(
        "every call asked gpt-5.6-sol on openai at high effort",
        p.requests.iter().all(|r| {
            r.provider == "openai"
                && r.model == "gpt-5.6-sol"
                && r.effort.as_deref() == Some("high")
        }),
        json!(p.requests),
    ));
    checks
}

#[tokio::test]
async fn edit_and_verify_matches_the_pinned_fabro() {
    run_cell(Cell {
        scenario:    "edit-and-verify",
        workflow:    "workflow.fabro",
        bundle:      vec!["workflow.fabro"],
        inputs:      Vec::new(),
        rules:       compare::rules(COMMON_BOOKKEEPING, &["notes.txt", "decision.txt"]),
        script:      Some(vec![interview_entry(
            "ship",
            "gate",
            "yes_no",
            &json!({ "kind": "choice", "value": "Y" }),
        )]),
        twins:       vec![(Provider::OpenAi, edit_and_verify_scripts)],
        expect:      expect_edit_and_verify,
        home_skills: None,
        probe:       None,
    })
    .await;
}

// ---------------------------------------------------------------------------
// Scenario: fallback-failover (task 12's capture: the primary fails after a
// completed tool effect and the chain falls back to Anthropic)
// ---------------------------------------------------------------------------

/// The primary answers the prompt with the append, then fails every request
/// that carries the tool's result with a 503, however often the client
/// retries.
fn fallback_openai_scripts(namespace: &str) -> Vec<Value> {
    use support::fabro::failures::{error, repeated};
    use support::fabro::twins::{model, scenario, shell_tool, tool_call};
    let provider = Provider::OpenAi;
    vec![
        scenario(
            provider,
            namespace,
            "append",
            model(provider),
            "Append the word reviewed",
            tool_call(
                "append",
                shell_tool(provider),
                json!({ "command": "printf 'reviewed\\n' >> notes.txt && echo APPEND_DONE" }),
            ),
        ),
        repeated(
            scenario(
                provider,
                namespace,
                "primary-down",
                model(provider),
                "APPEND_DONE",
                error(
                    503,
                    "server_error",
                    "service_unavailable",
                    "the primary is overloaded",
                ),
            ),
            12,
        ),
    ]
}

/// The fallback serves both continuations: the bare prompt (Fabro re-runs
/// it) and the tool history (Petri continues it). A request that carries
/// the history contains the prompt too, so the scripts are listed most
/// specific first: the twin takes the first unspent match.
fn fallback_anthropic_scripts(namespace: &str) -> Vec<Value> {
    use support::fabro::twins::{model, scenario, shell_tool, text, tool_call};
    let provider = Provider::Anthropic;
    let shell = shell_tool(provider);
    let model = model(provider);
    vec![
        scenario(
            provider,
            namespace,
            "answer",
            model,
            "READ_DONE",
            text("APPENDED: notes.txt now ends with reviewed."),
        ),
        scenario(
            provider,
            namespace,
            "read-back",
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
            "append",
            model,
            "Append the word reviewed",
            tool_call(
                "append",
                shell,
                json!({ "command": "printf 'reviewed\\n' >> notes.txt && echo APPEND_DONE" }),
            ),
        ),
    ]
}

fn expect_fallback_failover(p: &Projection) -> Vec<Check> {
    let mut checks = vec![check(
        "status is succeeded",
        p.status == "succeeded",
        json!(p.status),
    )];
    let notes = p
        .artifacts
        .get("notes.txt")
        .cloned()
        .flatten()
        .unwrap_or_default();
    checks.push(check(
        "notes.txt starts with the draft and ends with reviewed",
        notes.starts_with("draft\n") && notes.ends_with("reviewed\n"),
        json!(notes),
    ));
    let appends = notes.matches("reviewed\n").count();
    checks.push(check(
        "the append ran exactly once across the failover",
        appends == 1,
        json!({ "appends": appends, "notes": notes }),
    ));
    let openai: Vec<Option<&str>> = p
        .requests
        .iter()
        .filter(|r| r.provider == "openai")
        .map(|r| r.scenario.as_deref())
        .collect();
    checks.push(check(
        "the primary served the append, then failed every retry",
        openai.first() == Some(&Some("append"))
            && openai.len() >= 2
            && openai[1..].iter().all(|s| *s == Some("primary-down")),
        json!(openai),
    ));
    let anthropic: Vec<Option<&str>> = p
        .requests
        .iter()
        .filter(|r| r.provider == "anthropic")
        .map(|r| r.scenario.as_deref())
        .collect();
    checks.push(check(
        "the fallback finished the work on claude-sonnet-5",
        anthropic.last() == Some(&Some("answer"))
            && anthropic.contains(&Some("read-back"))
            && p.requests
                .iter()
                .filter(|r| r.provider == "anthropic")
                .all(|r| r.model == "claude-sonnet-5"),
        json!(anthropic),
    ));
    checks.push(check(
        "the last request of the run went to the fallback",
        p.requests.last().is_some_and(|r| r.provider == "anthropic"),
        json!(p.requests.last()),
    ));
    checks
}

#[tokio::test]
async fn fallback_failover_matches_the_pinned_fabro() {
    run_cell(Cell {
        scenario:    "fallback-failover",
        workflow:    "workflow.fabro",
        bundle:      vec!["workflow.fabro", "workflow.toml"],
        inputs:      Vec::new(),
        rules:       compare::rules(COMMON_BOOKKEEPING, &["notes.txt"]),
        script:      None,
        twins:       vec![
            (Provider::OpenAi, fallback_openai_scripts),
            (Provider::Anthropic, fallback_anthropic_scripts),
        ],
        expect:      expect_fallback_failover,
        home_skills: None,
        probe:       None,
        // Both engines run the failover in Pebble and keep the session, so
        // the append runs once on each; the reference before `05ebd0f`
        // re-ran the prompt from scratch and repeated it (the retired
        // `fallback-repeated-tool-effect` record).
    })
    .await;
}

// ---------------------------------------------------------------------------
// Scenario: skills-precedence (task 14's capture: the three skill
// directories, precedence, the prompt section and the skill tool)
// ---------------------------------------------------------------------------

/// The model calls the skill tool for `greet`, sees the winning copy's
/// content, and answers. Most specific first: the second request carries
/// the prompt too.
fn skills_scripts(namespace: &str) -> Vec<Value> {
    use support::fabro::twins::{model, scenario, text, tool_call};
    let provider = Provider::OpenAi;
    vec![
        scenario(
            provider,
            namespace,
            "greeted",
            model(provider),
            "REPOSITORY GREETING",
            text("GREETED Ada"),
        ),
        scenario(
            provider,
            namespace,
            "greet-call",
            model(provider),
            "Use the greet skill",
            tool_call("skill-1", "use_skill", json!({ "skill_name": "greet" })),
        ),
    ]
}

fn expect_skills(p: &Projection) -> Vec<Check> {
    let scenarios: Vec<Option<&str>> = p.requests.iter().map(|r| r.scenario.as_deref()).collect();
    vec![
        check(
            "status is succeeded",
            p.status == "succeeded",
            json!(p.status),
        ),
        check(
            "two model calls: the skill call, then the answer",
            scenarios == [Some("greet-call"), Some("greeted")],
            json!(scenarios),
        ),
        check(
            "both calls asked gpt-5.6-sol on openai",
            p.requests
                .iter()
                .all(|r| r.provider == "openai" && r.model == "gpt-5.6-sol"),
            json!(p.requests),
        ),
    ]
}

/// The engine-specific matcher: the first request carries the reference
/// prompt section and the reference `use_skill` tool; the second carries
/// the repository's greeting and neither losing copy.
fn probe_skills(bodies: &[Value]) -> Vec<Check> {
    let fixtures = repo_root().join("crates/fabro/acceptance/testdata/skills/expected");
    let section = fs::read_to_string(fixtures.join("prompt-section.use_skill.txt"))
        .expect("the reference prompt section");
    let tool: Value = serde_json::from_str(
        &fs::read_to_string(fixtures.join("tool.use_skill.json")).expect("the reference tool"),
    )
    .expect("tool.use_skill.json is JSON");
    let text = |body: &Value| serde_json::to_string(body).unwrap_or_default();
    let first = bodies.first().cloned().unwrap_or(Value::Null);
    let second = bodies.get(1).cloned().unwrap_or(Value::Null);
    // The section as it appears inside a JSON string.
    let section_in_json = text(&json!(section.trim()));
    let section_in_json = section_in_json.trim_matches('"');
    let offered = first["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|t| t["name"] == "use_skill"))
        .cloned()
        .unwrap_or(Value::Null);
    vec![
        check(
            "the first request carries the reference skills prompt section",
            text(&first).contains(section_in_json),
            json!({ "expected": section.trim(), "instructions": first["instructions"] }),
        ),
        check(
            "the first request offers the reference use_skill tool",
            offered["description"] == tool["description"]
                && offered["parameters"] == tool["parameters"],
            json!({ "offered": offered, "expected": tool }),
        ),
        check(
            "the second request carries the repository's greeting and neither losing copy",
            text(&second).contains("REPOSITORY GREETING")
                && !text(&second).contains("HOME GREETING")
                && !text(&second).contains("PROJECT GREETING"),
            json!({ "second_bytes": text(&second).len() }),
        ),
    ]
}

#[tokio::test]
async fn skills_precedence_matches_the_pinned_fabro() {
    run_cell(Cell {
        scenario:    "skills-precedence",
        workflow:    "workflow.fabro",
        bundle:      vec![
            "workflow.fabro",
            ".fabro/skills/greet/SKILL.md",
            ".fabro/skills/project-only/SKILL.md",
            "skills/greet/SKILL.md",
            "skills/repo-only/SKILL.md",
            "skills/cleanup/SKILL.md",
            "skills/no-frontmatter/SKILL.md",
            "skills/no-name/SKILL.md",
            "skills/unterminated/SKILL.md",
        ],
        inputs:      vec![("fixture", "{bundle}".to_owned())],
        rules:       compare::rules(COMMON_BOOKKEEPING, &[]),
        script:      None,
        twins:       vec![(Provider::OpenAi, skills_scripts)],
        home_skills: Some("crates/fabro/acceptance/testdata/skills/home/skills"),
        expect:      expect_skills,
        probe:       Some(probe_skills),
    })
    .await;
}
