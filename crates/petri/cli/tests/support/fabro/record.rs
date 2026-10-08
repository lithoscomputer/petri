//! Machine-readable evidence records for the black box scenarios.
//!
//! One record per scenario run, written into the run-scoped directory
//! `PETRI_EVIDENCE_DIR` names (`scripts/test-fabro-blackbox.sh` and CI set
//! it; the default is `target/fabro-evidence/adhoc`). A record carries every
//! pin the run went through, the launch inputs, the matched provider twin
//! scenarios, the raw and normalized observations, the final context, the
//! artifacts, the process output, the assertions, the compatibility
//! decisions, and the cleanup result. `scripts/fabro-coverage-report.py`
//! folds the records into the coverage report.
//!
//! Layout under the evidence directory:
//!
//! ```text
//! records/<record id>.json          the record (schema version 1)
//! bundles/<record id>/stdout.txt    the run's process output
//! bundles/<record id>/stderr.txt
//! bundles/<record id>/inspect.json  `petri inspect --json`, when the run finished
//! bundles/<record id>/<provider>-requests.jsonl   each twin's request log
//! bundles/<record id>/case/         the whole case directory, on failure only
//! ```
//!
//! The record id is `<scenario>--<backend>--<test>` (plus `--r<n>` for a
//! repeated run, `PETRI_EVIDENCE_REPEAT`). A record whose test panics before
//! `finish` is still written, as failed, by `Drop`.

use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, io, thread};

use serde_json::{Map, Value, json};

use super::evidence::locked_revision;
use super::launch::Finished;
use super::require::workspace_root;
use super::twins::Twin;

/// The record format this module writes.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// The backend a scenario ran on: one coverage cell per scenario and backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    Host,
    Docker,
}

impl Backend {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Docker => "docker",
        }
    }
}

/// The evidence directory of this run.
pub(crate) fn directory() -> PathBuf {
    env::var_os("PETRI_EVIDENCE_DIR").map_or_else(
        || workspace_root().join("target/fabro-evidence/adhoc"),
        PathBuf::from,
    )
}

/// The commit `Cargo.lock` locks for a git dependency, or `"unpinned"`.
fn locked_rev(name: &str) -> String {
    locked_revision(name).unwrap_or_else(|| "unpinned".to_owned())
}

/// Petri's own commit: `GITHUB_SHA` on a runner, else `git rev-parse HEAD`.
fn petri_commit(root: &Path) -> String {
    if let Ok(sha) = env::var("GITHUB_SHA")
        && !sha.is_empty()
    {
        return sha;
    }
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unknown".to_owned(),
            |output| String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
}

/// Every revision a scenario runs through, read from `Cargo.lock` and the
/// Fabro pin file at record time. The keys are the rows of the "Pinned
/// revisions" table in `crates/fabro/acceptance/CONTRACT.md`.
pub(crate) fn pins() -> Value {
    let root = workspace_root();
    let fabro = fs::read_to_string(root.join("crates/fabro/corpus-pin.txt"))
        .unwrap_or_default()
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or("unpinned")
        .to_owned();
    json!({
        "petri": { "version": env!("CARGO_PKG_VERSION"), "commit": petri_commit(&root) },
        "pebble": locked_rev("pebble-coding-agent"),
        "lithos_llm": locked_rev("lithos-llm"),
        "sandbox_driver": locked_rev("sandbox-driver"),
        "twins": locked_rev("twin-openai"),
        "fabro_reference": fabro,
    })
}

fn now() -> String {
    // RFC 3339 without a dependency: seconds since the epoch is enough for
    // ordering, and the report shows it as such.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("{secs}")
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `petri inspect --json` for a run directory, or `None` when the command
/// fails (an unfinished or damaged run is still evidence, just not a
/// document). Same isolation as `inspect::inspect`, without its assertion.
fn inspect_document(run_dir: &Path) -> Option<Value> {
    let path = env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let output = Command::new(env!("CARGO_BIN_EXE_petri"))
        .env_clear()
        .env("PATH", path)
        .args(["inspect", "--json", "--run-dir"])
        .arg(run_dir)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| serde_json::from_slice(&output.stdout).ok())
        .flatten()
}

