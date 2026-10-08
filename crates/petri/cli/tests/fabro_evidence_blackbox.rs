//! The evidence side of the readiness gate: a scenario run through the
//! shipped binary leaves a machine-readable record with every pin, its
//! launch inputs, the twin scenarios it consumed, its observations, final
//! context, assertions, and cleanup; a failed scenario keeps its whole case
//! directory; the coverage report (`scripts/fabro-coverage-report.py`)
//! counts only passed cells; and a required asset fails instead of skipping
//! when CI asks for it.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{env, fs, panic, process};

use serde_json::{Value, json};
use support::fabro::launch::Case;
use support::fabro::record::{Backend, Recorder, directory, outcomes};
use support::fabro::require;
use support::fabro::twins::{Provider, Twin, model, scenario, text};

const TEST_FILE: &str = "fabro_evidence_blackbox";

fn workspace_root() -> PathBuf {
    require::workspace_root()
}

fn script(name: &str, args: &[&str]) -> Output {
    Command::new("python3")
        .arg(workspace_root().join("scripts").join(name))
        .args(args)
        .env_remove("PETRI_EVIDENCE_DIR")
        .output()
        .expect("python3 runs the script")
}

fn read_json(path: &Path) -> Value {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("JSON")
}

/// A workflow with one command and one native agent answered by the twin.
fn prepare_and_answer(provider: Provider) -> String {
    format!(
        r#"digraph Evidence {{
    graph [backend="api"]
    start [shape=Mdiamond]
    exit [shape=Msquare]
    prepare [shape=parallelogram, script="printf 'draft\n' > notes.txt && echo prepared"]
    agent [prompt="Say EVIDENCE_DONE and nothing else.", model="{model}", provider="{provider}", on_failure="exit"]
    start -> prepare -> agent -> exit
}}"#,
        model = model(provider),
        provider = provider.id(),
    )
}

