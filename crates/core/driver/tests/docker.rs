//! Handoff §7 tests 3 and 10, Docker halves. These skip when no daemon is
//! reachable.
//!
//! The one that matters most is `docker_cancel_kills_the_exec_process_group`:
//! `docker kill` signals PID 1 and never reaches an exec'd step, so
//! cancellation has to be `docker exec … kill -- -PGID`. Getting that wrong is
//! the classic bug, and this is the test aimed at it.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use driver::RunConfig;
use executor::{Executor, Retention, SandboxLeaseId};
use executor_sandbox::{LeaseLedger, LeaseState, MemoryLedger, RoutingExecutor};
use ir::{GraphBuilder, RunStatus, RuntimeSpec, ScopeId, StepRef, validate};
use serde_json::json;
use steps::PROCESS_KIND;
use support::*;
use testkit::is_docker_ready;
use tokio::process::Command;
use tokio::time;

const IMAGE: &str = "alpine:3.20";
const AMD64_EMULATION_PROBE_IMAGE: &str = "amd64/busybox:1.36.1";

async fn amd64_emulation_ready() -> bool {
    if cfg!(target_arch = "x86_64") {
        return true;
    }
    Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            "linux/amd64",
            AMD64_EMULATION_PROBE_IMAGE,
            "true",
        ])
        .output()
        .await
        .is_ok_and(|output| output.status.success())
}

fn docker_graph(name: &str, run: &str) -> ir::Graph {
    let mut b = GraphBuilder::bare();
    let mut scope = ir::Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container(IMAGE);
    let scope = b.add_scope(scope);
    b.add_node(
        name,
        scope,
        StepRef::new(PROCESS_KIND, script_with(run, &json!({ "shell": "sh" }))),
    );
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// A step runs inside the container, in the sandbox's own workspace.
#[tokio::test]
async fn a_step_runs_inside_the_container() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-basic");
    let graph = docker_graph(
        "inside",
        r#"echo "running on $(uname -s)"; echo "where=$(pwd)" > "$CI_OUTPUT""#,
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let report = docker_driver(graph, &dir, config).await_run().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(
        log_lines(&report).iter().any(|l| l == "running on Linux"),
        "{:?}",
        log_lines(&report)
    );
    assert_eq!(
        output_of(&report, "inside")["where"],
        json!("/workspace"),
        "the workspace is at a known path inside the sandbox"
    );
}

/// §7 test 3, Docker variant, and the §4.1 test.
///
/// The step backgrounds a grandchild inside the container. `docker kill` would
/// signal PID 1 and leave both alive; only a signal to the exec's process
/// group reaches them. (That the grandchild dies is checked at the executor
/// level, where the sandbox stays live to read the heartbeat; here the
/// escalation record says which rung ended the step.)
#[tokio::test]
async fn docker_cancel_kills_the_exec_process_group() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-group-kill");
    let graph = docker_graph(
        "backgrounder",
        r"
( while :; do echo tick >> heartbeat; sleep 0.05; done ) &
echo ready > ready
sleep 300
",
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let (driver, prefix) = docker_driver_named(graph, &dir, config);
    let sandbox = format!("{prefix}l0");
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(
        wait_for_container_file(&sandbox, "/workspace/ready", Duration::from_secs(60)).await,
        "the step never started inside the container"
    );
    assert!(
        wait_for_container_file(&sandbox, "/workspace/heartbeat", Duration::from_secs(30)).await,
        "the grandchild never ticked"
    );

    handle.cancel(ir::CancelScopeId::ROOT).await;
    let report = run.await.expect("the run finished");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(
        status_of(&report, "backgrounder").as_deref(),
        Some("cancelled")
    );

    // This step does not trap TERM, so TERM alone must have ended it. If the signal
    // had not reached the group the ladder would have waited out the whole grace
    // period and escalated.
    assert_eq!(
        output_of(&report, "backgrounder")["cancel_escalation"],
        json!("sigterm"),
        "the signal never reached the exec's process group"
    );
    let leftovers = list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}

/// §7 test 10, Docker half. Release leaves no container behind.
#[tokio::test]
async fn docker_release_leaves_no_container() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-release");
    let graph = docker_graph("quick", "echo done");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let (driver, prefix) = docker_driver_named(graph, &dir, config);
    let report = driver.await_run().await;
    assert_eq!(report.status, RunStatus::Success);
    assert!(
        report.releases.iter().any(|r| r.released_any("sandbox")),
        "the release reported removing the sandbox: {:?}",
        report.releases
    );

    // Only this test's own containers: the suite runs in parallel.
    let leftovers = list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}