/// Copy a tree, skipping symlinks (the per-case plugin links point at large
/// binaries that are not evidence).
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// One scenario's record in progress. Build it up while the test runs, then
/// [`Recorder::finish`] writes it. The outcome is `passed` only when every
/// [`Recorder::check`] held and nothing marked the scenario skipped or
/// blocked.
pub(crate) struct Recorder {
    directory:     PathBuf,
    record_id:     String,
    scenario:      Map<String, Value>,
    started:       Instant,
    started_at:    String,
    launch:        Value,
    services:      Vec<Value>,
    observations:  Value,
    final_context: Value,
    artifacts:     Vec<Value>,
    assertions:    Vec<Value>,
    decisions:     Vec<Value>,
    differential:  Value,
    cleanup:       Value,
    links:         Vec<Value>,
    skip_reason:   Option<String>,
    block_reason:  Option<String>,
    case_root:     Option<PathBuf>,
    finished:      bool,
}

impl Recorder {
    /// Start a record in this run's evidence directory ([`directory`]).
    pub(crate) fn start(scenario: &str, backend: Backend, test: &str) -> Self {
        Self::start_in(&directory(), scenario, backend, test)
    }

    /// Start a record under an explicit evidence directory.
    pub(crate) fn start_in(directory: &Path, scenario: &str, backend: Backend, test: &str) -> Self {
        let mut record_id = format!(
            "{}--{}--{}",
            sanitize(scenario),
            backend.id(),
            sanitize(test)
        );
        if let Ok(repeat) = env::var("PETRI_EVIDENCE_REPEAT")
            && !repeat.is_empty()
        {
            record_id.push_str("--r");
            record_id.push_str(&sanitize(&repeat));
        }
        let mut meta = Map::new();
        meta.insert("id".into(), json!(scenario));
        meta.insert("backend".into(), json!(backend.id()));
        meta.insert("test".into(), json!(test));
        meta.insert("family".into(), Value::Null);
        meta.insert("bundle".into(), Value::Null);
        Self {
            directory: directory.to_path_buf(),
            record_id,
            scenario: meta,
            started: Instant::now(),
            started_at: now(),
            launch: Value::Null,
            services: Vec::new(),
            observations: Value::Null,
            final_context: Value::Null,
            artifacts: Vec::new(),
            assertions: Vec::new(),
            decisions: Vec::new(),
            differential: Value::Null,
            cleanup: json!({ "result": "not_checked", "detail": "" }),
            links: Vec::new(),
            skip_reason: None,
            block_reason: None,
            case_root: None,
            finished: false,
        }
    }

    pub(crate) fn record_id(&self) -> &str {
        &self.record_id
    }

    /// Where this record's bundle files go.
    pub(crate) fn bundle_dir(&self) -> PathBuf {
        self.directory.join("bundles").join(&self.record_id)
    }

    /// The scenario family (`review`, `interview`, ...) and the bundle id
    /// from `bundles.lock.json`, when the scenario runs a required bundle.
    pub(crate) fn scenario_meta(&mut self, family: &str, bundle: Option<&str>) {
        self.scenario.insert("family".into(), json!(family));
        self.scenario.insert("bundle".into(), json!(bundle));
    }

    /// The case directory, copied whole into the bundle on failure.
    pub(crate) fn case_root(&mut self, root: &Path) {
        self.case_root = Some(root.to_path_buf());
    }

    /// What the run was launched with: the workflow, the extra `petri run`
    /// arguments, the `--input` pairs, and the interview script if any.
    pub(crate) fn launch(&mut self, workflow: &Path, args: &[&str], inputs: &[(&str, &str)]) {
        let script = args
            .windows(2)
            .find(|pair| pair[0] == "--interview-script")
            .map(|pair| pair[1]);
        let workflow_toml = workflow.with_file_name("workflow.toml");
        self.launch = json!({
            "workflow": workflow,
            "args": args,
            "inputs": inputs.iter().map(|(k, v)| json!({ "key": k, "value": v })).collect::<Vec<_>>(),
            "interview_script": script,
            "workflow_toml": workflow_toml.is_file().then(|| fs::read_to_string(&workflow_toml).unwrap_or_default()),
            "workflow_text": fs::read_to_string(workflow).unwrap_or_default(),
        });
    }