#[tokio::test]
async fn a_host_scenario_writes_a_complete_evidence_record() {
    let provider = Provider::OpenAi;
    let mut case = Case::new("evidence-host");
    let twin = Twin::start(provider, &case.root.join("twins"), vec![scenario(
        provider,
        &case.credential,
        "answer",
        model(provider),
        "EVIDENCE_DONE",
        text("EVIDENCE_DONE"),
    )])
    .await;
    case.redirect(&twin);
    let workflow = case.workflow(&prepare_and_answer(provider), None);
    let test = format!("{TEST_FILE}::a_host_scenario_writes_a_complete_evidence_record");
    // Into this run's evidence directory, like every scenario: CI keeps it.
    let mut record = Recorder::start("evidence-smoke", Backend::Host, &test);
    record.scenario_meta("evidence", None);
    record.launch(&workflow, &[], &[]);

    let finished = case.run(&workflow, &[]).await;
    record.finished(&finished);
    record.twin(&twin);
    assert!(
        record.check("exit code 0", finished.code == Some(0), &finished.stderr),
        "{}",
        finished.stderr
    );
    assert!(record.check(
        "run: success",
        finished.status_line() == Some("success"),
        &finished.stderr
    ));
    assert!(record.check(
        "the twin's scripted answer was consumed once",
        twin.consumed() == ["answer"] && twin.unmatched() == 0,
        format!("{:?}", twin.consumed())
    ));
    record.artifact(&case.workspace().join("notes.txt"), "the command's file");
    record.decision(
        "none",
        "note",
        "crates/fabro/acceptance/CONTRACT.md#accepted-differences",
        "no accepted difference is exercised by this scenario",
    );
    record.library_link("pebble", "pebble-cli::coding_sessions");
    finished.assert_no_leaked_processes().await;
    record.cleanup("clean", "no process launched for the case is still alive");
    let path = record.finish();
    assert!(path.starts_with(directory()), "{}", path.display());

    // The record, read back the way the coverage report reads it.
    let record = read_json(&path);
    assert_eq!(record["schema_version"], json!(1));
    assert_eq!(record["outcome"], json!("passed"));
    assert_eq!(record["scenario"]["id"], json!("evidence-smoke"));
    assert_eq!(record["scenario"]["backend"], json!("host"));
    assert_eq!(record["scenario"]["test"], json!(test));
    assert_eq!(record["scenario"]["family"], json!("evidence"));
    assert_eq!(record["launch"]["workflow"], json!(workflow));
    assert!(
        record["launch"]["workflow_text"]
            .as_str()
            .is_some_and(|t| t.contains("EVIDENCE_DONE")),
        "{}",
        record["launch"]
    );
    assert_eq!(record["services"][0]["provider"], json!("openai"));
    assert_eq!(
        record["services"][0]["scenarios_consumed"],
        json!(["answer"])
    );
    assert_eq!(record["services"][0]["unmatched_requests"], json!(0));
    assert_eq!(record["observations"]["raw"]["exit_code"], json!(0));
    assert_eq!(
        record["observations"]["normalized"]["status"],
        json!("success")
    );
    assert_eq!(
        record["final_context"]["response.agent"],
        json!("EVIDENCE_DONE")
    );
    assert_eq!(record["artifacts"][0]["bytes"], json!("draft\n".len()));
    assert_eq!(record["assertions"].as_array().map(Vec::len), Some(3));
    assert!(
        record["assertions"]
            .as_array()
            .expect("assertions")
            .iter()
            .all(|a| a["outcome"] == "passed"),
        "{}",
        record["assertions"]
    );
    assert_eq!(record["cleanup"]["result"], json!("clean"));
    assert_eq!(record["library_links"][0]["library"], json!("pebble"));
    assert_eq!(
        record["library_links"][0]["revision"],
        record["pins"]["pebble"]
    );
    for pin in [
        "pebble",
        "lithos_llm",
        "sandbox_driver",
        "twins",
        "fabro_reference",
    ] {
        let value = record["pins"][pin].as_str().expect(pin);
        assert!(value.len() >= 7 && value != "unpinned", "{pin}: {value}");
    }
    assert!(record["pins"]["petri"]["commit"].is_string());

    // The bundle: process output, the inspect document, the twin's log, and
    // no case copy because the scenario passed.
    let bundle = PathBuf::from(record["bundle"]["dir"].as_str().expect("bundle dir"));
    for file in [
        "stdout.txt",
        "stderr.txt",
        "inspect.json",
        "openai-requests.jsonl",
    ] {
        assert!(
            bundle.join(file).is_file(),
            "{file} in {}",
            bundle.display()
        );
    }
    assert!(!bundle.join("case").exists());
    assert!(record["bundle"]["case"].is_null());

    // The coverage report passes the run, checked on a private copy of this one
    // record so other scenarios recording into the same run cannot change the
    // counts.
    let evidence = case.root.join("evidence");
    fs::create_dir_all(evidence.join("records")).expect("evidence copy");
    fs::copy(
        &path,
        evidence
            .join("records")
            .join(path.file_name().expect("name")),
    )
    .expect("copy the record");
    let coverage = script("fabro-coverage-report.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
        "--strict",
    ]);
    assert!(
        coverage.status.success(),
        "{}",
        String::from_utf8_lossy(&coverage.stderr)
    );
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["totals"]["required"], json!(1));
    assert_eq!(report["totals"]["passed"], json!(1));
    assert_eq!(report["ok"], json!(true));
    twin.stop();
}

