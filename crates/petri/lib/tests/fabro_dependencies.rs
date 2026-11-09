//! Petri never depends on a Fabro crate.
//!
//! Fabro embeds Petri, so the production dependency direction is Fabro to
//! Petri. Petri's own component crates (`petri-fabro-*`, `petri-attractor-*`,
//! `petri-frontend-attractor` and `petri-frontend-fabro`) are allowed. A crate
//! of the fabro-sh/fabro repository (`fabro-*`) is not, whether it is reached
//! by a path, by git, transitively through another crate, in any dependency
//! kind, or from a separate Cargo workspace inside this repository. The check
//! reads the full `cargo metadata` resolve graph, every package and every
//! dependency kind, and separately scans nested manifests and build scripts
//! that the main workspace does not see. Only testing and parity harness code
//! may launch a pinned Fabro binary as a subprocess; nothing may link it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, process};

use serde_json::{Value, json};

/// The fetched Fabro checkout: data for the compatibility tests, never a
/// build input.
const CORPUS_MARKER: &str = "crates/fabro/corpus/";
const FABRO_REPOSITORY: &str = "fabro-sh/fabro";

fn is_fabro_name(name: &str) -> bool {
    name == "fabro" || name.starts_with("fabro-")
}

fn is_petri_name(name: &str) -> bool {
    name == "petri" || name.starts_with("petri-")
}

fn mentions_fabro_source(text: &str) -> bool {
    text.contains(FABRO_REPOSITORY) || text.contains(CORPUS_MARKER)
}

fn text_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A resolved package that is a Fabro crate: by name, by source identity, or
/// by where its manifest lives.
fn package_is_fabro(package: &Value) -> bool {
    let name = text_of(package, "name");
    if is_petri_name(name) {
        return false;
    }
    is_fabro_name(name)
        || mentions_fabro_source(text_of(package, "id"))
        || mentions_fabro_source(text_of(package, "source"))
        || mentions_fabro_source(text_of(package, "manifest_path"))
}

fn describe(package: &Value) -> String {
    let source = package
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("path");
    format!(
        "{} (source: {source}, manifest: {})",
        text_of(package, "name"),
        text_of(package, "manifest_path")
    )
}

fn kind_name(kind: Option<&Value>) -> &str {
    kind.and_then(Value::as_str).unwrap_or("normal")
}

/// Every way the `cargo metadata` document reaches a Fabro crate, one line
/// per finding. Empty means the graph is clean.
fn forbidden_fabro_packages(metadata: &Value) -> Vec<String> {
    let empty = Vec::new();
    let packages = metadata["packages"].as_array().unwrap_or(&empty);
    let by_id: HashMap<&str, &Value> = packages
        .iter()
        .map(|package| (text_of(package, "id"), package))
        .collect();
    let mut findings = Vec::new();
    for package in packages {
        if package_is_fabro(package) {
            findings.push(format!("package {}", describe(package)));
        }
        // Manifest declarations, resolved or not: a direct dependency on a
        // Fabro crate is a violation even before cargo resolves it.
        for dependency in package["dependencies"].as_array().unwrap_or(&empty) {
            let name = text_of(dependency, "name");
            let path = text_of(dependency, "path");
            let source = text_of(dependency, "source");
            let by_name = is_fabro_name(name) && !is_petri_name(name);
            if by_name || mentions_fabro_source(path) || mentions_fabro_source(source) {
                findings.push(format!(
                    "{} declares {name} ({} dependency; source: {}, path: {})",
                    text_of(package, "name"),
                    kind_name(dependency.get("kind")),
                    if source.is_empty() { "path" } else { source },
                    if path.is_empty() { "-" } else { path },
                ));
            }
        }
    }
    // The resolve graph: transitive edges, in every dependency kind.
    for node in metadata["resolve"]["nodes"].as_array().unwrap_or(&empty) {
        let from = by_id
            .get(text_of(node, "id"))
            .map_or_else(|| text_of(node, "id"), |package| text_of(package, "name"));
        for dependency in node["deps"].as_array().unwrap_or(&empty) {
            let target_id = text_of(dependency, "pkg");
            let target = by_id.get(target_id);
            let forbidden = target.map_or_else(
                || mentions_fabro_source(target_id),
                |package| package_is_fabro(package),
            );
            if !forbidden {
                continue;
            }
            let target_name = target.map_or(target_id, |package| text_of(package, "name"));
            for kind in dependency["dep_kinds"].as_array().unwrap_or(&empty) {
                findings.push(format!(
                    "{from} -> {target_name} ({}) resolves to a Fabro crate",
                    kind_name(kind.get("kind"))
                ));
            }
        }
    }
    findings
}