/// §6 driver test 3, Docker half: after a kill (a second cancel), the step goes
/// straight to `SIGKILL` — the 10s TERM grace is deliberately long enough that
/// waiting it out would fail the timing assertion — and release still leaves no
/// container behind.
#[tokio::test]
async fn docker_kill_leaves_no_container() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-kill");
    let graph = docker_graph(
        "stubborn",
        r"
trap '' TERM
echo ready > ready
while :; do sleep 0.1; done
",
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(10))
        .with_cleanup_grace(Duration::from_secs(300))
        .with_retention(Retention::Never);

    let (driver, prefix) = docker_driver_named(graph, &dir, config);
    let sandbox = format!("{prefix}l0");
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(
        wait_for_container_file(&sandbox, "/workspace/ready", Duration::from_secs(60)).await,
        "the step never started inside the container"
    );
    handle.cancel(ir::CancelScopeId::ROOT).await;
    let killed_at = Instant::now();
    handle.cancel(ir::CancelScopeId::ROOT).await;
    let report = time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the kill ends the run")
        .expect("the run finished");
    let elapsed = killed_at.elapsed();

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "stubborn").as_deref(), Some("cancelled"));
    assert_eq!(
        output_of(&report, "stubborn")["cancel_escalation"],
        json!("sigkill"),
        "the kill skipped the ladder"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "no TERM grace was waited out: {elapsed:?}"
    );
    let leftovers = list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}

/// A lost sandbox reference cannot be replaced by guessing from a name
/// conflict. The Docker provider must return AlreadyExists with the id.
#[tokio::test]
async fn a_name_conflict_without_a_sandbox_reference_fails_the_scope() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-fence");
    // A process crash skips the driver's Drop and scope release. Start the
    // old workload directly through the executor so that abandoning it leaves
    // that same resource state. A separate session keeps the heartbeat alive
    // when the plugin closes its exec stream.
    let graph = docker_graph(
        "beat",
        r#"
if [ -e done ]; then
  a=$(wc -c < heartbeat); sleep 0.5; b=$(wc -c < heartbeat)
  echo "before=$a" > "$CI_OUTPUT"; echo "after=$b" >> "$CI_OUTPUT"
  exit 0
fi
setsid sh -c 'while :; do echo tick >> heartbeat; sleep 0.05; done' &
sleep 300
"#,
    );
    let config = |retention| {
        RunConfig::new(dir.path())
            .with_grace(Duration::from_secs(2))
            .with_retention(retention)
    };

    let crashed = RoutingExecutor::local(dir.path(), Retention::Always);
    let prefix = crashed.container_prefix();
    let spec = executor::ScopeSpec::new(ScopeId::new(0), "scope-0")
        .with_runtime(RuntimeSpec::container(IMAGE));
    let env = crashed
        .acquire(&spec, &executor::AcquireContext::bare())
        .await
        .expect("acquire the crashed run's sandbox");
    let process = env
        .exec()
        .spawn(executor::ProcessSpec::new("sh", &[
            "-c",
            "setsid sh -c 'while :; do echo tick >> heartbeat; sleep 0.05; done' & sleep 300",
        ]))
        .await
        .expect("start the crashed run's workload");
    let sandbox = format!("{prefix}l0");
    assert!(
        wait_for_container_file(&sandbox, "/workspace/heartbeat", Duration::from_secs(60)).await,
        "the step never started inside the container"
    );
    // Deliberately skip release, which a driver abort now performs.
    drop(process);
    drop(env);
    drop(crashed);
    let crashed_id = container_id(&sandbox)
        .await
        .expect("the crashed run's container outlives its driver");
    assert!(
        container_write(&sandbox, "/workspace/done", "").await,
        "the crashed container is still running"
    );

    let report = docker_driver(graph, &dir, config(Retention::Never))
        .await_run()
        .await;
    assert_eq!(
        report.status,
        RunStatus::Failed,
        "{:?}",
        report.state.errors()
    );
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "beat")
        .expect("the scope failure is reported");
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("env_acquire")
    );
    // Refusing adoption still cleans up the run's resource.
    assert!(
        container_id(&sandbox).await.is_none(),
        "release removed the sandbox (was {crashed_id})"
    );
    let leftovers = list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}

