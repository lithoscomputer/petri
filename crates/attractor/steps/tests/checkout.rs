//! The root `start` stage's checkout on the host executor: a repository with
//! more history than the clone's depth, checked out whole, as a cone of one
//! directory, and refused when a cone entry names no directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use attractor_steps::checkout::EVENT_KIND;
use attractor_steps::register;
use frontend::Diagnostics;
use frontend_attractor::{CloneSettings, RunSettings, lower};
use runtime::driver::ExecutionReport;
use runtime::engine::Event;
use runtime::executor::Retention;
use runtime::frontend::{CompileInputs, NoFiles, REPOSITORY_VAR};
use runtime::ir::{RunStatus, Status, StepEvent};
use runtime::{RunOptions, Runtime};
use serde_json::{Value, json};
use testkit::RunDir;

/// More commits than [`DEPTH`], so the checkout is shallow.
const COMMITS: usize = 5;
const DEPTH: i64 = 3;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository with a root file, `inside/` (with a nested directory) and
/// `outside/`, and [`COMMITS`] commits.
fn repository(dir: &RunDir) -> PathBuf {
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join("inside/deep")).expect("creates the repository");
    fs::create_dir_all(repo.join("outside")).expect("creates the repository");
    fs::write(repo.join("inside/a.txt"), "a\n").expect("writes");
    fs::write(repo.join("inside/deep/d.txt"), "d\n").expect("writes");
    fs::write(repo.join("outside/b.txt"), "b\n").expect("writes");
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    for commit in 0..COMMITS {
        fs::write(repo.join("README"), format!("{commit}\n")).expect("writes");
        git(&repo, &["add", "-A"]);
        git(&repo, &[
            "-c",
            "user.name=petri",
            "-c",
            "user.email=petri@example.com",
            "commit",
            "--quiet",
            "-m",
            &format!("commit {commit}"),
        ]);
    }
    repo
}

/// Run `start -> exit` with the repository bound and `sparse` as the cone,
/// keeping the workspace for inspection.
async fn check_out(dir: &RunDir, repo: &Path, sparse: &[&str]) -> ExecutionReport {
    let settings = RunSettings {
        clone: CloneSettings {
            depth: DEPTH,
            sparse: sparse.iter().map(|s| (*s).to_owned()).collect(),
            ..CloneSettings::default()
        },
        ..RunSettings::default()
    };
    let inputs = CompileInputs::new().with_var(REPOSITORY_VAR, repo.to_string_lossy().into_owned());
    let lowered = lower(
        "test.fabro",
        "digraph T {\n  start [shape=Mdiamond]\n  exit [shape=Msquare]\n  start -> exit\n}",
        &NoFiles,
        &inputs,
        settings,
        Diagnostics::new(),
    );
    let graph = lowered.graph.expect("lowers");
    let mut options = RunOptions::new(dir.path());
    options.grace = Duration::from_secs(2);
    options.retention = Retention::Always;
    options.echo = false;
    register(Runtime::standard())
        .options(options)
        .run(graph)
        .await
        .expect("replay is byte-identical")
}

fn checkout_event(report: &ExecutionReport) -> Value {
    report
        .state
        .log
        .events()
        .find_map(|e| match e {
            Event::StepProgressRecorded {
                ev: StepEvent::Custom(value),
                ..
            } if value["kind"] == EVENT_KIND => Some(value.clone()),
            _ => None,
        })
        .expect("the checkout is recorded")
}

/// The message the first failed step finished with.
fn failure_message(report: &ExecutionReport) -> String {
    report
        .state
        .log
        .events()
        .find_map(|e| match e {
            Event::StepFinished { outcome, .. } => match &outcome.status {
                Status::Failure(failure) => Some(failure.message.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("a step failed")
}

/// The work tree's files, `.git` aside, sorted and relative to its root.
fn work_tree_files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).expect("reads the workspace") {
            let path = entry.expect("reads the workspace").path();
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative = path.strip_prefix(root).expect("under the root");
                out.push(relative.to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// A cone of `inside` seeds only that directory and the root's files. The
/// cone travels in `.git`, so the extracted workspace is itself a sparse,
/// shallow, clean checkout of the same commit.
#[tokio::test]
async fn a_sparse_checkout_seeds_only_the_cone_and_the_root_files() {
    let dir = RunDir::new("checkout-sparse");
    let repo = repository(&dir);
    let report = check_out(&dir, &repo, &["inside/"]).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let workspace = dir.workspace();
    assert_eq!(work_tree_files(&workspace), [
        "README",
        "inside/a.txt",
        "inside/deep/d.txt"
    ]);
    assert!(
        workspace.join(".git").is_dir(),
        "the clone's `.git` is there"
    );
    assert_eq!(git(&workspace, &["sparse-checkout", "list"]), "inside\n");
    assert_eq!(
        git(&workspace, &["config", "--bool", "core.sparseCheckoutCone"]),
        "true\n"
    );
    assert_eq!(
        git(&workspace, &["status", "--porcelain"]),
        "",
        "the cone hides `outside/` without deleting it"
    );
    assert_eq!(
        git(&workspace, &["rev-parse", "--is-shallow-repository"]),
        "true\n"
    );
    assert_eq!(
        git(&workspace, &["rev-list", "--count", "HEAD"]),
        format!("{DEPTH}\n")
    );
    assert_eq!(
        git(&workspace, &["rev-parse", "HEAD"]),
        git(&repo, &["rev-parse", "HEAD"])
    );

    let event = checkout_event(&report);
    assert_eq!(
        event["sparse"],
        json!(["inside"]),
        "the entry is normalized"
    );
    assert_eq!(event["depth"], json!(DEPTH));
}

/// No cone is a full checkout, as before: every file, no sparse config.
#[tokio::test]
async fn an_empty_cone_is_a_full_checkout() {
    let dir = RunDir::new("checkout-full");
    let repo = repository(&dir);
    let report = check_out(&dir, &repo, &[]).await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let workspace = dir.workspace();
    assert_eq!(work_tree_files(&workspace), [
        "README",
        "inside/a.txt",
        "inside/deep/d.txt",
        "outside/b.txt"
    ]);
    assert!(
        !workspace.join(".git/info/sparse-checkout").exists(),
        "a full checkout carries no cone"
    );
    assert_eq!(git(&workspace, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&workspace, &["rev-parse", "--is-shallow-repository"]),
        "true\n"
    );
    assert_eq!(checkout_event(&report)["sparse"], json!([]));
}

/// An entry that names no single directory fails the checkout before
/// anything is cloned, and the run with it.
#[tokio::test]
async fn an_invalid_cone_entry_fails_the_checkout() {
    for entry in ["", "/abs", "a/../b", "./inside", "a//b", "src/*", "!inside"] {
        let dir = RunDir::new("checkout-invalid");
        let repo = repository(&dir);
        let report = check_out(&dir, &repo, &[entry]).await;
        assert_eq!(report.status, RunStatus::Failed, "{entry:?} is refused");
        let failure = failure_message(&report);
        assert!(
            failure.starts_with("checkout: [run.clone] sparse entry"),
            "{entry:?} names the entry: {failure}"
        );
        assert!(
            !dir.workspace().join(".git").exists(),
            "{entry:?}: nothing is checked out"
        );
    }
}