/// Is this line of a Cargo manifest a dependency on a Fabro crate? A
/// workspace alias (`fabro-steps = { workspace = true }`) and a renamed Petri
/// package (`package = "petri-fabro-acceptance"`) are Petri's own crates.
fn manifest_line_reaches_fabro(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with('#') {
        return false;
    }
    let names_a_fabro_key = trimmed
        .split_once('=')
        .is_some_and(|(key, _)| is_fabro_name(key.trim().trim_matches('"')))
        || (trimmed.starts_with('[') && trimmed.contains("dependencies.fabro"));
    let is_petri_alias =
        trimmed.contains("workspace = true") || trimmed.contains("package = \"petri-");
    (names_a_fabro_key && !is_petri_alias)
        || trimmed.contains("package = \"fabro")
        || mentions_fabro_source(trimmed)
}

fn skip_dir(path: &Path, root: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if matches!(name, "target" | ".git" | ".ai" | "node_modules") {
        return true;
    }
    // A nested git checkout is not this repository's build input: a worktree
    // under `.claude/worktrees/` (with its own fetched corpus) or the Fabro
    // corpus checkout itself. `.git` is a directory in a plain checkout and a
    // file in a worktree.
    if path != root && path.join(".git").exists() {
        return true;
    }
    // Test data, never a build input: the Fabro corpus checkout.
    path.strip_prefix(root)
        .is_ok_and(|rel| rel.starts_with("crates/fabro/corpus"))
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !skip_dir(&path, root) {
                walk(&path, root, out);
            }
        } else if path
            .file_name()
            .is_some_and(|n| n == "Cargo.toml" || n == "build.rs")
        {
            out.push(path);
        }
    }
}

/// Every nested Cargo manifest line and build script line under `root` that
/// reaches a Fabro crate, outside `target/`, `.git/`, `.ai/`, and the fetched
/// corpus. This catches a separate Cargo
/// workspace inside the repository, which the main `cargo metadata` never sees.
fn forbidden_fabro_manifests(root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort();
    let mut findings = Vec::new();
    for file in files {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let is_manifest = file.file_name().is_some_and(|n| n == "Cargo.toml");
        for (index, line) in text.lines().enumerate() {
            let hit = if is_manifest {
                manifest_line_reaches_fabro(line)
            } else {
                mentions_fabro_source(line)
            };
            if hit {
                findings.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(root).unwrap_or(&file).display(),
                    index + 1,
                    line.trim()
                ));
            }
        }
    }
    findings
}

fn real_metadata() -> Value {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata is JSON")
}

fn package(name: &str, id: &str, source: Option<&str>, manifest: &str, deps: &Value) -> Value {
    json!({
        "name": name,
        "id": id,
        "source": source,
        "manifest_path": manifest,
        "dependencies": deps,
    })
}

fn node(id: &str, deps: &Value) -> Value {
    json!({ "id": id, "deps": deps })
}

fn edge(name: &str, pkg: &str, kind: Option<&str>) -> Value {
    json!({ "name": name, "pkg": pkg, "dep_kinds": [{ "kind": kind, "target": null }] })
}

const PETRI_ROOT: &str = "/repo";
const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

fn petri_package(name: &str, deps: &Value) -> Value {
    package(
        name,
        &format!("path+file://{PETRI_ROOT}/crates/x/{name}#{name}@0.1.0"),
        None,
        &format!("{PETRI_ROOT}/crates/x/{name}/Cargo.toml"),
        deps,
    )
}

fn petri_id(name: &str) -> String {
    format!("path+file://{PETRI_ROOT}/crates/x/{name}#{name}@0.1.0")
}

