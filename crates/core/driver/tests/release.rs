//! §6 driver test 3, host half: after a kill — or an aborted step future, or a
//! workload that murders its own supervisor — no process group survives scope
//! release, no zombie outlives it, and a normal release completes well inside
//! the observation deadline.
//!
//! The sentinel is the mechanism under test: the group leader that pins the
//! pgid from spawn to release, so release's one `SIGKILL` can never reach a
//! recycled id. Ownership is what these tests assert — deterministically, not
//! by timing a watcher.

mod support;

use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

use driver::RunConfig;
use executor::{Executor as _, ProcessSpec, Retention, ScopeOutcome, ScopeSpec};
use executor_sandbox::HostExecutor;
use ir::{CancelScopeId, GraphBuilder, RunStatus, ScopeId, StepRef};
use serde_json::json;
use support::*;
use tokio::time;

/// Pids of live processes whose command line mentions `pattern` — which the
/// sentinel's does: it carries its status-file path, unique per run directory.
/// Zombies have no readable command line, so a reaped-or-zombie sentinel drops
/// out.
fn pids_matching(pattern: &str) -> Vec<i32> {
    let output = Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .expect("pgrep runs");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// The pattern a sentinel's command line carries: its record paths, under
/// the built-in Host instance that owns `sandbox` (a `host-<instance>-s<n>`
/// id), which no other test's provider shares.
fn sentinel_pattern(sandbox: &str) -> String {
    let instance = sandbox
        .strip_prefix("host-")
        .and_then(|rest| rest.rsplit_once("-s"))
        .map(|(instance, _)| instance)
        .expect("a built-in Host sandbox id");
    format!("sandbox-driver-host/{instance}/")
}

/// The `ps` state of one pid, empty when the process is fully gone.
fn ps_state(pid: i32) -> String {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A wedged step kind's future is aborted by the hard deadline; nothing kills
/// its process tree at that moment — with `kill_on_drop` gone, that is
/// release's job.
#[tokio::test]
async fn an_aborted_steps_process_tree_dies_at_release() {
    let dir = RunDir::new("aborted-tree");
    let mut b = GraphBuilder::new();
    b.add_node(
        "wedged",
        ScopeId::new(0),
        StepRef::new(SPAWN_AND_WEDGE_KIND, json!({})),
    );
    let graph = b.build();

    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_millis(300))
        .with_retention(Retention::Always);
    let driver = host_driver_full(
        graph,
        &dir,
        executor::MapSecrets::empty(),
        RunConfig {
            hard_deadline_slack: Duration::from_millis(300),
            ..config
        },
        runners_with_spawn_and_wedge(),
    );
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    let workspace = dir.workspace();

    assert!(
        wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await,
        "the spawned tree never started"
    );
    let heartbeat = workspace.join("heartbeat");
    assert!(wait_for_file(&heartbeat, Duration::from_secs(10)).await);

    handle.cancel(CancelScopeId::ROOT).await;
    let report = time::timeout(Duration::from_secs(15), run)
        .await
        .expect("the hard deadline ends the run")
        .expect("the run finished");

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(
        output_of(&report, "wedged")["cancel_escalation"],
        json!(driver::CANCEL_FORCED),
        "the future was aborted, not returned"
    );
    assert!(
        report
            .releases
            .iter()
            .any(|r| r.is_clean() && r.kept_any("sandbox")),
        "release reported killing the group: {:?}",
        report.releases
    );
    // The grandchild died with the group at release — the report has already been
    // handed back, so anything still ticking here survived it.
    time::sleep(Duration::from_millis(400)).await;
    let before = file_len(&heartbeat);
    time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        file_len(&heartbeat),
        before,
        "the aborted step's grandchild outlived release"
    );
}

/// A workload leader that exits naturally while its backgrounded child remains:
/// the step succeeds, and release still kills the survivor.
#[tokio::test]
async fn a_natural_exits_survivor_dies_at_release() {
    let dir = RunDir::new("survivor");
    let graph = {
        let mut b = GraphBuilder::new();
        add_script(
            &mut b,
            "leaver",
            ScopeId::new(0),
            "( while :; do echo tick >> heartbeat; sleep 0.05; done ) >/dev/null 2>&1 &\necho done",
        );
        let graph = b.build();
        ir::validate(&graph).expect("valid");
        graph
    };
    let config = RunConfig::new(dir.path()).with_retention(Retention::Always);
    let report = host_driver_with(graph, &dir, executor::MapSecrets::empty(), config)
        .await_run()
        .await;

    assert_eq!(report.status, RunStatus::Success);
    assert_eq!(status_of(&report, "leaver").as_deref(), Some("success"));
    assert!(
        report
            .releases
            .iter()
            .any(|r| r.is_clean() && r.kept_any("sandbox")),
        "the group was killed even though the workspace was kept: {:?}",
        report.releases
    );
    let heartbeat = dir.workspace().join("heartbeat");
    time::sleep(Duration::from_millis(400)).await;
    let before = file_len(&heartbeat);
    time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        file_len(&heartbeat),
        before,
        "the backgrounded survivor outlived release"
    );
}

