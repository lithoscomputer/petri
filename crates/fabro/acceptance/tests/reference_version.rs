//! Reference-version checks: every fixture, capture, decision and evidence
//! record that names a Fabro revision names the pin in
//! `crates/fabro/corpus-pin.txt`. CI runs this without the corpus, the
//! binary or Docker; it reads tracked files only (and evidence records when
//! a run left them).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::{env, fs};

use serde_json::Value;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("the repository root")
}

fn pin() -> String {
    let text =
        fs::read_to_string(root().join("crates/fabro/corpus-pin.txt")).expect("the pin file");
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split_whitespace().next().unwrap_or(line).to_owned())
        .expect("the pin file names a commit")
}

fn json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display()))
}

fn scenario_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root().join("crates/fabro/acceptance/scenarios"))
        .expect("the scenarios directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// The pin is a full 40-hex commit.
#[test]
fn the_pin_is_a_full_commit() {
    let pin = pin();
    assert_eq!(pin.len(), 40, "{pin}");
    assert!(pin.chars().all(|c| c.is_ascii_hexdigit()), "{pin}");
}

/// Every oracle fixture was generated at the pin.
#[test]
fn every_oracle_fixture_names_the_pin() {
    let pin = pin();
    let dir = root().join("crates/fabro/oracle/expected");
    let mut count = 0;
    for entry in fs::read_dir(&dir)
        .expect("the oracle fixtures")
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        count += 1;
        let fixture = json(&path);
        assert_eq!(
            fixture["fabro_commit"].as_str(),
            Some(pin.as_str()),
            "{} was generated at another Fabro revision",
            path.display()
        );
    }
    assert!(count > 0, "no oracle fixtures under {}", dir.display());
}

/// Every scenario's Fabro capture was taken at the pin: the differential
/// reference (`reference.json`) and the phase 2 capture (`normalized.json`)
/// where present.
#[test]
fn every_scenario_reference_names_the_pin() {
    let pin = pin();
    let mut references = 0;
    for dir in scenario_dirs() {
        let reference = dir.join("fabro-reference/reference.json");
        if reference.is_file() {
            references += 1;
            let document = json(&reference);
            assert_eq!(
                document["fabro_revision"].as_str(),
                Some(pin.as_str()),
                "{} was captured at another Fabro revision",
                reference.display()
            );
            assert!(
                document["fabro_version"]
                    .as_str()
                    .is_some_and(|v| v.contains(&pin[..7])),
                "{}: the version string does not carry the pinned short SHA",
                reference.display()
            );
            assert!(
                document["projection"].is_object() && document["bundle_digest"].is_string(),
                "{}: not a reference document",
                reference.display()
            );
        }
        let normalized = dir.join("fabro-reference/normalized.json");
        if normalized.is_file() {
            let document = json(&normalized);
            assert_eq!(
                document["provenance"]["fabro_revision"].as_str(),
                Some(pin.as_str()),
                "{} was captured at another Fabro revision",
                normalized.display()
            );
        }
    }
    assert!(references > 0, "no differential references committed");
}

/// The difference kinds the comparison emits. A decision may accept only
/// these; a typo would otherwise accept nothing silently.
const KINDS: &[&str] = &[
    "status",
    "path.skipped_stage",
    "path.branch_stage",
    "path.order",
    "fork.count",
    "fork.node",
    "branch.count",
    "branch.id",
    "branch.index",
    "branch.item_label",
    "branch.status",
    "branch.context_updates",
    "branch.context_updates.missing_in_petri",
    "branch.context_updates.missing_in_fabro",
    "branch.stages",
    "context.value",
    "context.value.missing_in_petri",
    "context.value.missing_in_fabro",
    "context.missing_in_petri",
    "context.missing_in_fabro",
    "value.trailing_newline",
    "artifact",
    "interview.count",
    "interview.node",
    "interview.kind",
    "interview.text",
    "interview.options",
    "interview.reply",
    "interview.delivery",
    "request.count",
    "request",
    "count",
];