/// An acquire nobody waited out leaves nothing to leak, wherever the drop
/// lands. The live case is the sweep aborting a run 90s after a cancel it
/// ignored: the acquire future is dropped mid-flight, and the provider's
/// create — already sent to the plugin — still completes. Two things cover
/// it. A create that lands after its acquire is gone is deleted on arrival.
/// And whatever a crash leaves unrecorded carries the workspace label, so
/// the next acquire over the same run dir attaches it rather than creating
/// a second one, and its release ends it. The drop points are sampled across
/// the whole acquire; every round must end with no container.
#[tokio::test]
async fn an_abandoned_acquire_leaves_no_container() {
    if !is_docker_ready().await {
        return;
    }
    // The image must be present, or early rounds kill the pull mid-flight and
    // acquire never gets to the create window this test aims at.
    assert!(
        Command::new("docker")
            .args(["pull", IMAGE])
            .output()
            .await
            .is_ok_and(|o| o.status.success()),
        "could not pre-pull {IMAGE}"
    );

    let spec = executor::ScopeSpec::new(ScopeId::new(0), "scope-0")
        .with_runtime(RuntimeSpec::container(IMAGE));
    let mut timeout_ms: u64 = 50;
    loop {
        let dir = RunDir::new(&format!("docker-abandon-{timeout_ms}"));
        let executor = RoutingExecutor::local(dir.path().to_path_buf(), Retention::Never);
        let ledger = Arc::new(MemoryLedger::default());
        executor.set_ledger(ledger.clone());
        let prefix = executor.container_prefix();
        let ctx = executor::AcquireContext::bare();

        let completed = match time::timeout(
            Duration::from_millis(timeout_ms),
            executor.acquire(&spec, &ctx),
        )
        .await
        {
            Ok(result) => {
                // The whole acquire fit inside this round's timeout: the drop
                // points have been sampled past the create window.
                if let Ok(handle) = result {
                    executor
                        .release(handle, executor::ScopeOutcome::Succeeded)
                        .await;
                }
                true
            }
            Err(_elapsed) => {
                // Wait for the owned cleanup's confirmation. An empty daemon
                // listing alone does not prove a delayed create has settled.
                // Sixty seconds, like this file's other container waits: under
                // the full suite this round shares the daemon with several
                // long container tests, and a create-then-delete round trip
                // through the plugin has exceeded fifteen seconds there while
                // the whole test finishes in under two seconds alone.
                time::timeout(Duration::from_secs(60), async {
                    loop {
                        let record = LeaseLedger::lookup(&*ledger, SandboxLeaseId::new(0))
                            .await
                            .expect("ledger");
                        if record.is_none_or(|record| record.state == LeaseState::Deleted) {
                            break;
                        }
                        time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .expect("an abandoned acquisition settles its deletion");
                let leftovers = list_containers(&prefix).await;
                assert!(
                    leftovers.is_empty(),
                    "an acquire abandoned at {timeout_ms}ms leaked: {leftovers:?}"
                );
                false
            }
        };
        drop(executor);
        // A fresh process over the same run dir attaches whatever the round
        // left — at most one sandbox for the scope — and its release ends it.
        let again = RoutingExecutor::local(dir.path().to_path_buf(), Retention::Never);
        let handle = again
            .acquire(&spec, &executor::AcquireContext::bare())
            .await
            .expect("the scope reconciles after an abandoned acquire");
        assert_eq!(
            list_containers(&prefix).await.len(),
            1,
            "exactly one sandbox for the scope after reconciliation"
        );
        let report = again
            .release(handle, executor::ScopeOutcome::Succeeded)
            .await;
        assert!(report.is_clean(), "{report:?}");
        assert!(
            list_containers(&prefix).await.is_empty(),
            "round {timeout_ms}ms left containers"
        );
        if completed {
            break;
        }
        timeout_ms += 100;
        assert!(
            timeout_ms < 30_000,
            "acquire of a local image never completed within {timeout_ms}ms"
        );
    }
}

/// A non-zero exit inside the container maps the same way it does on the host.
#[tokio::test]
async fn docker_exit_statuses_propagate() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-exit");
    let graph = docker_graph("failing", "echo about to fail; exit 7");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let report = docker_driver(graph, &dir, config).await_run().await;
    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "failing")
        .unwrap();
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("exit_status:7"),
        "the exit status survived `docker exec`, `setsid` and the wrapper"
    );
}

/// A bad image is an acquire failure, not a run abort.
#[tokio::test]
async fn a_bad_image_fails_the_scope() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-bad-image");
    let mut b = GraphBuilder::bare();
    let mut scope = ir::Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container("petri-nonexistent/definitely-not-real:v0");
    let scope = b.add_scope(scope);
    b.add_node(
        "doomed",
        scope,
        StepRef::new(PROCESS_KIND, script("echo never runs")),
    );
    let graph = b.build();

    let config = RunConfig::new(dir.path()).with_retention(Retention::Never);
    let report = docker_driver(graph, &dir, config).await_run().await;

    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "doomed")
        .expect("the firing failed rather than vanishing");
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("env_acquire")
    );
    assert!(report.state.is_finished(), "the run completed");
}

