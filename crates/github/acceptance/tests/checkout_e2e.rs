//! Local checkout, end to end: a substituted `actions/checkout` materializes
//! the workspace from a real local repository — committed HEAD, the dirty
//! tracked edit, the untracked file, never the ignored one, `.git` and mode
//! bits included — in a host job and in a containerized job. Fully offline:
//! no action source is configured at all.

mod support;

use std::path::PathBuf;
use std::{env, fs, process};

use acceptance::runs::RUNNER_IMAGE_2404;
use runtime::ir::RunStatus;
use serde_json::json;
use support::*;

/// A real repository: a committed tree (a file, an executable script, a
/// .gitignore), a dirty tracked edit, an untracked file, an ignored file.
struct Fixture(PathBuf);

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = env::temp_dir()
            .join("petri-checkout-fixture")
            .join(format!("{label}-{}", process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the fixture dir");
        fs::write(dir.join("a.txt"), "committed\n").expect("write");
        fs::write(dir.join(".gitignore"), "*.log\n").expect("write");
        fs::write(dir.join("script.sh"), "#!/bin/sh\necho ran-script\n").expect("write");
        #[cfg(unix)]
        {
            use std::fs::Permissions;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join("script.sh"), Permissions::from_mode(0o755))
                .expect("chmod");
        }
        commit_fixture(&dir);
        // The tree you have: a dirty tracked edit, an untracked file, an
        // ignored file that must never travel.
        fs::write(dir.join("a.txt"), "dirty-edit\n").expect("write");
        fs::write(dir.join("b.txt"), "untracked\n").expect("write");
        fs::write(dir.join("noise.log"), "ignored bulk\n").expect("write");
        Self(dir)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn checkout_workflow(container: Option<&str>) -> String {
    let container = container.map_or(String::new(), |image| format!("    container: {image}\n"));
    format!(
        "on: push\n\
         jobs:\n\
         \x20 probe:\n\
         \x20   runs-on: ubuntu-latest\n\
         {container}\
         \x20   steps:\n\
         \x20     - uses: actions/checkout@v4\n\
         \x20     - run: cat a.txt\n\
         \x20     - run: cat b.txt\n\
         \x20     - run: test ! -e noise.log && echo ignored-stayed-home\n\
         \x20     - run: ./script.sh\n\
         \x20     - run: git rev-parse --short HEAD && echo git-works\n\
         \x20     - run: test \"$(git rev-parse --abbrev-ref HEAD)\" = main && echo on-run-branch\n\
         \x20     - run: test \"$(git rev-parse origin/main)\" = \"$(git rev-parse HEAD)\" && echo tracking-ref-set\n\
         \x20     - run: test \"$(git remote get-url origin)\" = https://github.com/example/repo && echo origin-names-repo\n\
         \x20 pathed:\n\
         \x20   runs-on: ubuntu-latest\n\
         {container}\
         \x20   steps:\n\
         \x20     - uses: actions/checkout@v4\n\
         \x20       with:\n\
         \x20         path: nested/copy\n\
         \x20     - run: cat nested/copy/a.txt\n"
    )
}

async fn run_checkout(label: &str, container: Option<&str>) {
    let fixture = Fixture::new(label);
    let mut graph = lower_ok(&checkout_workflow(container));
    graph.params.insert(
        "petri".into(),
        json!({ "repo": fixture.0.display().to_string() }),
    );
    let report = run_host(graph, label).await;
    let lines = log_lines(&report);
    assert_eq!(
        report.status,
        RunStatus::Success,
        "statuses: {:?}\nlog: {lines:?}",
        report
            .state
            .history()
            .iter()
            .map(|r| (r.name.to_string(), r.outcome.status.tag()))
            .collect::<Vec<_>>()
    );
    let has = |needle: &str| lines.iter().any(|l| l == needle);
    assert!(
        has("dirty-edit"),
        "the dirty tracked edit travels: {lines:?}"
    );
    assert!(has("untracked"), "the untracked file travels");
    assert!(has("ignored-stayed-home"), "the ignored file does not");
    assert!(has("ran-script"), "mode bits survive");
    assert!(has("git-works"), "`.git` rides along");
    assert!(has("on-run-branch"), "the run's branch is checked out");
    assert!(has("tracking-ref-set"), "origin/<branch> matches HEAD");
    assert!(has("origin-names-repo"), "origin is the repository URL");
}

/// Host job, no action source configured — the substitution is offline.
#[tokio::test(flavor = "multi_thread")]
async fn local_checkout_materializes_the_tree_you_have() {
    if !is_tool_ready("git") {
        return;
    }
    run_checkout("host", None).await;
}

/// The same tree in a containerized job, streamed through the workspace.
#[tokio::test(flavor = "multi_thread")]
async fn local_checkout_reaches_containerized_jobs() {
    if !is_tool_ready("git") {
        return;
    }
    if !testkit::is_docker_ready().await {
        return;
    }
    run_checkout("boxed", Some(RUNNER_IMAGE_2404)).await;
}

/// The same tree in a plain image with no `safe.directory` override: the
/// job runs as root, so the checkout must belong to root, not to the host
/// user that packed it, or every `git` step refuses the workspace.
#[tokio::test(flavor = "multi_thread")]
async fn local_checkout_belongs_to_the_container_user() {
    if !is_tool_ready("git") {
        return;
    }
    if !testkit::is_docker_ready().await {
        return;
    }
    run_checkout("plain", Some("buildpack-deps:noble")).await;
}

/// A corpus-shaped source — detached at its pin, no branch at all — still
/// materializes onto the run's branch, with the matching remote-tracking ref:
/// the git shape GitHub's checkout guarantees, whatever the source's own.
#[tokio::test(flavor = "multi_thread")]
async fn a_detached_branchless_source_lands_on_the_run_branch() {
    if !is_tool_ready("git") {
        return;
    }
    let fixture = Fixture::new("detached");
    git_in(&fixture.0, &["checkout", "--quiet", "--detach"]);
    git_in(&fixture.0, &["branch", "-D", "main"]);
    let text = "on: push\n\
                jobs:\n\
                \x20 probe:\n\
                \x20   runs-on: ubuntu-latest\n\
                \x20   steps:\n\
                \x20     - uses: actions/checkout@v4\n\
                \x20     - run: test \"$(git rev-parse --abbrev-ref HEAD)\" = main && echo on-run-branch\n\
                \x20     - run: test \"$(git rev-parse origin/main)\" = \"$(git rev-parse HEAD)\" && echo tracking-ref-set\n";
    let mut graph = lower_ok(text);
    graph.params.insert(
        "petri".into(),
        json!({ "repo": fixture.0.display().to_string() }),
    );
    let report = run_host(graph, "detached").await;
    let lines = log_lines(&report);
    assert_eq!(report.status, RunStatus::Success, "log: {lines:?}");
    assert!(lines.iter().any(|l| l == "on-run-branch"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "tracking-ref-set"), "{lines:?}");
}
