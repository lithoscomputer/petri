//! Evidence records for the differential matrix.
//!
//! Every scenario run writes one machine-readable record per engine under
//! the evidence root (`PETRI_EVIDENCE_DIR`, default `target/fabro-differential`
//! in the repository), with the pins of everything that produced it: Petri's
//! source identity, the consumed Pebble and `lithos-llm` revisions, the
//! sandbox-driver plugin revision, the twins' revision, the Fabro identity
//! and pin, the bundle digest, tool versions and the sandbox image. The
//! normalized projection sits beside pointers to the raw observations.
//!
//! The committed Fabro reference of a scenario
//! (`crates/fabro/acceptance/scenarios/<name>/fabro-reference/reference.json`)
//! is the baseline. A live Fabro run must reproduce it exactly; a change is
//! a failure with a pointer-by-pointer diff, and only
//! `PETRI_FABRO_REFERENCE_RECORD=1` rewrites the file, so a refresh is a
//! reviewable `git diff`, never a silent replacement.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, fs};

use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use super::compare::{Projection, from_value, json_differences, to_value};
use super::fabro_adapter::{FabroBinary, pinned_commit, repo_root};

/// Set to `1` to rewrite a scenario's committed Fabro reference from the
/// live run.
pub(crate) const RECORD_ENV: &str = "PETRI_FABRO_REFERENCE_RECORD";

/// Where evidence records go.
pub(crate) fn evidence_root() -> PathBuf {
    env::var_os("PETRI_EVIDENCE_DIR").map_or_else(
        || repo_root().join("target").join("fabro-differential"),
        PathBuf::from,
    )
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git(args: &[&str]) -> Option<String> {
    let root = repo_root();
    let mut full = vec!["-C", root.to_str()?];
    full.extend_from_slice(args);
    command_output("git", &full)
}

/// A locked git dependency's revision, from `Cargo.lock`. The manifests
/// track `branch = "main"`; the lockfile chooses the commit.
pub(crate) fn locked_revision(package: &str) -> Option<String> {
    let text = fs::read_to_string(repo_root().join("Cargo.lock")).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        if line == "[[package]]" {
            in_package = false;
            continue;
        }
        if line == format!("name = \"{package}\"") {
            in_package = true;
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(source) = line.strip_prefix("source = \"") {
            return source
                .rsplit('#')
                .next()
                .map(|rev| rev.trim_end_matches('"').to_owned());
        }
    }
    None
}

/// The sandbox-driver plugin revision the tests launch: the locked
/// `sandbox-driver` commit (`scripts/plugins-build.sh` builds the plugins
/// from the same commit).
fn plugin_revision() -> Option<String> {
    locked_revision("sandbox-driver")
}

/// The pins of everything that produced a record.
pub(crate) fn pins(fabro: Option<&FabroBinary>) -> Value {
    let mut pins = Map::new();
    pins.insert(
        "petri".into(),
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "commit": git(&["rev-parse", "HEAD"]),
            "dirty": git(&["status", "--porcelain", "--untracked-files=no"])
                .map(|status| !status.is_empty()),
            "binary": env!("CARGO_BIN_EXE_petri"),
        }),
    );
    pins.insert(
        "libraries".into(),
        json!({
            "pebble-coding-agent": locked_revision("pebble-coding-agent"),
            "lithos-llm": locked_revision("lithos-llm"),
            "sandbox-driver": locked_revision("sandbox-driver"),
            "twin-openai": locked_revision("twin-openai"),
            "twin-anthropic": locked_revision("twin-anthropic"),
        }),
    );
    pins.insert(
        "fabro".into(),
        json!({
            "pin": pinned_commit(),
            "version": fabro.map(|f| f.version.clone()),
            "commit": fabro.map(|f| f.commit.clone()),
            "binary": fabro.map(|f| f.path.to_string_lossy().into_owned()),
        }),
    );
    pins.insert(
        "sandbox".into(),
        json!({
            // Host and Docker are sandbox-driver built-ins, linked in.
            "providers": "built-in",
            "plugin_revision": plugin_revision(),
            // The runner label the Docker backend maps to its default image.
            "docker_runner_label": "ubuntu-24.04",
        }),
    );
    pins.insert(
        "tools".into(),
        json!({
            "git": command_output("git", &["--version"]),
            "python3": command_output("python3", &["--version"]),
            "bash": command_output("bash", &["--version"]).map(|v| v.lines().next().unwrap_or_default().to_owned()),
            "docker": command_output("docker", &["--version"]),
            "rustc": command_output("rustc", &["--version"]),
            "os": format!("{} {}", env::consts::OS, env::consts::ARCH),
        }),
    );
    Value::Object(pins)
}