/// The safety property itself, asserted as ownership rather than timing: after
/// the workload and every straggler have exited on their own, the sentinel is
/// still alive — pinning the pgid — until release, and release leaves nothing,
/// zombie included. The normal release also completes well inside the
/// observation deadline: an `ESRCH`-before-reap ordering would hit that
/// deadline on every run, because the zombie leader keeps the group visible.
#[tokio::test]
async fn the_sentinel_pins_the_group_until_release() {
    let dir = RunDir::new("sentinel-pins");
    let executor = HostExecutor::new(dir.path());
    let env = executor
        .acquire(
            &ScopeSpec::new(ScopeId::new(0), "scope-0"),
            &executor::AcquireContext::bare(),
        )
        .await
        .expect("acquire");

    let mut handle = env
        .exec()
        .spawn(ProcessSpec::new("bash", &["-c", "echo hi"]))
        .await
        .expect("spawn");
    let status = handle.wait().await.expect("wait");
    assert!(status.is_success());

    // The workload is gone; the sentinel is not. Its command line names the
    // status file under its provider instance's own directory.
    let pattern = sentinel_pattern(&env.sandbox().instance);
    time::sleep(Duration::from_millis(100)).await;
    let sentinels = pids_matching(&pattern);
    assert_eq!(
        sentinels.len(),
        1,
        "the sentinel outlives its workload until release"
    );
    let sentinel = sentinels[0];

    let released_at = Instant::now();
    let report = executor.release(env, ScopeOutcome::Succeeded).await;
    let elapsed = released_at.elapsed();
    assert!(
        report.released_any("sandbox"),
        "release reported the group: {report:?}"
    );
    assert!(report.is_clean(), "{report:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "a normal release completes well inside the observation deadline: {elapsed:?}"
    );
    assert_eq!(
        ps_state(sentinel),
        "",
        "no sentinel — and no zombie — outlives release"
    );
}

/// The hostile case: the workload kills its own supervisor. The unreaped zombie
/// still pins the identifier, so release cannot signal a recycled group;
/// reaping at release leaves no zombie behind; and the workload's own end is
/// observed as group death rather than trusted to a supervisor that no longer
/// exists.
#[tokio::test]
async fn a_workload_that_kills_its_sentinel_cannot_free_the_group_id() {
    let dir = RunDir::new("hostile-workload");
    let executor = HostExecutor::new(dir.path());
    let env = executor
        .acquire(
            &ScopeSpec::new(ScopeId::new(0), "scope-0"),
            &executor::AcquireContext::bare(),
        )
        .await
        .expect("acquire");

    let mut handle = env
        .exec()
        .spawn(ProcessSpec::new("bash", &[
            "-c",
            "while [ ! -f go ]; do sleep 0.05; done; kill -9 $PPID; echo after > after.txt",
        ]))
        .await
        .expect("spawn");

    // Catch the sentinel's pid while it is alive, then let the murder happen.
    let pattern = sentinel_pattern(&env.sandbox().instance);
    let deadline = Instant::now() + Duration::from_secs(10);
    let sentinel = loop {
        if let Some(pid) = pids_matching(&pattern).first().copied() {
            break pid;
        }
        assert!(Instant::now() < deadline, "no sentinel appeared");
        time::sleep(Duration::from_millis(20)).await;
    };
    let workspace = executor.workspace_for("scope-0");
    fs::write(workspace.join("go"), b"").expect("go file");

    let status = handle.wait().await.expect("wait resolves via group death");
    assert_eq!(
        status.signal,
        Some(9),
        "with the supervisor dead, the outcome is the kill, not a recorded status"
    );
    assert!(
        workspace.join("after.txt").exists(),
        "the workload kept running after murdering its sentinel"
    );
    assert!(
        ps_state(sentinel).starts_with('Z'),
        "the dead sentinel is held unreaped, still pinning the group id"
    );

    let report = executor.release(env, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(
        ps_state(sentinel),
        "",
        "release reaped the sentinel: no zombie left behind"
    );
}

/// FD hygiene: the sentinel closes its copies of the output pipes once the
/// workload holds them, so a trivial step's log stream reaches EOF immediately
/// instead of waiting out the five-second drain limit.
#[tokio::test]
async fn a_trivial_step_does_not_wait_out_the_log_drain() {
    let dir = RunDir::new("fd-hygiene");
    let graph = {
        let mut b = GraphBuilder::new();
        add_script(&mut b, "hello", ScopeId::new(0), "echo hi");
        let graph = b.build();
        ir::validate(&graph).expect("valid");
        graph
    };
    let started_at = Instant::now();
    let report = run_host(graph, &dir).await;
    let elapsed = started_at.elapsed();

    assert_eq!(report.status, RunStatus::Success);
    assert!(
        log_lines(&report).iter().any(|l| l == "hi"),
        "{:?}",
        log_lines(&report)
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "the pipes reached EOF without the drain limit: {elapsed:?}"
    );
}