/// Every decision record parses, carries every required field, has an id
/// equal to its file name, and accepts only known difference kinds.
#[test]
fn every_decision_record_is_complete() {
    let dir = root().join("crates/fabro/acceptance/decisions");
    let mut ids = BTreeSet::new();
    for entry in fs::read_dir(&dir)
        .expect("the decisions directory")
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let text = fs::read_to_string(&path).expect("read the record");
        let record: toml::Value =
            toml::from_str(&text).unwrap_or_else(|e| panic!("{} is not TOML: {e}", path.display()));
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        for field in [
            "id",
            "title",
            "fabro",
            "petri",
            "user_visible_effect",
            "reason",
            "acceptance",
        ] {
            assert!(
                record
                    .get(field)
                    .and_then(toml::Value::as_str)
                    .is_some_and(|s| !s.trim().is_empty()),
                "{}: `{field}` is missing or empty",
                path.display()
            );
        }
        assert_eq!(
            record["id"].as_str(),
            Some(stem),
            "{}: the id must equal the file name",
            path.display()
        );
        assert!(ids.insert(stem.to_owned()), "duplicate decision id {stem}");
        for key in ["scenarios", "bundles"] {
            assert!(
                record.get(key).and_then(toml::Value::as_array).is_some(),
                "{}: `{key}` must be a list",
                path.display()
            );
        }
        if let Some(migration) = record.get("migration") {
            for key in ["old_bundle", "new_bundle"] {
                assert!(
                    migration.get(key).and_then(toml::Value::as_str).is_some(),
                    "{}: a migration names `{key}`",
                    path.display()
                );
            }
        }
        for accepts in record
            .get("accepts")
            .and_then(toml::Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let kind = accepts["kind"].as_str().unwrap_or_default();
            assert!(
                KINDS.contains(&kind),
                "{}: accepts an unknown difference kind `{kind}`",
                path.display()
            );
        }
        // A known defect of the pinned Fabro is an exact assertion name in
        // exactly named scenarios: the differential cell and the coverage
        // report both read it, so a loose entry could excuse too much.
        if let Some(defects) = record.get("known_defects") {
            let defects = defects
                .as_array()
                .unwrap_or_else(|| panic!("{}: `known_defects` must be a list", path.display()));
            for defect in defects {
                assert!(
                    defect.as_str().is_some_and(|name| !name.trim().is_empty()),
                    "{}: a known defect is an assertion name",
                    path.display()
                );
            }
            if !defects.is_empty() {
                let scenarios = record["scenarios"].as_array().cloned().unwrap_or_default();
                assert!(
                    !scenarios.is_empty()
                        && scenarios
                            .iter()
                            .all(|s| s.as_str().is_some_and(|s| !s.contains('*'))),
                    "{}: a record with known defects names its scenarios exactly (no glob)",
                    path.display()
                );
            }
        }
    }
    assert!(
        !ids.is_empty(),
        "no decision records under {}",
        dir.display()
    );
}

/// Evidence records a differential run left (under `PETRI_EVIDENCE_DIR` or
/// `target/fabro-differential`) were produced by the pinned Fabro.
#[test]
#[expect(
    clippy::print_stderr,
    reason = "an absent evidence directory is reported, not failed"
)]
fn evidence_records_name_the_pin() {
    let pin = pin();
    let dir = env::var_os("PETRI_EVIDENCE_DIR")
        .map_or_else(|| root().join("target/fabro-differential"), PathBuf::from);
    let Ok(entries) = fs::read_dir(&dir) else {
        eprintln!(
            "no evidence records under {} (nothing to check)",
            dir.display()
        );
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let record = entry.path().join("fabro.json");
        if !record.is_file() {
            continue;
        }
        let document = json(&record);
        assert_eq!(
            document["pins"]["fabro"]["pin"].as_str(),
            Some(pin.as_str()),
            "{}: pin mismatch",
            record.display()
        );
        if let Some(commit) = document["pins"]["fabro"]["commit"].as_str() {
            assert_eq!(
                commit,
                pin,
                "{}: captured from another Fabro",
                record.display()
            );
        }
    }
}

/// The provisioning script exists and refuses PATH's `fabro`: it is the
/// only way the harness obtains a reference binary.
#[test]
fn the_provisioning_script_is_present() {
    let script = root().join("scripts/fabro-provision.sh");
    let text = fs::read_to_string(&script).expect("scripts/fabro-provision.sh");
    assert!(text.contains("corpus-pin.txt"));
    assert!(text.contains("--version"));
    assert!(
        text.contains("never used") || text.contains("never taken"),
        "the script must say PATH's fabro is never used"
    );
}