/// The SHA-256 over `<sha256> <path>` lines of the bundle's declared files
/// under `dir`, sorted by path: the bundle digest. Only the files both
/// engines run are covered, so scenario metadata beside them (task 17's
/// scenario documents, the `fabro-reference` capture) never moves it. A
/// missing file fails: the declaration is part of the scenario.
pub(crate) fn bundle_digest(dir: &Path, files: &[&str]) -> String {
    let mut paths: Vec<&str> = files.to_vec();
    paths.sort_unstable();
    let mut lines = String::new();
    for relative in paths {
        let path = dir.join(relative);
        let bytes = fs::read(&path)
            .unwrap_or_else(|error| panic!("bundle file {}: {error}", path.display()));
        let digest = Sha256::digest(&bytes);
        let _ = writeln!(lines, "{digest:x} {relative}");
    }
    format!("{:x}", Sha256::digest(lines.as_bytes()))
}

/// One evidence record under construction.
pub(crate) struct Record {
    pub(crate) scenario:   String,
    pub(crate) engine:     String,
    pub(crate) fields:     Map<String, Value>,
    pub(crate) assertions: Vec<Value>,
}

impl Record {
    pub(crate) fn new(scenario: &str, engine: &str, fabro: Option<&FabroBinary>) -> Self {
        let mut fields = Map::new();
        fields.insert("schema_version".into(), json!(1));
        fields.insert("scenario".into(), json!(scenario));
        fields.insert("engine".into(), json!(engine));
        fields.insert("pins".into(), pins(fabro));
        Self {
            scenario: scenario.to_owned(),
            engine: engine.to_owned(),
            fields,
            assertions: Vec::new(),
        }
    }

    pub(crate) fn set(&mut self, key: &str, value: Value) {
        self.fields.insert(key.to_owned(), value);
    }

    /// Record one independent assertion's outcome. A failed assertion is
    /// recorded, then the caller fails the test.
    pub(crate) fn assert(&mut self, name: &str, passed: bool, detail: impl Into<Value>) {
        self.assertions.push(json!({
            "name": name,
            "passed": passed,
            "detail": detail.into(),
        }));
    }

    /// Write the record and return its path.
    pub(crate) fn write(&self) -> PathBuf {
        let dir = evidence_root().join(&self.scenario);
        fs::create_dir_all(&dir).expect("evidence dir");
        let mut fields = self.fields.clone();
        fields.insert("assertions".into(), json!(self.assertions));
        let path = dir.join(format!("{}.json", self.engine));
        fs::write(
            &path,
            serde_json::to_vec_pretty(&Value::Object(fields)).expect("record"),
        )
        .expect("write the evidence record");
        path
    }
}

/// Copy a raw observation tree under the evidence root so a failed run
/// keeps its evidence after the case directory ages out.
pub(crate) fn keep_raw(scenario: &str, engine: &str, name: &str, source: &Path) -> Option<PathBuf> {
    let dest = evidence_root().join(scenario).join(engine).join(name);
    if source.is_dir() {
        let _ = fs::remove_dir_all(&dest);
        super::fabro_adapter::copy_tree(source, &dest).ok()?;
    } else if source.is_file() {
        fs::create_dir_all(dest.parent()?).ok()?;
        fs::copy(source, &dest).ok()?;
    } else {
        return None;
    }
    Some(dest)
}

/// The committed reference file of a scenario.
pub(crate) fn reference_path(scenario: &str) -> PathBuf {
    repo_root()
        .join("crates/fabro/acceptance/scenarios")
        .join(scenario)
        .join("fabro-reference")
        .join("reference.json")
}

/// The committed reference projection of a scenario, if any.
pub(crate) fn load_reference(scenario: &str) -> Option<(Value, Projection)> {
    let path = reference_path(scenario);
    let text = fs::read_to_string(&path).ok()?;
    let document: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not JSON: {error}", path.display()));
    assert_eq!(
        document["fabro_revision"].as_str(),
        Some(pinned_commit().as_str()),
        "{} was captured from a Fabro other than the pin",
        path.display()
    );
    let projection = from_value(&document["projection"])
        .unwrap_or_else(|error| panic!("{} holds no projection: {error}", path.display()));
    Some((document, projection))
}