/// The checker sees a direct path dependency into the fetched corpus.
#[test]
fn a_direct_path_dependency_on_a_corpus_crate_is_caught() {
    let corpus = format!("{PETRI_ROOT}/crates/fabro/corpus/fabro/lib/components/fabro-workflow");
    let metadata = json!({
        "packages": [
            petri_package("petri-fabro-acceptance", &json!([
                { "name": "fabro-workflow", "source": null, "req": "*", "kind": null, "path": corpus }
            ])),
            package(
                "fabro-workflow",
                &format!("path+file://{corpus}#fabro-workflow@0.347.0"),
                None,
                &format!("{corpus}/Cargo.toml"),
                &json!([]),
            ),
        ],
        "resolve": { "nodes": [
            node(&petri_id("petri-fabro-acceptance"), &json!([
                edge("attractor.workflow", &format!("path+file://{corpus}#fabro-workflow@0.347.0"), None)
            ])),
        ] },
    });
    let findings = forbidden_fabro_packages(&metadata);
    assert!(
        findings
            .iter()
            .any(|f| f.starts_with("package fabro-workflow")),
        "{findings:#?}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f.contains("petri-fabro-acceptance declares fabro-workflow (normal")),
        "{findings:#?}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f
                == "petri-fabro-acceptance -> fabro-workflow (normal) resolves to a Fabro crate"),
        "{findings:#?}"
    );
}

/// The checker sees a git dependency on the Fabro repository.
#[test]
fn a_git_dependency_on_the_fabro_repository_is_caught() {
    let git_id = "git+https://github.com/fabro-sh/fabro?rev=abc#fabro-types@0.347.0";
    let metadata = json!({
        "packages": [
            petri_package("petri-fabro-steps", &json!([
                { "name": "fabro-types", "source": "git+https://github.com/fabro-sh/fabro?rev=abc", "req": "*", "kind": null }
            ])),
            package(
                "fabro-types",
                git_id,
                Some("git+https://github.com/fabro-sh/fabro?rev=abc#abc"),
                "/home/u/.cargo/git/checkouts/fabro-1/abc/lib/foundation/fabro-types/Cargo.toml",
                &json!([]),
            ),
        ],
        "resolve": { "nodes": [
            node(&petri_id("petri-fabro-steps"), &json!([edge("fabro_types", git_id, None)])),
        ] },
    });
    let findings = forbidden_fabro_packages(&metadata);
    assert!(
        findings
            .iter()
            .any(|f| f
                .starts_with("package fabro-types (source: git+https://github.com/fabro-sh/fabro")),
        "{findings:#?}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f == "petri-fabro-steps -> fabro-types (normal) resolves to a Fabro crate"),
        "{findings:#?}"
    );
}

/// The checker sees a Fabro crate reached only through another crate.
#[test]
fn a_transitive_dependency_is_caught_through_the_resolve_graph() {
    let some_id = format!("{REGISTRY}#some-crate@1.0.0");
    let fabro_id = format!("{REGISTRY}#fabro-core@0.347.0");
    let metadata = json!({
        "packages": [
            petri_package("petri-x", &json!([
                { "name": "some-crate", "source": REGISTRY, "req": "^1", "kind": null }
            ])),
            package("some-crate", &some_id, Some(REGISTRY), "/home/u/.cargo/registry/src/some-crate-1.0.0/Cargo.toml", &json!([
                { "name": "fabro-core", "source": REGISTRY, "req": "*", "kind": null }
            ])),
            package("fabro-core", &fabro_id, Some(REGISTRY), "/home/u/.cargo/registry/src/fabro-core-0.347.0/Cargo.toml", &json!([])),
        ],
        "resolve": { "nodes": [
            node(&petri_id("petri-x"), &json!([edge("some_crate", &some_id, None)])),
            node(&some_id, &json!([edge("fabro_core", &fabro_id, None)])),
            node(&fabro_id, &json!([])),
        ] },
    });
    let findings = forbidden_fabro_packages(&metadata);
    assert!(
        findings
            .iter()
            .any(|f| f == "some-crate -> fabro-core (normal) resolves to a Fabro crate"),
        "{findings:#?}"
    );
    assert!(
        !findings.iter().any(|f| f.starts_with("petri-x declares")),
        "petri-x never names the crate; the edge is what the graph shows: {findings:#?}"
    );
}

