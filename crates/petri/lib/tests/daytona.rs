//! The standalone host on `--backend daytona`: a run's sandbox is a sandbox the
//! run records by provider and id, retention keeps it stopped and `prune`
//! deletes it with a tombstone, and a resume after a crash attaches the same
//! sandbox and fences it before the step runs again. Every test skips without a
//! Daytona credential and a plugin whose backend accepts it, unless
//! `PETRI_REQUIRE_DAYTONA` says the tier must run. The Docker halves are in
//! `host.rs`; `crates/core/executor-sandbox/DAYTONA.md` maps Fabro's former
//! live suite onto these.

use std::time::Duration;

use petri::execution::prune::{PruneError, prune};
use petri::execution::{self, host};
use petri::executor::Retention;
use petri::ir::{GraphBuilder, RunStatus, Scope, ScopeId, StepRef};
use petri::steps::PROCESS_KIND;
use petri::{RunOptions, SandboxBackend};
use serde_json::json;
use testkit::{DaytonaObserver, RunDir, is_daytona_ready, wait_for_file};

/// The root invocation's scope 0 is the run's first lease.
const LEASE: u64 = 0;

fn options(dir: &RunDir, retention: Retention) -> RunOptions {
    let mut options = RunOptions::new(dir.path());
    options.retention = retention;
    options.sandbox.backend = SandboxBackend::Daytona;
    options
}

/// A run kept by retention leaves its sandbox stopped on the provider, with its
/// record saying so; `petri sandbox prune` deletes it, records the
/// tombstone, and refuses to run while a live process holds the run.
#[tokio::test]
async fn a_daytona_run_keeps_its_sandbox_stopped_and_prune_deletes_it() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("host-daytona-prune");
    let rt = petri::runtime().options(options(&dir, Retention::Always));
    let mut b = GraphBuilder::bare();
    let scope = b.add_scope(Scope::new(ScopeId::new(0)));
    b.add_node(
        "write",
        scope,
        StepRef::new(
            PROCESS_KIND,
            testkit::script_with("echo kept > kept.txt", &json!({ "shell": "sh" })),
        ),
    );
    let report = host::run(&rt, b.build()).await.expect("runs");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let run_id = testkit::recorded_run_id(dir.path());
    let status = observer
        .sandbox(&run_id, LEASE)
        .await
        .expect("retention kept the sandbox");
    assert!(
        observer.eventually_stopped(&run_id, LEASE).await,
        "retention kept the sandbox, stopped: {status:?}"
    );
    let records = execution::ResourceStore::load(&testkit::read_run_dir(dir.path()).await)
        .await
        .expect("records load");
    let record = records
        .resolve(execution::SandboxLeaseId::new(LEASE))
        .expect("lease 0");
    assert_eq!(record.state, execution::LeaseState::Stopped);
    assert_eq!(record.provider, "daytona");
    assert_eq!(
        record.resource_id.as_deref(),
        Some(status.reference.id.as_str()),
        "the record names the provider's sandbox"
    );
    assert!(record.admission.is_some() && record.reference.is_some());
    drop(records);

    let prune_rt = petri::runtime().options(options(&dir, Retention::Never));
    let pruned = prune(&prune_rt).await.expect("prune");
    assert!(pruned.is_clean(), "{pruned:?}");
    assert_eq!(pruned.deleted.len(), 1, "{pruned:?}");
    assert!(
        observer.sandbox(&run_id, LEASE).await.is_none(),
        "prune deleted the sandbox"
    );
    let records = execution::ResourceStore::load(&testkit::read_run_dir(dir.path()).await)
        .await
        .expect("records load");
    assert_eq!(
        records
            .resolve(execution::SandboxLeaseId::new(LEASE))
            .expect("the tombstone stays")
            .state,
        execution::LeaseState::Deleted
    );
    drop(records);

    // A second prune finds only the tombstone.
    let again = prune(&prune_rt).await.expect("prune again");
    assert!(again.deleted.is_empty() && again.is_clean(), "{again:?}");

    // A held run is refused.
    let held = testkit::write_run_dir(dir.path()).await;
    let error = prune(&prune_rt)
        .await
        .expect_err("a held run is not pruned");
    assert!(matches!(error, PruneError::RunHeld(_)), "{error}");
    drop(held);
    assert!(observer.eventually_gone(&run_id).await.is_empty());
    observer.shutdown().await;
}

/// The fence across a resume, through the host: `host::resume` builds a
/// fresh executor, which finds the crashed run's sandbox by the run id recorded
/// in the run dir and the lease's workspace label, attaches it, and fences
/// it (one stop, one start) before re-dispatching the step into the same
/// workspace.
#[tokio::test]
async fn resume_fences_the_crashed_daytona_sandbox() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("host-daytona-resume");
    let rt = petri::runtime().options(options(&dir, Retention::Never));
    let mut b = GraphBuilder::bare();
    let scope = b.add_scope(Scope::new(ScopeId::new(0)));
    // The beater runs in its own session: an aborted driver still lets the
    // orphaned step task stop its own process group as the channels close,
    // so a detached beater is what a dead *process* leaves behind. With
    // `done` already in the workspace the step measures the heartbeat instead
    // of beating: two sizes a second apart, equal only if the beater is dead.
    b.add_node(
        "beat",
        scope,
        StepRef::new(
            PROCESS_KIND,
            testkit::script_with(
                r#"
if [ -e done ]; then
  a=$(wc -c < heartbeat); sleep 1; b=$(wc -c < heartbeat)
  echo "before=$a" > "$CI_OUTPUT"; echo "after=$b" >> "$CI_OUTPUT"
  exit 0
fi
setsid sh -c 'while :; do echo tick >> heartbeat; sleep 0.05; done' &
sleep 300
"#,
                &json!({ "shell": "sh" }),
            ),
        ),
    );
    let graph = b.build();

    let run = tokio::spawn(async move { host::run(&rt, graph).await });
    assert!(
        wait_for_file(
            &dir.path().join(execution::RUN_FILE),
            Duration::from_secs(60)
        )
        .await,
        "the run never started"
    );
    let run_id = testkit::recorded_run_id(dir.path());
    assert!(
        observer
            .wait_for_file(
                &run_id,
                LEASE,
                "/workspace/heartbeat",
                Duration::from_secs(900)
            )
            .await,
        "the step never started inside the sandbox"
    );
    // The crash: the host task is gone (coordinator, driver and lease drop,
    // release never runs), the sandbox beats on.
    run.abort();
    let _ = run.await;
    let crashed = observer
        .sandbox(&run_id, LEASE)
        .await
        .expect("the crashed run's sandbox outlives it");
    assert!(
        observer.write(&run_id, LEASE, "/workspace/done", b"").await,
        "the crashed sandbox is still running"
    );

    // A resuming process builds everything fresh over the same run dir.
    let rt = petri::runtime().options(options(&dir, Retention::Never));
    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );
    let output = testkit::output_of(&resumed, "beat");
    assert_eq!(
        output["before"], output["after"],
        "the crashed sandbox's beater kept writing: the fence missed it ({output})"
    );
    let leftovers: Vec<String> = observer
        .sandboxes(&run_id)
        .await
        .into_iter()
        .map(|status| format!("{} ({:?})", status.reference.id, status.state))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the run's release ended the sandbox (was {}): {leftovers:?}",
        crashed.reference.id
    );
    observer.shutdown().await;
}