/// Check the live Fabro projection against the committed reference, or
/// record it when `PETRI_FABRO_REFERENCE_RECORD=1`. Returns the differences
/// as text when the reference does not match and recording is off.
#[expect(
    clippy::print_stderr,
    reason = "a recorded reference is announced on the test's stderr"
)]
pub(crate) fn check_or_record_reference(
    scenario: &str,
    fabro: &FabroBinary,
    bundle_digest: &str,
    projection: &Projection,
    twins: &Value,
) -> Result<(), String> {
    let path = reference_path(scenario);
    let recording = env::var(RECORD_ENV).is_ok_and(|v| v == "1");
    let live = to_value(projection);
    if !recording {
        let Some((document, _)) = load_reference(scenario) else {
            return Err(format!(
                "no committed Fabro reference at {}; run with {RECORD_ENV}=1 to record one and review \
                 the diff",
                path.display()
            ));
        };
        // The identity map carries the run's raw ids and paths, which
        // differ per run by design; everything else must match.
        let mut live = live.clone();
        let mut reference = document["projection"].clone();
        for value in [&mut live, &mut reference] {
            if let Some(map) = value.as_object_mut() {
                map.remove("identities");
            }
        }
        let mut differences = json_differences(&live, &reference);
        if document["bundle_digest"].as_str() != Some(bundle_digest) {
            differences.push(format!(
                "/bundle_digest: run {bundle_digest} vs reference {}",
                document["bundle_digest"]
            ));
        }
        if differences.is_empty() {
            return Ok(());
        }
        return Err(format!(
            "the live pinned Fabro run differs from the committed reference {} ({} differences); \
             review them and rerun with {RECORD_ENV}=1 to refresh the file:\n{}",
            path.display(),
            differences.len(),
            differences.join("\n")
        ));
    }
    // The committed file carries no per-run values: the identity map keeps
    // its placeholders only (the raw run id and paths are in the evidence
    // record), and the twins are named by provider.
    let mut live = live;
    if let Some(identities) = live.get_mut("identities").and_then(Value::as_object_mut) {
        for value in identities.values_mut() {
            *value = Value::String("<per run>".to_owned());
        }
    }
    let document = json!({
        "schema_version": 1,
        "scenario": scenario,
        "fabro_repository": "https://github.com/fabro-sh/fabro",
        "fabro_revision": fabro.commit,
        "fabro_version": fabro.version,
        "captured_on": command_output("date", &["-u", "+%Y-%m-%d"]),
        "captured_by": "crates/petri/cli/tests/fabro_differential.rs through tests/support/fabro/fabro_adapter.rs",
        "bundle_digest": bundle_digest,
        "twins": twins,
        "normalization": [
            "generated run id -> <RUN_ID>",
            "the run's working directory -> <WORKSPACE>",
            "blob://sha256/<hex> -> <BLOB_n> (the dump's inline value is compared where the event log offloaded it)",
            "engine bookkeeping context keys named by the scenario are moved to `bookkeeping`, never dropped",
        ],
        "projection": live,
    });
    fs::create_dir_all(path.parent().expect("a parent")).expect("reference dir");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&document).expect("reference"),
    )
    .expect("write the reference");
    eprintln!("recorded the Fabro reference at {}", path.display());
    Ok(())
}

/// The evidence a comparison leaves: both records, the differences and the
/// decisions applied.
pub(crate) fn write_comparison(
    scenario: &str,
    differences: &[super::compare::Difference],
    fabro_source: &str,
) -> PathBuf {
    let dir = evidence_root().join(scenario);
    fs::create_dir_all(&dir).expect("evidence dir");
    let decisions: BTreeMap<String, usize> = differences
        .iter()
        .filter_map(|d| d.decision.clone())
        .fold(BTreeMap::new(), |mut map, id| {
            *map.entry(id).or_default() += 1;
            map
        });
    let document = json!({
        "schema_version": 1,
        "scenario": scenario,
        "fabro_source": fabro_source,
        "differences": differences,
        "unresolved": differences.iter().filter(|d| d.decision.is_none()).count(),
        "decisions_applied": decisions,
    });
    let path = dir.join("comparison.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&document).expect("comparison"),
    )
    .expect("write the comparison");
    path
}