#[test]
fn a_failed_scenario_keeps_its_full_bundle_and_never_counts_as_passed() {
    let root = env::temp_dir().join(format!(
        "petri-evidence-failure-{}-{}",
        process::id(),
        testkit::unique_id()
    ));
    let case = root.join("case");
    fs::create_dir_all(case.join("run")).expect("case dir");
    fs::write(case.join("run").join("marker.txt"), "kept\n").expect("marker");
    let evidence = root.join("evidence");

    let mut record = Recorder::start_in(
        &evidence,
        "evidence-failure",
        Backend::Docker,
        &format!("{TEST_FILE}::a_failed_scenario_keeps_its_full_bundle"),
    );
    record.case_root(&case);
    assert!(record.check("holds", true, ""));
    assert!(!record.check("the file says shipped", false, "it says held"));
    let path = record.finish();

    let record = read_json(&path);
    assert_eq!(record["outcome"], json!("failed"));
    assert_eq!(record["scenario"]["backend"], json!("docker"));
    let copy = PathBuf::from(record["bundle"]["case"].as_str().expect("case copy"));
    assert_eq!(
        fs::read_to_string(copy.join("run").join("marker.txt")).expect("copied marker"),
        "kept\n"
    );
    assert_eq!(record["assertions"][1]["outcome"], json!("failed"));

    let coverage = script("fabro-coverage-report.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
        "--strict",
    ]);
    assert_eq!(coverage.status.code(), Some(1));
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["totals"]["failed"], json!(1));
    assert_eq!(report["totals"]["passed"], json!(0));
    assert_eq!(report["ok"], json!(false));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_panic_before_finish_still_writes_a_failed_record() {
    let evidence = env::temp_dir().join(format!(
        "petri-evidence-panic-{}-{}",
        process::id(),
        testkit::unique_id()
    ));
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let mut record = Recorder::start_in(
            &evidence,
            "evidence-panic",
            Backend::Host,
            &format!("{TEST_FILE}::a_panic_before_finish"),
        );
        record.check("first", true, "");
        panic!("the scenario blew up");
    }));
    assert!(result.is_err());
    let recorded = outcomes(&evidence);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    let (id, outcome) = recorded.iter().next().expect("one record");
    assert!(id.starts_with("evidence-panic--host--"), "{id}");
    assert_eq!(outcome, "failed");
    let record = read_json(&evidence.join("records").join(format!("{id}.json")));
    assert!(
        record["assertions"]
            .as_array()
            .expect("assertions")
            .iter()
            .any(|a| a["name"] == "record"
                && a["detail"].as_str().is_some_and(|d| d.contains("panicked"))),
        "{}",
        record["assertions"]
    );
    let _ = fs::remove_dir_all(&evidence);
}

