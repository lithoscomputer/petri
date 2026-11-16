//! The Host plugin: spawn, output, exit status, environment, and the
//! cancellation ladder, with no daemon.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use executor::{
    AcquireContext, Executor, ProcessSpec, Retention, SandboxLeaseId, ScopeOutcome, ScopeSpec, Sig,
};
use executor_sandbox::{
    HostExecutor, LeaseLedger, LeaseState, LostSandbox, MemoryLedger, RoutingExecutor,
    SandboxOptions,
};
use testkit::RunDir;
use tokio::fs;
use tokio::time::timeout;

fn scope() -> ScopeSpec {
    ScopeSpec::new(ir::ScopeId::new(1), "env-1").with_env(
        [(
            smol_str::SmolStr::new("SCOPE_VAR"),
            smol_str::SmolStr::new("scope-value"),
        )]
        .into_iter()
        .collect(),
    )
}

#[tokio::test]
async fn dropping_a_process_closes_its_output_without_releasing_the_scope() {
    let dir = RunDir::new("host-process-drop");
    let executor = HostExecutor::new(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let mut process = handle
        .exec()
        .spawn(ProcessSpec::new("bash", &["-c", "echo ready; sleep 300"]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let ready = timeout(Duration::from_secs(10), lines.recv())
        .await
        .expect("the process started")
        .expect("the process wrote a line");
    assert_eq!(ready.line, "ready");

    drop(process);
    assert!(
        timeout(Duration::from_secs(1), lines.recv())
            .await
            .expect("the abandoned process's pumps were stopped")
            .is_none()
    );
    // The process owns its helpers independently of the reusable scope.
    let mut next = handle
        .exec()
        .spawn(ProcessSpec::new("true", &[]))
        .await
        .expect("the scope still accepts work");
    assert!(next.wait().await.expect("wait").is_success());
    assert!(
        executor
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
}

#[tokio::test]
async fn a_step_runs_and_reports_its_exit_code() {
    let dir = RunDir::new("host-exit-code");
    let executor = HostExecutor::new(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let mut process = handle
        .exec()
        .spawn(ProcessSpec::new("bash", &["-c", "echo hello; exit 3"]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let mut seen = Vec::new();
    while let Some(line) = lines.recv().await {
        seen.push(line.line);
    }
    let status = process.wait().await.expect("wait");
    assert_eq!(seen, vec!["hello".to_owned()]);
    assert_eq!(status.code, Some(3));
    executor.release(handle, ScopeOutcome::Failed).await;
}

#[tokio::test]
async fn scope_env_reaches_the_process() {
    let dir = RunDir::new("host-scope-env");
    let executor = HostExecutor::new(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let mut process = handle
        .exec()
        .spawn(ProcessSpec::new("bash", &[
            "-c",
            "printf '%s' \"$SCOPE_VAR\"",
        ]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let mut seen = String::new();
    while let Some(line) = lines.recv().await {
        seen.push_str(&line.line);
    }
    process.wait().await.expect("wait");
    assert_eq!(seen, "scope-value");
    executor.release(handle, ScopeOutcome::Succeeded).await;
}

#[tokio::test]
async fn a_sigterm_ends_a_sleeping_step() {
    let dir = RunDir::new("host-sigterm");
    let executor = HostExecutor::new(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let mut process = handle
        .exec()
        .spawn(ProcessSpec::new("bash", &["-c", "sleep 300"]))
        .await
        .expect("spawn");
    process.signal(Sig::Term).await.expect("signal");
    let status = timeout(Duration::from_secs(10), process.wait())
        .await
        .expect("wait returned in time")
        .expect("wait");
    assert!(
        status.signal.is_some(),
        "expected a signalled status: {status:?}"
    );
    executor.release(handle, ScopeOutcome::Succeeded).await;
}

#[tokio::test]
async fn the_workspace_is_written_and_read() {
    let dir = RunDir::new("host-workspace-io");
    let executor = HostExecutor::new(dir.path());
    let handle = executor
        .acquire(&scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();
    env.write_file(Path::new("note.txt"), b"hi")
        .await
        .expect("write");
    let read = env.read_file(Path::new("note.txt")).await.expect("read");
    assert_eq!(read.as_deref(), Some(b"hi".as_slice()));
    let missing = env
        .read_file(Path::new("nope.txt"))
        .await
        .expect("read missing");
    assert_eq!(missing, None);
    executor.release(handle, ScopeOutcome::Succeeded).await;
}

#[tokio::test]
async fn a_resumed_host_lease_applies_the_lost_workspace_policy() {
    for (state, missing, policy) in [
        (LeaseState::Live, true, LostSandbox::Refuse),
        (LeaseState::Stopped, true, LostSandbox::Refuse),
        (LeaseState::Live, true, LostSandbox::Replace),
        (LeaseState::Live, false, LostSandbox::Refuse),
        (LeaseState::Allocating, true, LostSandbox::Refuse),
    ] {
        let dir = RunDir::new("host-lost-workspace");
        let ledger = Arc::new(MemoryLedger::default());
        let lease = SandboxLeaseId::new(0);
        let ctx = AcquireContext::bare().with_lease(lease);
        let first = RoutingExecutor::local(dir.path(), Retention::Always);
        first.set_ledger(ledger.clone());
        let handle = first.acquire(&scope(), &ctx).await.unwrap();
        let workspace = PathBuf::from(handle.exec().workspace_path());
        fs::write(workspace.join("kept"), b"original")
            .await
            .unwrap();
        assert!(
            first
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        first.shutdown().await;
        match state {
            LeaseState::Allocating => {
                let admission = ledger
                    .lookup(lease)
                    .await
                    .unwrap()
                    .unwrap()
                    .admission
                    .unwrap();
                ledger.allocating(lease, &admission).await.unwrap();
            }
            LeaseState::Stopped => ledger.stopped(lease).await.unwrap(),
            _ => {}
        }
        if missing {
            fs::remove_dir_all(&workspace).await.unwrap();
        }

        let resumed =
            RoutingExecutor::with_options(dir.path(), Retention::Always, SandboxOptions {
                lost_sandbox: policy,
                ..SandboxOptions::default()
            });
        resumed.set_ledger(ledger.clone());
        let result = resumed.acquire(&scope(), &ctx).await;
        if missing && state != LeaseState::Allocating && policy == LostSandbox::Refuse {
            assert!(
                !workspace.exists(),
                "a refused resume must not recreate the workspace"
            );
            let error = result.expect_err("the lost workspace is refused");
            assert!(error.to_string().contains("workspace was lost"), "{error}");
            assert_eq!(ledger.lookup(lease).await.unwrap().unwrap().state, state);
        } else {
            let handle = result.expect("an intact workspace or permitted create resumes");
            assert!(workspace.is_dir());
            if !missing {
                assert_eq!(fs::read(workspace.join("kept")).await.unwrap(), b"original");
            }
            assert!(
                resumed
                    .release(handle, ScopeOutcome::Succeeded)
                    .await
                    .is_clean()
            );
        }
        resumed.shutdown().await;
    }
}