/// The checker sees dev-only and build-only dependencies too.
#[test]
fn dev_and_build_dependencies_are_caught() {
    let acp_id = format!("{REGISTRY}#fabro-acp@0.347.0");
    let support_id = format!("{REGISTRY}#fabro-build-support@0.347.0");
    let metadata = json!({
        "packages": [
            petri_package("petri-y", &json!([
                { "name": "fabro-acp", "source": REGISTRY, "req": "*", "kind": "dev" },
                { "name": "fabro-build-support", "source": REGISTRY, "req": "*", "kind": "build" }
            ])),
            package("fabro-acp", &acp_id, Some(REGISTRY), "/r/fabro-acp/Cargo.toml", &json!([])),
            package("fabro-build-support", &support_id, Some(REGISTRY), "/r/fabro-build-support/Cargo.toml", &json!([])),
        ],
        "resolve": { "nodes": [
            node(&petri_id("petri-y"), &json!([
                edge("fabro_acp", &acp_id, Some("dev")),
                edge("fabro_build_support", &support_id, Some("build")),
            ])),
        ] },
    });
    let findings = forbidden_fabro_packages(&metadata);
    assert!(
        findings
            .iter()
            .any(|f| f == "petri-y -> fabro-acp (dev) resolves to a Fabro crate"),
        "{findings:#?}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f == "petri-y -> fabro-build-support (build) resolves to a Fabro crate"),
        "{findings:#?}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f.contains("declares fabro-acp (dev dependency")),
        "{findings:#?}"
    );
}

/// Petri's own Fabro component crates and its other git dependencies pass.
#[test]
fn petri_fabro_crates_and_other_dependencies_are_allowed() {
    let pebble_id =
        "git+ssh://git@github.com/lithoscomputer/pebble.git?rev=a2f#pebble-coding-agent@0.1.0";
    let driver_id =
        "git+ssh://git@github.com/lithoscomputer/sandbox-driver.git?rev=a56#sandbox-driver@0.1.0";
    let metadata = json!({
        "packages": [
            petri_package("petri-attractor-steps", &json!([
                { "name": "pebble-coding-agent", "source": "git+ssh://git@github.com/lithoscomputer/pebble.git?rev=a2f", "req": "*", "kind": null },
                { "name": "frontend-attractor", "source": null, "req": "*", "kind": null, "path": format!("{PETRI_ROOT}/crates/x/petri-frontend-attractor") }
            ])),
            petri_package("petri-frontend-attractor", &json!([])),
            // A renamed dependency: cargo records the package name and the
            // alias separately.
            petri_package("petri-fabro-acceptance", &json!([
                { "name": "petri-attractor-steps", "rename": "attractor-steps", "source": null, "req": "*", "kind": "dev", "path": format!("{PETRI_ROOT}/crates/x/petri-attractor-steps") }
            ])),
            package("pebble-coding-agent", pebble_id, Some("git+ssh://git@github.com/lithoscomputer/pebble.git?rev=a2f#a2f"), "/home/u/.cargo/git/checkouts/pebble/a2f/Cargo.toml", &json!([])),
            package("sandbox-driver", driver_id, Some("git+ssh://git@github.com/lithoscomputer/sandbox-driver.git?rev=a56#a56"), "/home/u/.cargo/git/checkouts/sandbox-driver/a56/Cargo.toml", &json!([])),
        ],
        "resolve": { "nodes": [
            node(&petri_id("petri-attractor-steps"), &json!([
                edge("pebble_coding_agent", pebble_id, None),
                edge("frontend_attractor", &petri_id("petri-frontend-attractor"), None),
            ])),
            node(&petri_id("petri-fabro-acceptance"), &json!([
                edge("attractor_steps", &petri_id("petri-attractor-steps"), Some("dev")),
            ])),
            node(pebble_id, &json!([])),
            node(driver_id, &json!([])),
        ] },
    });
    assert_eq!(forbidden_fabro_packages(&metadata), Vec::<String>::new());
}

/// A separate Cargo workspace or a build script inside the repository that
/// reaches Fabro is caught, and the fetched corpus itself is skipped.
#[test]
fn nested_manifests_and_build_scripts_are_scanned_and_the_corpus_is_skipped() {
    let root = env::temp_dir().join(format!("petri-fabro-deps-{}", process::id()));
    let _ = fs::remove_dir_all(&root);
    let write = |rel: &str, text: &str| {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        fs::write(path, text).expect("write");
    };
    write(
        "tool/Cargo.toml",
        "[package]\nname = \"tool\"\n\n[workspace]\n\n[dependencies]\nfabro-workflow = { path = \"../crates/fabro/corpus/fabro/lib/components/fabro-workflow\" }\nserde = \"1\"\n",
    );
    write(
        "tool/build.rs",
        "fn main() {\n    println!(\"cargo:rerun-if-changed=../crates/fabro/corpus/fabro\");\n}\n",
    );
    write(
        "crates/x/Cargo.toml",
        "[dependencies]\npetri-fabro-steps = { workspace = true }\nfabro-steps = { workspace = true }\nfrontend-fabro = { package = \"petri-frontend-fabro\", path = \"../fabro/frontend\" }\n",
    );
    write(
        "crates/fabro/corpus/fabro/Cargo.toml",
        "[dependencies]\nfabro-core = { path = \"lib/foundation/fabro-core\" }\n",
    );
    write("target/debug/build/x/Cargo.toml", "fabro-core = \"1\"\n");
    // A nested worktree is its own checkout (`.git` is a file there) and
    // carries its own fetched corpus; the scan must not enter it.
    write(
        ".claude/worktrees/wt/.git",
        "gitdir: /elsewhere/.git/worktrees/wt\n",
    );
    write(
        ".claude/worktrees/wt/crates/fabro/corpus/fabro/Cargo.toml",
        "fabro-core = { path = \"lib/foundation/fabro-core\" }\n",
    );
    write(
        ".claude/worktrees/wt/tools/gen/Cargo.toml",
        "fabro-workflow = { path = \"../../crates/fabro/corpus/fabro/lib/components/fabro-workflow\" }\n",
    );
    let findings = forbidden_fabro_manifests(&root);
    let _ = fs::remove_dir_all(&root);
    assert_eq!(findings.len(), 2, "{findings:#?}");
    assert!(
        findings[0].starts_with("tool/Cargo.toml:7: fabro-workflow"),
        "{findings:#?}"
    );
    assert!(findings[1].starts_with("tool/build.rs:2:"), "{findings:#?}");
}

/// Manifest lines that name a Fabro crate are told apart from Petri's own
/// aliases.
#[test]
fn manifest_lines_are_classified() {
    assert!(manifest_line_reaches_fabro("fabro-workflow = \"1\""));
    assert!(manifest_line_reaches_fabro(
        "fabro = { git = \"https://github.com/fabro-sh/fabro\" }"
    ));
    assert!(manifest_line_reaches_fabro("[dev-dependencies.fabro-acp]"));
    assert!(manifest_line_reaches_fabro(
        "x = { package = \"fabro-types\", version = \"1\" }"
    ));
    assert!(!manifest_line_reaches_fabro(
        "fabro-steps = { workspace = true }"
    ));
    assert!(!manifest_line_reaches_fabro(
        "fabro-steps = { package = \"petri-fabro-steps\", path = \"crates/attractor/steps\" }"
    ));
    assert!(!manifest_line_reaches_fabro(
        "frontend-fabro = { workspace = true }"
    ));
    assert!(!manifest_line_reaches_fabro("# fabro-workflow was removed"));
}

/// The real workspace: no package, declaration, or resolve edge reaches a
/// Fabro crate, while Petri's own `petri-fabro-*` crates are present.
#[test]
fn petri_never_depends_on_a_fabro_crate() {
    let metadata = real_metadata();
    let findings = forbidden_fabro_packages(&metadata);
    assert!(
        findings.is_empty(),
        "Fabro crates in Petri's dependency graph:\n  {}",
        findings.join("\n  ")
    );
    let petri_fabro = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .filter(|package| {
            let name = text_of(package, "name");
            name.starts_with("petri-fabro-") || name.starts_with("petri-attractor-")
        })
        .count();
    assert!(
        petri_fabro >= 2,
        "the allow-list is exercised by Petri's own Fabro and Attractor crates, saw {petri_fabro}"
    );
}

/// The real repository: no nested manifest or build script reaches Fabro.
#[test]
fn no_nested_manifest_or_build_script_reaches_fabro() {
    let metadata = real_metadata();
    let root = PathBuf::from(metadata["workspace_root"].as_str().expect("workspace_root"));
    let findings = forbidden_fabro_manifests(&root);
    assert!(
        findings.is_empty(),
        "manifests or build scripts reach Fabro:\n  {}",
        findings.join("\n  ")
    );
}
