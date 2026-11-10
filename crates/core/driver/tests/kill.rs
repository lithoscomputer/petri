//! The two-tier stop wiring (§10): the first root cancel is polite and admits
//! `run_on_cancel` cleanup; the cleanup-grace timer — or a second cancel —
//! feeds `KillRequested`, and after a kill nothing starts and nothing waits out
//! a grace.

mod support;

use std::time::{Duration, Instant};

use driver::RunConfig;
use engine::{Event, EventOrigin};
use executor::Retention;
use ir::{CancelScopeId, GraphBuilder, RunStatus, ScopeId};
use serde_json::json;
use support::*;
use tokio::time;

/// work → cleanup, with cleanup opted in to run after a cancel.
fn cleanup_graph(work_script: &str, cleanup_script: &str) -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let work = add_script(&mut b, "work", scope, work_script);
    let cleanup = add_script(&mut b, "cleanup", scope, cleanup_script);
    b.link(work, cleanup);
    b.node_mut(cleanup).run_on_cancel = true;
    let graph = b.build();
    ir::validate(&graph).expect("valid");
    graph
}

/// The log's one `KillRequested`, which must be External, with nothing starting
/// after it.
fn assert_kill_in_log(report: &driver::ExecutionReport) {
    let kills: Vec<u64> = report
        .state
        .log
        .records()
        .iter()
        .filter(|r| matches!(r.event, Event::KillRequested { .. }))
        .map(|r| {
            assert_eq!(
                r.origin,
                EventOrigin::External,
                "the kill is in the log as External"
            );
            r.seq
        })
        .collect();
    assert_eq!(kills.len(), 1, "exactly one KillRequested");
    let kill_seq = kills[0];
    assert!(
        !report
            .state
            .log
            .records()
            .iter()
            .any(|r| r.seq > kill_seq && matches!(r.event, Event::StepStarted { .. })),
        "no step starts after the kill"
    );
}

/// §6 driver test 1: a real cleanup process executes after a cancel — pinned by
/// which step started, not just by the file it left behind.
#[tokio::test]
async fn cleanup_runs_after_a_cancel() {
    let dir = RunDir::new("cleanup-after-cancel");
    let graph = cleanup_graph(
        "echo ready > ready && sleep 300",
        "echo cleaned > cleaned.txt",
    );
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(1))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let driver = host_driver_with(graph.clone(), &dir, executor::MapSecrets::empty(), config);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await);
    handle.cancel(CancelScopeId::ROOT).await;
    let report = run.await.expect("the run finished");

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "work").as_deref(), Some("cancelled"));
    assert!(
        started(&report).iter().any(|n| n == "cleanup"),
        "the cleanup step started for real: {:?}",
        started(&report)
    );
    assert_eq!(status_of(&report, "cleanup").as_deref(), Some("success"));
    assert!(workspace.join("cleaned.txt").exists());
    assert_replay_identical(&graph, &report);
}

/// §6 driver test 2, first half: cleanup-grace expiry feeds `KillRequested`.
/// The running process ignores TERM, and its TERM grace exceeds the root
/// cleanup grace. Waiting for its ready marker before cancel makes timer
/// expiry independent of how long a new cleanup process takes to start.
#[tokio::test]
async fn cleanup_grace_expiry_feeds_kill() {
    let dir = RunDir::new("cleanup-grace-kill");
    let mut builder = GraphBuilder::new();
    add_script(
        &mut builder,
        "work",
        ScopeId::new(0),
        "trap '' TERM; echo ready > ready; sleep 300",
    );
    let graph = builder.build();
    let config = RunConfig::new(dir.path())
        .with_grace(Duration::from_secs(10))
        .with_cleanup_grace(Duration::from_secs(2))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let driver = host_driver_with(graph.clone(), &dir, executor::MapSecrets::empty(), config);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await);
    let cancelled_at = Instant::now();
    handle.cancel(CancelScopeId::ROOT).await;
    let report = time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the kill ends the run")
        .expect("the run finished");
    let elapsed = cancelled_at.elapsed();

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "work").as_deref(), Some("cancelled"));
    assert_eq!(
        output_of(&report, "work")["cancel_escalation"],
        json!("sigkill"),
        "Control::Kill went straight to SIGKILL"
    );
    assert!(
        elapsed < Duration::from_secs(7),
        "no 10s TERM grace was waited out after the kill: {elapsed:?}"
    );
    assert_kill_in_log(&report);
    assert_one_terminal_per_firing(&report);
    assert_replay_identical(&graph, &report);
}

/// §6 driver test 2, second half: a second root cancel feeds `KillRequested`
/// without waiting for the cleanup grace, even after work used its full TERM
/// grace.
#[tokio::test]
async fn a_second_cancel_feeds_kill() {
    let dir = RunDir::new("second-cancel-kill");
    let graph = cleanup_graph(
        "trap '' TERM; echo ready > ready; sleep 300",
        "trap '' TERM; echo x > cleanup-started; sleep 300",
    );
    let term_grace = Duration::from_secs(10);
    let config = RunConfig::new(dir.path())
        .with_grace(term_grace)
        .with_cleanup_grace(Duration::from_secs(300))
        .with_retention(Retention::Always);
    let workspace = dir.workspace();

    let driver = host_driver_with(graph.clone(), &dir, executor::MapSecrets::empty(), config);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());

    assert!(wait_for_file(&workspace.join("ready"), Duration::from_secs(10)).await);
    handle.cancel(CancelScopeId::ROOT).await;
    // Work ignores TERM, so cleanup cannot start until the TERM grace expires.
    // Leave a separate startup window after that grace; this is setup, not the
    // second cancel's latency assertion below.
    assert!(
        wait_for_file(
            &workspace.join("cleanup-started"),
            term_grace + Duration::from_secs(10),
        )
        .await,
        "the polite cancel admitted the cleanup"
    );
    let killed_at = Instant::now();
    handle.cancel(CancelScopeId::ROOT).await;
    let report = time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the second cancel ends the run without waiting the 5 minute cleanup grace")
        .expect("the run finished");
    let elapsed = killed_at.elapsed();

    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "work").as_deref(), Some("cancelled"));
    assert_eq!(
        output_of(&report, "work")["cancel_escalation"],
        json!("sigkill")
    );
    assert_eq!(
        output_of(&report, "cleanup")["cancel_escalation"],
        json!("sigkill")
    );
    assert!(
        elapsed < Duration::from_secs(7),
        "the kill skipped the TERM grace: {elapsed:?}"
    );
    assert_kill_in_log(&report);
    assert_one_terminal_per_firing(&report);
    assert_replay_identical(&graph, &report);
}