    /// A provider twin the run talked to: which scripted scenarios it
    /// consumed, in order, and whether any request matched nothing. The
    /// twin's request log is copied into the bundle.
    pub(crate) fn twin(&mut self, twin: &Twin) {
        let log = twin.request_log();
        let dir = self.bundle_dir();
        let _ = fs::create_dir_all(&dir);
        let log_path = dir.join(format!("{}-requests.jsonl", twin.provider.id()));
        let lines: Vec<String> = log.iter().map(Value::to_string).collect();
        let _ = fs::write(&log_path, lines.join("\n") + "\n");
        self.services.push(json!({
            "kind": "twin",
            "provider": twin.provider.id(),
            "base_url": twin.base_url,
            "scenarios_consumed": twin.consumed(),
            "unmatched_requests": twin.unmatched(),
            "requests": log.len(),
            "request_log": log_path,
        }));
    }

    /// What the launch left behind: the exit, the process output (into the
    /// bundle), the normalized run facts from the terminal lines, and the
    /// final context read through `petri inspect --json` when the run
    /// finished.
    pub(crate) fn finished(&mut self, finished: &Finished) {
        self.case_root = Some(finished.case_root.clone());
        let dir = self.bundle_dir();
        let _ = fs::create_dir_all(&dir);
        let stdout = dir.join("stdout.txt");
        let stderr = dir.join("stderr.txt");
        let _ = fs::write(&stdout, &finished.stdout);
        let _ = fs::write(&stderr, &finished.stderr);
        let mut inspect_path = Value::Null;
        if finished.code.is_some()
            && finished.run_dir.join("run.json").is_file()
            && let Some(document) = inspect_document(&finished.run_dir)
        {
            let path = dir.join("inspect.json");
            let _ = fs::write(
                &path,
                serde_json::to_vec_pretty(&document).unwrap_or_default(),
            );
            inspect_path = json!(path);
            if document["complete"].as_bool() == Some(true) {
                self.final_context = super::inspect::root_context(&document).clone();
            }
        }
        let receipt = finished.run_dir.join("interviews.json");
        let interviews = receipt
            .is_file()
            .then(|| {
                fs::read_to_string(&receipt)
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            })
            .flatten();
        self.observations = json!({
            "raw": {
                "exit_code": finished.code,
                "timed_out": finished.timed_out,
                "stdout": stdout,
                "stderr": stderr,
                "stdout_bytes": finished.stdout.len(),
                "stderr_bytes": finished.stderr.len(),
                "inspect": inspect_path,
                "run_dir": finished.run_dir,
            },
            "normalized": {
                "status": finished.status_line(),
                "finished_nodes": finished.finished_nodes(),
                "workspaces": finished.reported_workspaces(),
                "sandboxes": finished.reported_sandboxes(),
                "interviews": interviews,
            },
        });
    }

    /// Record one assertion. Returns `ok` so a test can also `assert!` it.
    pub(crate) fn check(&mut self, name: &str, ok: bool, detail: impl Display) -> bool {
        self.assertions.push(json!({
            "name": name,
            "outcome": if ok { "passed" } else { "failed" },
            "detail": detail.to_string(),
        }));
        ok
    }

    /// A file the scenario produced or checked, with its role.
    pub(crate) fn artifact(&mut self, path: &Path, role: &str) {
        let digest = fs::read(path).ok().map(|bytes| bytes.len());
        self.artifacts
            .push(json!({ "path": path, "role": role, "bytes": digest }));
    }

    /// A compatibility decision the scenario relies on: an accepted
    /// difference or a tracked departure, by its `CONTRACT.md` reference.
    pub(crate) fn decision(&mut self, id: &str, kind: &str, reference: &str, detail: &str) {
        self.decisions.push(json!({
            "id": id, "kind": kind, "reference": reference, "detail": detail,
        }));
    }

    /// The differential comparison against the pinned Fabro, as the
    /// differential harness records it (the shape is that task's).
    pub(crate) fn differential(&mut self, comparison: Value) {
        self.differential = comparison;
    }

    /// The owning library's contract test a scenario exercises, with the
    /// revision it is pinned at.
    pub(crate) fn library_link(&mut self, library: &str, test: &str) {
        let revision = pins()[library].clone();
        self.links
            .push(json!({ "library": library, "revision": revision, "test": test }));
    }