#[test]
fn the_coverage_report_counts_only_passed_cells() {
    let root = env::temp_dir().join(format!(
        "petri-evidence-coverage-{}-{}",
        process::id(),
        testkit::unique_id()
    ));
    let evidence = root.join("evidence");
    // Scenario records (this module), a per-cell result (the scenario
    // tests' `CellRecord`), and a per-engine record (the differential
    // matrix) all feed the same report.
    Recorder::start_in(&evidence, "alpha/one", Backend::Host, "alpha_one").finish();
    Recorder::start_in(&evidence, "alpha/one", Backend::Docker, "alpha_one_docker")
        .skipped("no Docker daemon");
    fs::create_dir_all(evidence.join("cells")).expect("cells dir");
    fs::write(
        evidence.join("cells").join("beta__one@host__openai.json"),
        serde_json::to_vec_pretty(
            &json!({ "cell": "beta/one@host/openai", "status": "passed", "note": null }),
        )
        .expect("cell"),
    )
    .expect("write cell");
    fs::create_dir_all(evidence.join("epsilon/one")).expect("engine dir");
    fs::write(
        evidence.join("epsilon/one").join("petri.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1, "scenario": "epsilon/one", "engine": "petri",
            "pins": { "pebble": "a" },
            "assertions": [{ "name": "status", "passed": false, "detail": "failed" }]
        }))
        .expect("engine record"),
    )
    .expect("write engine record");
    let matrix = root.join("matrix.json");
    fs::write(
        &matrix,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "cells": [
                { "cell": "alpha/one@host/openai", "scenario": "alpha/one", "backend": "host", "agent": "api:openai", "required": true, "status": "planned", "reason": null, "test": "alpha_one" },
                { "cell": "alpha/one@docker/openai", "scenario": "alpha/one", "backend": "docker", "agent": "api:openai", "required": true, "status": "planned", "reason": null, "test": "alpha_one_docker" },
                { "cell": "beta/one@host/openai", "scenario": "beta/one", "backend": "host", "agent": "api:openai", "required": true, "status": "planned", "reason": null, "test": "beta_one" },
                { "cell": "gamma/one@host/none", "scenario": "gamma/one", "backend": "host", "agent": "none", "required": true, "status": "planned", "reason": null, "test": "gamma_one" },
                { "cell": "delta/one@host/none", "scenario": "delta/one", "backend": "host", "agent": "none", "required": false, "status": "excluded", "reason": "not in the replacement set", "test": null },
                { "cell": "zeta/one@docker/none", "scenario": "zeta/one", "backend": "docker", "agent": "none", "required": true, "status": "blocked", "reason": "needs a fixture repository", "test": null },
                { "cell": "epsilon/one@host/none", "scenario": "epsilon/one", "backend": "host", "agent": "none", "required": true, "status": "planned", "reason": null, "test": "epsilon_one" },
            ]
        }))
        .expect("matrix"),
    )
    .expect("write matrix");
    let args = |extra: &[&str]| -> Vec<String> {
        let mut args = vec![
            "--evidence".to_owned(),
            evidence.to_str().expect("utf-8").to_owned(),
            "--matrix".to_owned(),
            matrix.to_str().expect("utf-8").to_owned(),
        ];
        args.extend(extra.iter().map(|s| (*s).to_owned()));
        args
    };

    // Not strict: exit 0 and the counts.
    let plain = args(&[]);
    let output = script(
        "fabro-coverage-report.py",
        &plain.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(
        report["totals"]["required"],
        json!(6),
        "{}",
        report["totals"]
    );
    assert_eq!(report["totals"]["passed"], json!(2));
    assert_eq!(report["totals"]["skipped"], json!(1));
    assert_eq!(report["totals"]["missing"], json!(1));
    assert_eq!(report["totals"]["blocked"], json!(1));
    assert_eq!(report["totals"]["excluded"], json!(1));
    assert_eq!(report["totals"]["failed"], json!(1));
    assert_eq!(report["ok"], json!(false));
    assert_eq!(report["ci_ok"], json!(false));
    let markdown = fs::read_to_string(evidence.join("coverage.md")).expect("coverage.md");
    assert!(
        markdown.contains("| alpha/one@docker/openai | required | skipped |"),
        "{markdown}"
    );
    assert!(
        markdown.contains("| gamma/one@host/none | required | missing |"),
        "{markdown}"
    );
    assert!(
        markdown.contains("| epsilon/one@host/none | required | failed |"),
        "{markdown}"
    );
    assert!(markdown.contains("Gate: NOT passed"), "{markdown}");

    // Strict: the same run fails.
    let strict = args(&["--strict"]);
    let strict = script(
        "fabro-coverage-report.py",
        &strict.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(strict.status.code(), Some(1));

    // A runner failure overrides a passed record: two engines agreeing on a
    // wrong report still fail when the runner said so.
    let junit = root.join("junit.xml");
    fs::write(
        &junit,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites><testsuite name="petri-cli::t"><testcase classname="petri-cli::t" name="alpha_one"><failure message="assertion"/></testcase></testsuite></testsuites>"#,
    )
    .expect("junit");
    let with_junit = args(&["--junit", junit.to_str().expect("utf-8")]);
    let output = script(
        "fabro-coverage-report.py",
        &with_junit.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert!(output.status.success());
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["totals"]["passed"], json!(1));
    assert_eq!(report["totals"]["failed"], json!(2));

    // A runner responsible for the host backend only: the Docker cells are
    // excluded there, and a blocked cell alone keeps the run clean while the
    // gate stays not passed.
    fs::remove_file(evidence.join("epsilon/one").join("petri.json")).expect("remove");
    Recorder::start_in(&evidence, "gamma/one", Backend::Host, "gamma_one").finish();
    Recorder::start_in(&evidence, "epsilon/one", Backend::Host, "epsilon_one").finish();
    let host_only = args(&["--strict", "--backends", "host"]);
    let output = script(
        "fabro-coverage-report.py",
        &host_only.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(
        report["totals"]["required"],
        json!(4),
        "{}",
        report["totals"]
    );
    assert_eq!(report["totals"]["passed"], json!(4));
    assert_eq!(report["totals"]["excluded"], json!(3));
    assert_eq!(report["ci_ok"], json!(true));
    assert_eq!(report["ok"], json!(true));
    let _ = fs::remove_dir_all(&root);
}

/// A pinned-Fabro assertion that a decision record lists under
/// `known_defects` is expected: the cell passes with a note naming the
/// record. A failed assertion no record lists still fails the cell, Petri's
/// records never get the allowance, and without the record the same
/// failure is an ordinary failure.
#[test]
fn a_known_fabro_defect_is_expected_and_an_unexpected_one_still_fails() {
    const KNOWN: &str = "the append ran once";
    const OTHER: &str = "the last request went to the fallback";
    let root = env::temp_dir().join(format!(
        "petri-evidence-defects-{}-{}",
        process::id(),
        testkit::unique_id()
    ));
    let evidence = root.join("evidence");
    let scenario_dir = evidence.join("theta");
    fs::create_dir_all(&scenario_dir).expect("engine dir");
    let write = |path: &Path, value: &Value| {
        fs::write(path, serde_json::to_vec_pretty(value).expect("json")).expect("write");
    };
    let engine_record = |engine: &str, assertions: Value| {
        json!({
            "schema_version": 1, "scenario": "theta", "engine": engine,
            "pins": { "pebble": "a" }, "assertions": assertions,
        })
    };
    let passed = |name: &str| json!({ "name": name, "passed": true, "detail": null });
    let failed = |name: &str| json!({ "name": name, "passed": false, "detail": { "appends": 2 } });
    write(
        &scenario_dir.join("petri.json"),
        &engine_record("petri", json!([passed(KNOWN), passed(OTHER)])),
    );
    write(
        &scenario_dir.join("fabro.json"),
        &engine_record("fabro", json!([failed(KNOWN), passed(OTHER)])),
    );
    let decisions = root.join("decisions");
    fs::create_dir_all(&decisions).expect("decisions dir");
    fs::write(
        decisions.join("theta-repeats-the-append.toml"),
        r#"id = "theta-repeats-the-append"
title = "The pinned Fabro repeats the append"
scenarios = ["theta"]
bundles = ["*"]
fabro = "runs the append twice"
petri = "runs it once"
user_visible_effect = "a duplicated line"
reason = "a baseline defect of the pinned Fabro"
acceptance = "never; retire when the pin no longer repeats it"
known_defects = ["the append ran once"]
"#,
    )
    .expect("decision");
    let matrix = root.join("matrix.json");
    write(
        &matrix,
        &json!({
            "schema_version": 1,
            "cells": [
                { "cell": "theta@host/openai", "scenario": "theta", "backend": "host", "agent": "api:openai", "required": true, "status": "planned", "reason": null, "test": "petri-cli::fabro_differential::theta_matches_the_pinned_fabro" },
            ]
        }),
    );
    let run = |decisions: &Path| -> Output {
        script("fabro-coverage-report.py", &[
            "--evidence",
            evidence.to_str().expect("utf-8"),
            "--matrix",
            matrix.to_str().expect("utf-8"),
            "--decisions",
            decisions.to_str().expect("utf-8"),
            "--strict",
        ])
    };

    // The known defect is expected: the strict report passes and says why.
    let output = run(&decisions);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(
        report["totals"]["required"],
        json!(1),
        "{}",
        report["totals"]
    );
    assert_eq!(report["totals"]["passed"], json!(1));
    assert_eq!(report["totals"]["failed"], json!(0));
    assert_eq!(report["ok"], json!(true));
    assert_eq!(report["ci_ok"], json!(true));
    assert_eq!(
        report["known_defects"],
        json!([{ "scenario": "theta", "assertion": KNOWN, "decision": "theta-repeats-the-append" }])
    );
    let entry = &report["entries"][0];
    assert_eq!(entry["result"], json!("passed"));
    let detail = entry["detail"].as_str().expect("detail");
    assert!(
        detail.contains("theta-repeats-the-append") && detail.contains(KNOWN),
        "{detail}"
    );
    let markdown = fs::read_to_string(evidence.join("coverage.md")).expect("coverage.md");
    assert!(
        markdown.contains("Known defects of the pinned Fabro applied")
            && markdown.contains("Gate: passed."),
        "{markdown}"
    );

    // A Fabro failure no record lists still fails, beside the known one.
    write(
        &scenario_dir.join("fabro.json"),
        &engine_record("fabro", json!([failed(KNOWN), failed(OTHER)])),
    );
    let output = run(&decisions);
    assert_eq!(output.status.code(), Some(1));
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["totals"]["failed"], json!(1), "{}", report["totals"]);
    let detail = report["entries"][0]["detail"].as_str().expect("detail");
    assert!(
        detail.contains(&format!("fabro failed ['{OTHER}']")),
        "{detail}"
    );

    // Petri never gets the allowance.
    write(
        &scenario_dir.join("fabro.json"),
        &engine_record("fabro", json!([failed(KNOWN), passed(OTHER)])),
    );
    write(
        &scenario_dir.join("petri.json"),
        &engine_record("petri", json!([failed(KNOWN), passed(OTHER)])),
    );
    let output = run(&decisions);
    assert_eq!(output.status.code(), Some(1));
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["totals"]["failed"], json!(1), "{}", report["totals"]);
    let detail = report["entries"][0]["detail"].as_str().expect("detail");
    assert!(
        detail.contains(&format!("petri failed ['{KNOWN}']")),
        "{detail}"
    );

    // Without the decision record the Fabro failure is an ordinary failure.
    write(
        &scenario_dir.join("petri.json"),
        &engine_record("petri", json!([passed(KNOWN), passed(OTHER)])),
    );
    let output = run(&root.join("no-decisions"));
    assert_eq!(output.status.code(), Some(1));
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["totals"]["failed"], json!(1), "{}", report["totals"]);
    assert_eq!(report["known_defects"], json!([]));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn an_empty_evidence_run_is_not_a_passing_gate() {
    let evidence = env::temp_dir().join(format!(
        "petri-evidence-empty-{}-{}",
        process::id(),
        testkit::unique_id()
    ));
    fs::create_dir_all(&evidence).expect("evidence dir");
    let strict = script("fabro-coverage-report.py", &[
        "--evidence",
        evidence.to_str().expect("utf-8"),
        "--strict",
    ]);
    assert_eq!(strict.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&strict.stderr);
    assert!(stderr.contains("no evidence records"), "{stderr}");
    let report = read_json(&evidence.join("coverage.json"));
    assert_eq!(report["ok"], json!(false));
    let _ = fs::remove_dir_all(&evidence);
}

