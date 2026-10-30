//! A GitHub Actions workflow named the way a user types it from the
//! repository root: a relative path, and a relative `--repo`. The local
//! checkout still finds the repository and copies its tree into the job.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use testkit::RunDir;

const WORKFLOW: &str = "on: push\n\
                        jobs:\n\
                        \x20 probe:\n\
                        \x20   runs-on: ubuntu-latest\n\
                        \x20   steps:\n\
                        \x20     - uses: actions/checkout@v4\n\
                        \x20     - run: cat marker.txt\n";

fn git_in(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "petri")
        .env("GIT_AUTHOR_EMAIL", "petri@test")
        .env("GIT_COMMITTER_NAME", "petri")
        .env("GIT_COMMITTER_EMAIL", "petri@test")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A committed repository holding the workflow and a marker file, beside
/// the run directory rather than inside it.
fn repository(dir: &RunDir) -> PathBuf {
    let repo = dir.path().join("repo");
    fs::create_dir_all(repo.join(".github/workflows")).expect("create the workflow dir");
    fs::write(repo.join(".github/workflows/ci.yml"), WORKFLOW).expect("write the workflow");
    fs::write(repo.join("marker.txt"), "checked-out-marker\n").expect("write the marker");
    git_in(&repo, &["init", "--quiet", "-b", "main"]);
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "--quiet", "-m", "fixture"]);
    repo
}

fn run_from(repo: &Path, run_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_petri"))
        .current_dir(repo)
        .arg("run")
        .arg("--run-dir")
        .arg(run_dir)
        .args(args)
        .output()
        .expect("petri runs")
}

fn assert_checked_out(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the run succeeds:\n{stdout}\n{stderr}"
    );
    // Step output streams to stderr beside the run's progress.
    assert!(
        stderr.contains("checked-out-marker"),
        "the job read the checked-out file:\n{stdout}\n{stderr}"
    );
}

#[test]
fn a_relative_workflow_path_checks_out_the_repository() {
    let dir = RunDir::new("gha-cli-relative-file");
    let repo = repository(&dir);
    let output = run_from(&repo, &dir.path().join("run"), &[
        ".github/workflows/ci.yml",
    ]);
    assert_checked_out(&output);
}

#[test]
fn a_relative_repo_flag_checks_out_the_repository() {
    let dir = RunDir::new("gha-cli-relative-repo");
    let repo = repository(&dir);
    let output = run_from(&repo, &dir.path().join("run"), &[
        "--repo",
        ".",
        "./.github/workflows/ci.yml",
    ]);
    assert_checked_out(&output);
}