    /// The cleanup result: `clean`, `leaked`, or `not_checked`.
    pub(crate) fn cleanup(&mut self, result: &str, detail: &str) {
        self.cleanup = json!({ "result": result, "detail": detail });
    }

    /// Write the record as skipped: an absent asset or backend. Never a pass.
    pub(crate) fn skipped(mut self, reason: &str) -> PathBuf {
        self.skip_reason = Some(reason.to_owned());
        self.finish()
    }

    /// Write the record as blocked: the scenario is required but a recorded
    /// blocker keeps it from running. Never a pass.
    pub(crate) fn blocked(mut self, reason: &str) -> PathBuf {
        self.block_reason = Some(reason.to_owned());
        self.finish()
    }

    /// Write the record and return its path. On failure the whole case
    /// directory is copied into the bundle.
    pub(crate) fn finish(mut self) -> PathBuf {
        self.finished = true;
        self.write(None)
    }

    fn outcome(&self, note: Option<&str>) -> &'static str {
        if note.is_some() || self.assertions.iter().any(|a| a["outcome"] == "failed") {
            "failed"
        } else if self.skip_reason.is_some() {
            "skipped"
        } else if self.block_reason.is_some() {
            "blocked"
        } else {
            "passed"
        }
    }

    fn write(&mut self, note: Option<&str>) -> PathBuf {
        let outcome = self.outcome(note);
        let records = self.directory.join("records");
        let _ = fs::create_dir_all(&records);
        let bundle = self.bundle_dir();
        let mut case_copy = Value::Null;
        if outcome == "failed"
            && let Some(root) = &self.case_root
            && root.is_dir()
        {
            let target = bundle.join("case");
            if copy_tree(root, &target).is_ok() {
                case_copy = json!(target);
            }
        }
        let mut assertions = self.assertions.clone();
        if let Some(note) = note {
            assertions.push(json!({ "name": "record", "outcome": "failed", "detail": note }));
        }
        let record = json!({
            "schema_version": SCHEMA_VERSION,
            "record_id": self.record_id,
            "scenario": Value::Object(self.scenario.clone()),
            "run": {
                "evidence_dir": self.directory,
                "started_at_epoch": self.started_at,
                "duration_ms": self.started.elapsed().as_millis(),
                "repeat": env::var("PETRI_EVIDENCE_REPEAT").ok(),
                "host": { "os": env::consts::OS, "arch": env::consts::ARCH },
                "ci": {
                    "github_run_id": env::var("GITHUB_RUN_ID").ok(),
                    "github_job": env::var("GITHUB_JOB").ok(),
                    "runner": env::var("RUNNER_NAME").ok(),
                },
            },
            "pins": pins(),
            "launch": self.launch,
            "services": self.services,
            "observations": self.observations,
            "final_context": self.final_context,
            "artifacts": self.artifacts,
            "process_output": self.observations["raw"].clone(),
            "assertions": assertions,
            "outcome": outcome,
            "skip_reason": self.skip_reason,
            "block_reason": self.block_reason,
            "compatibility": {
                "decisions": self.decisions,
                "differential": self.differential,
            },
            "cleanup": self.cleanup,
            "library_links": self.links,
            "bundle": {
                "dir": bundle.is_dir().then_some(&bundle),
                "case": case_copy,
            },
        });
        let path = records.join(format!("{}.json", self.record_id));
        let _ = fs::write(
            &path,
            serde_json::to_vec_pretty(&record).unwrap_or_default(),
        );
        path
    }
}

impl Drop for Recorder {
    /// A test that panics before `finish` still leaves a failed record, so
    /// the coverage report shows the scenario as failed rather than missing.
    fn drop(&mut self) {
        if !self.finished {
            let note = if thread::panicking() {
                "the test panicked before the record was finished"
            } else {
                "the test ended before the record was finished"
            };
            self.write(Some(note));
        }
    }
}

/// The scenario ids and outcomes recorded under `directory`, for tests of
/// the reporting itself.
pub(crate) fn outcomes(directory: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Ok(entries) = fs::read_dir(directory.join("records")) {
        for entry in entries.flatten() {
            let Ok(text) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            out.insert(
                record["record_id"].as_str().unwrap_or_default().to_owned(),
                record["outcome"].as_str().unwrap_or_default().to_owned(),
            );
        }
    }
    out
}