#[test]
fn a_required_asset_fails_instead_of_skipping_when_ci_asks() {
    assert_eq!(
        require::decide(true, Some("1"), "PETRI_REQUIRE_FABRO_BUNDLES", "absent"),
        Ok(true)
    );
    assert_eq!(
        require::decide(false, None, "PETRI_REQUIRE_FABRO_BUNDLES", "absent"),
        Ok(false)
    );
    assert_eq!(
        require::decide(false, Some(""), "PETRI_REQUIRE_FABRO_BUNDLES", "absent"),
        Ok(false)
    );
    assert_eq!(
        require::decide(
            false,
            Some("1"),
            "PETRI_REQUIRE_FABRO_BINARY",
            "the pinned fabro binary is absent"
        ),
        Err("PETRI_REQUIRE_FABRO_BINARY is set, but the pinned fabro binary is absent".to_owned())
    );
}

#[test]
fn the_vendored_bundles_are_found_or_skipped_visibly() {
    // The bundles are tracked in the repository. The helper names the bundle
    // directory when it exists; on an incomplete checkout it skips (or fails
    // under the require variable, which CI sets).
    if let Some(dir) = require::bundle("interview") {
        assert!(
            dir.join(".fabro/workflows/interview/workflow.fabro")
                .is_file()
        );
    }
}

/// A cell a test skips for want of a resource stays `skipped` after the record
/// is dropped; the report then reads it as skipped, never as a failed pass.
#[test]
fn a_skipped_cell_record_stays_skipped_when_dropped() {
    let dir = testkit::RunDir::new("evidence-skipped-cell");
    support::fabro::scenario::CellRecord::skip_in(
        dir.path(),
        "probe/skipped@docker/none",
        "no Docker daemon",
    );
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(dir.path().join("probe__skipped__docker__none.json")).expect("record"),
    )
    .expect("json");
    assert_eq!(record["status"], "skipped", "{record}");
    assert_eq!(record["note"], "no Docker daemon", "{record}");
}
