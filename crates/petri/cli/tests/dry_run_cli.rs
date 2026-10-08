//! A dry run touches no provider: `petri run --dry-run` of a bundle whose
//! settings select a Daytona or Docker environment succeeds with an empty
//! `PATH` and no `PETRI_SANDBOX_*` variable, its scope records name the
//! `simulated` provider, and every corpus bundle that lowers reaches exit
//! the same way. A dry run also persists the graph the launch lowered, which
//! is where `--model` and `--provider` land.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{env, fs};

use testkit::RunDir;

/// `petri` with the environment cleared: an empty directory as the whole
/// `PATH`, `HOME` and `TMPDIR` kept, and no sandbox plugin setting at all.
fn petri_without_plugins(dir: &RunDir) -> Command {
    let empty = dir.path().join("empty-bin");
    fs::create_dir_all(&empty).expect("the empty bin dir");
    let mut command = Command::new(env!("CARGO_BIN_EXE_petri"));
    command.env_clear();
    command.env("PATH", &empty);
    for key in ["HOME", "TMPDIR"] {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

/// A bundle at `root`: the project layer selecting `provider`, and one
/// workflow with a prompt stage and a human gate.
fn write_bundle(root: &Path, provider: &str, image: Option<&str>) -> PathBuf {
    let project = root.join(".fabro");
    let workflow_dir = project.join("workflows/demo");
    fs::create_dir_all(&workflow_dir).expect("the bundle dirs");
    let image = image
        .map(|image| format!("\n[environments.selected.image]\ndocker = \"{image}\"\n"))
        .unwrap_or_default();
    let settings = format!(
        "[run.environment]\nid = \"selected\"\n\n[environments.selected]\nprovider = \
         \"{provider}\"\n{image}"
    );
    fs::write(project.join("project.toml"), settings).expect("write the project layer");
    let workflow = workflow_dir.join("workflow.fabro");
    fs::write(
        &workflow,
        r#"digraph Demo {
            start [shape=Mdiamond]
            exit [shape=Msquare]
            plan [prompt="Plan"]
            gate [shape=hexagon, label="Go?"]
            build [shape=parallelogram, script="echo built"]
            start -> plan -> gate
            gate -> build [label="[G] Go"]
            gate -> exit [label="[S] Stop"]
            build -> exit
        }"#,
    )
    .expect("write the workflow");
    workflow
}

fn dry_run(dir: &RunDir, workflow: &Path, run_dir: &Path) -> Output {
    petri_without_plugins(dir)
        .args(["run", "--quiet", "--dry-run", "--run-dir"])
        .arg(run_dir)
        .arg(workflow)
        .output()
        .expect("petri runs")
}

/// Every record in the run's engine logs and coordinator log.
fn records(run_dir: &Path) -> String {
    let mut out = fs::read_to_string(run_dir.join("coordinator.jsonl")).unwrap_or_default();
    let executions = run_dir.join("executions");
    if let Ok(entries) = fs::read_dir(&executions) {
        for entry in entries.flatten() {
            if let Ok(log) = fs::read_to_string(entry.path().join("events.jsonl")) {
                out.push_str(&log);
            }
        }
    }
    out
}

fn assert_simulated_dry_run(label: &str, provider: &str, image: Option<&str>) {
    let dir = RunDir::new(label);
    let workflow = write_bundle(&dir.path().join("bundle"), provider, image);
    let run_dir = dir.path().join("run");
    let ran = dry_run(&dir, &workflow, &run_dir);
    let stderr = String::from_utf8_lossy(&ran.stderr);
    assert!(ran.status.success(), "{stderr}");
    assert!(stderr.contains("run: success"), "{stderr}");
    for stage in ["plan", "gate", "build", "exit"] {
        assert!(
            stderr.contains(&format!("success {stage}")),
            "{stage}:\n{stderr}"
        );
    }
    assert!(
        !stderr.contains("plugin") && !stderr.contains("acquire failed"),
        "no provider was reached for:\n{stderr}"
    );
    let records = records(&run_dir);
    assert!(
        records.contains(r#""event":"scope.acquired""#)
            && records.contains(r#""provider":"simulated""#),
        "the scope records name the simulated provider:\n{records}"
    );
    assert!(
        records.contains(r#""event":"scope.released""#),
        "the lease's end is recorded:\n{records}"
    );
    assert!(
        !records.contains(&format!(r#""provider":"{provider}""#)),
        "no lease was reserved on the selected provider:\n{records}"
    );
    assert!(
        !run_dir.join("host-registry").exists() && !run_dir.join("scopes").exists(),
        "no host plugin ran and no workspace was created"
    );
}

#[test]
fn a_dry_run_of_a_daytona_bundle_needs_no_plugin() {
    assert_simulated_dry_run("dry-run-daytona", "daytona", None);
}

#[test]
fn a_dry_run_of_a_docker_bundle_needs_no_plugin() {
    assert_simulated_dry_run("dry-run-docker", "docker", Some("alpine:3.20"));
}

/// `petri run --model` and `--provider` on a bare DOT file, with no settings
/// files: the launch's choice beats the graph's `default_model` and
/// `default_provider` on a node that names no model, a node that names its
/// own model keeps it with the graph's provider, and the persisted graph's
/// `fabro.launch` records the launch.
#[test]
fn the_launch_model_beats_the_graph_default_in_a_bare_dot_file() {
    let dir = RunDir::new("dry-run-launch-model");
    let workflow = dir.path().join("plain.dot");
    fs::write(
        &workflow,
        r#"digraph Plain {
            graph [default_model="graph-model", default_provider="graph-provider"]
            start [shape=Mdiamond]
            exit [shape=Msquare]
            plan [prompt="Plan"]
            named [prompt="Review", model="node-model"]
            start -> plan -> named -> exit
        }"#,
    )
    .expect("write the workflow");
    let run_dir = dir.path().join("run");
    let ran = petri_without_plugins(&dir)
        .args(["run", "--quiet", "--dry-run", "--run-dir"])
        .arg(&run_dir)
        .args(["--model", "cli-model", "--provider", "cli-provider"])
        .arg(&workflow)
        .output()
        .expect("petri runs");
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let graphs = fs::read_dir(run_dir.join("graphs"))
        .expect("the persisted graphs")
        .flatten()
        .map(|entry| {
            let text = fs::read_to_string(entry.path()).expect("read a graph");
            serde_json::from_str::<serde_json::Value>(&text).expect("a graph is JSON")
        })
        .collect::<Vec<_>>();
    let root = graphs
        .iter()
        .find(|graph| graph["params"].get("fabro.launch").is_some())
        .expect("the root graph carries the launch");
    let config = |prompt: &str| {
        root["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .map(|node| &node["step"]["config"])
            .find(|config| config["prompt"] == prompt)
            .unwrap_or_else(|| panic!("a node prompting {prompt:?}"))
            .clone()
    };
    let plan = config("Plan");
    assert_eq!(plan["model"], "cli-model");
    assert_eq!(plan["provider"], "cli-provider");
    let named = config("Review");
    assert_eq!(named["model"], "node-model");
    assert_eq!(named["provider"], "graph-provider");
    let launch = &root["params"]["fabro.launch"];
    assert_eq!(launch["model"], "cli-model");
    assert_eq!(launch["provider"], "cli-provider");
}

/// The corpus checkout `scripts/corpus-fetch-fabro.sh` makes.
fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fabro/corpus/fabro")
}

/// Every workflow the corpus holds, as the acceptance sweep walks it:
/// Fabro's own `.fabro/workflows/**/*.fabro`, the demo workflows under
/// `docs/`, and the Attractor `test/attractor/*.dot` fixtures.
fn corpus_workflows(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, &root.join(".fabro"), "fabro", &mut out);
    walk(root, &root.join("docs"), "fabro", &mut out);
    walk(root, &root.join("test/attractor"), "dot", &mut out);
    out.sort();
    out.dedup();
    out
}

fn walk(root: &Path, dir: &Path, extension: &str, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "node_modules") {
                continue;
            }
            walk(root, &path, extension, out);
        } else if path.extension().is_some_and(|e| e == extension)
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Corpus workflows that cannot reach exit under stubs, and why, as the
/// acceptance sweep (`petri-fabro-acceptance`'s `runs` test) lists them.
const EXPECTED_STOPS: &[(&str, &str)] = &[(
    ".fabro/workflows/code-review/code-review.fabro",
    "`finders` fans out over a list the `prepare` command builds",
)];

/// The corpus project layer selects a Daytona environment for every
/// bundle, so before the dry run acquired no sandbox this sweep needed the
/// Daytona plugin. Now every workflow that lowers reaches exit with an empty
/// `PATH`; one that does not lower is refused at load, before any scope.
#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test says why on the runner's stderr; a test binary has no other sink"
)]
fn every_corpus_workflow_dry_runs_without_a_plugin() {
    let root = corpus_root();
    if !root.join(".fabro/workflows").is_dir() {
        assert!(
            !env::var("PETRI_REQUIRE_FABRO_CORPUS").is_ok_and(|v| !v.is_empty()),
            "PETRI_REQUIRE_FABRO_CORPUS is set, but the Fabro corpus is not fetched"
        );
        eprintln!("skipping: Fabro corpus not fetched; run scripts/corpus-fetch-fabro.sh");
        return;
    }
    let dir = RunDir::new("dry-run-corpus");
    let mut reached_exit = 0;
    let mut refused_at_load = Vec::new();
    let mut stuck = Vec::new();
    for (index, file) in corpus_workflows(&root).iter().enumerate() {
        let run_dir = dir.path().join(format!("run-{index}"));
        let ran = dry_run(&dir, &root.join(file), &run_dir);
        let stderr = String::from_utf8_lossy(&ran.stderr);
        assert!(
            !stderr.contains("plugin") && !stderr.contains("acquire failed"),
            "{file} reached for a provider:\n{stderr}"
        );
        if ran.status.success() {
            let records = records(&run_dir);
            assert!(
                records.contains(r#""provider":"simulated""#),
                "{file}: the scope records name the simulated provider:\n{records}"
            );
            reached_exit += 1;
        } else if stderr.contains("error[") {
            refused_at_load.push(file.clone());
        } else if EXPECTED_STOPS.iter().any(|(stop, _)| stop == file) {
            assert!(
                stderr.contains("run: failed"),
                "{file} is an expected stop, not a refusal:\n{stderr}"
            );
        } else {
            stuck.push(format!("{file}:\n{stderr}"));
        }
    }
    assert!(reached_exit > 0, "some corpus workflows reach exit");
    assert!(
        stuck.is_empty(),
        "{} corpus workflow(s) did not reach exit under a plugin-free dry run:\n{}",
        stuck.len(),
        stuck.join("\n")
    );
    eprintln!(
        "dry-run corpus sweep: {reached_exit} reached exit, {} refused at load, {} expected stop(s)",
        refused_at_load.len(),
        EXPECTED_STOPS.len()
    );
}