/// The step's real duration and exit status both survive, whether or not
/// `setsid` forked. If `docker exec` returned early we would see a fast success
/// here instead of a slow failure.
#[tokio::test]
async fn docker_wait_follows_the_step_not_the_client() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-wait");
    let graph = docker_graph("slow-failure", "sleep 1; exit 5");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let started = Instant::now();
    let report = docker_driver(graph, &dir, config).await_run().await;
    let elapsed = started.elapsed();

    assert_eq!(report.status, RunStatus::Failed);
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "slow-failure")
        .unwrap();
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("exit_status:5")
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "the wait returned before the step was done: {elapsed:?}"
    );
}

/// A single-architecture image with no manifest for this daemon still
/// acquires: the pull retries as linux/amd64 — CI images target GitHub's
/// hosted runners — and the step runs (emulated, on a non-amd64 host). The
/// `amd64/` library namespace publishes amd64-only manifests, so this
/// exercises the fallback on arm64 daemons and the native path on amd64 ones;
/// either way the container reports an x86_64 machine.
#[tokio::test]
#[expect(
    clippy::print_stderr,
    reason = "the skip notice belongs to the test runner's output, which no subscriber reads"
)]
async fn an_amd64_only_image_acquires_via_the_platform_fallback() {
    if !is_docker_ready().await {
        return;
    }
    if !amd64_emulation_ready().await {
        eprintln!("skipping: the Docker daemon cannot execute linux/amd64 binaries");
        return;
    }
    let dir = RunDir::new("docker-platform-fallback");
    let mut b = GraphBuilder::bare();
    let mut scope = ir::Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container("amd64/alpine:3.20");
    let scope = b.add_scope(scope);
    b.add_node(
        "arch",
        scope,
        StepRef::new(
            PROCESS_KIND,
            script_with("uname -m", &json!({ "shell": "sh" })),
        ),
    );
    let graph = b.build();
    validate(&graph).expect("valid");
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let report = docker_driver(graph, &dir, config).await_run().await;
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert!(
        log_lines(&report).iter().any(|l| l == "x86_64"),
        "{:?}",
        log_lines(&report)
    );
}

/// Output printed *after* a pause survives, on both streams. The exit status
/// was always safe (the wrapper records it), but the log path was not: run
/// directly under `docker exec`, `setsid` forks and the client detaches from a
/// step still running — every later line is lost. Fast steps never showed it;
/// the keeper shell the executor now runs `setsid` under is what keeps the
/// client attached.
#[tokio::test]
async fn docker_output_after_a_pause_is_captured() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-late-output");
    let graph = docker_graph(
        "slow-talker",
        "echo start; sleep 2; echo after-sleep; echo err-after >&2; exit 3",
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(2))
        .with_retention(Retention::Never);

    let report = docker_driver(graph, &dir, config).await_run().await;

    let lines = log_lines(&report);
    for expected in ["start", "after-sleep", "err-after"] {
        assert!(
            lines.iter().any(|l| l == expected),
            "`{expected}` reached the log: {lines:?}"
        );
    }
    assert_eq!(
        status_of(&report, "slow-talker").as_deref(),
        Some("failure")
    );
    let record = report
        .state
        .history()
        .iter()
        .find(|r| r.name == "slow-talker")
        .unwrap();
    assert_eq!(
        record
            .outcome
            .status
            .failure_info()
            .map(|f| f.class.as_str()),
        Some("exit_status:3")
    );
}

/// The wrapper is inside the group the ladder kills, so it never gets to record
/// a status. That path must land on the cancellation outcome rather than
/// hanging on a status file that is not coming, or falling back to a default.
#[tokio::test]
async fn docker_cancel_without_a_recorded_status_still_reports_cancelled() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-killed-wrapper");
    // Ignores TERM, so the ladder escalates and SIGKILLs the whole group, wrapper
    // included, mid-step.
    let graph = docker_graph(
        "stubborn",
        r"
trap '' TERM
echo ready > ready
while :; do sleep 0.1; done
",
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(1))
        .with_retention(Retention::Never);

    let (driver, prefix) = docker_driver_named(graph, &dir, config);
    let sandbox = format!("{prefix}l0");
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_container_file(&sandbox, "/workspace/ready", Duration::from_secs(60)).await);
    handle.cancel(ir::CancelScopeId::ROOT).await;

    let report = time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the run must not hang waiting for a status file that is not coming")
        .expect("the run finished");

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "stubborn").as_deref(), Some("cancelled"));
    assert_eq!(
        output_of(&report, "stubborn")["cancel_escalation"],
        json!("sigkill"),
        "the escalation is recorded even though the provider observed no exit"
    );
    let leftovers = list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}
