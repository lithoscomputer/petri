//! A killed plugin fails current calls and is relaunched once for new work.

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::{env, fs};

use executor::{
    AcquireContext, Executor, OneShotContainer, ProcessHandle, ProcessSpec, Retention,
    SandboxLeaseId, ScopeOutcome, ScopeSpec,
};
use executor_sandbox::{MemoryLedger, PluginSettings, PluginSource, RoutingExecutor};
use ir::{RuntimeSpec, ScopeId};
use testkit::{RunDir, container_id, is_docker_ready, list_containers, sandbox_name};
use tokio::process::Command;
use tokio::time::{sleep, timeout};

const WAIT: Duration = Duration::from_secs(15);

async fn started(process: &mut dyn ProcessHandle) {
    let mut lines = process.lines().expect("process output");
    let first = timeout(WAIT, lines.recv())
        .await
        .expect("the process starts before the deadline")
        .expect("the process reports its start");
    assert_eq!(first.line, "started");
}

#[tokio::test]
async fn a_plugin_crash_fails_calls_then_recovers_one_generation_and_the_same_sandbox() {
    if !is_docker_ready().await {
        return;
    }
    let directory = RunDir::new("plugin-crash");
    let launcher = directory.path().join("launch");
    let binary = env::var_os("PETRI_SANDBOX_DOCKER_PLUGIN").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../target/plugins/bin/sandbox-driver-docker")
        },
        PathBuf::from,
    );
    symlink(
        fs::canonicalize(binary).unwrap(),
        directory.path().join("plugin"),
    )
    .unwrap();
    fs::write(&launcher, "#!/bin/sh\ncd -- \"$(dirname -- \"$0\")\" || exit 1\necho \"$$\" >> launches\nexec ./plugin\n").unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let supervisor = Arc::new(PluginSource::new(
        PluginSettings::at_path("docker", &launcher).unwrap(),
    ));
    let router = RoutingExecutor::with_provider_source(
        supervisor.clone(),
        directory.path(),
        Retention::Never,
    );
    router.set_ledger(Arc::new(MemoryLedger::default()));
    let scope = ScopeSpec::new(ScopeId::new(0), "scope-0")
        .with_runtime(RuntimeSpec::container("alpine:3.20"));
    let lease = SandboxLeaseId::new(0);
    let ctx = AcquireContext::bare().with_lease(lease);
    let handle = router.acquire(&scope, &ctx).await.unwrap();
    let generation = supervisor.current().await.unwrap();
    let name = sandbox_name(directory.path(), lease.raw());
    let original = container_id(&name).await.unwrap();
    handle
        .exec()
        .write_file(Path::new("before"), b"kept")
        .await
        .unwrap();

    let mut execs = Vec::new();
    for marker in ["first", "second"] {
        let mut process = handle
            .exec()
            .spawn(ProcessSpec::new("sh", &[
                "-c",
                &format!("echo {marker} >> calls; echo started; exec sleep 300"),
            ]))
            .await
            .unwrap();
        started(&mut *process).await;
        execs.push(process);
    }
    let mut oneshot = handle
        .container_runner()
        .unwrap()
        .run(OneShotContainer::registry("alpine:3.20").with_args(&[
            "sh",
            "-c",
            "echo oneshot > /workspace/oneshot; echo started; exec sleep 300",
        ]))
        .await
        .unwrap();
    started(&mut *oneshot).await;
    let launches = fs::read_to_string(directory.path().join("launches")).unwrap();
    assert_eq!(launches.lines().count(), 1);
    assert!(
        Command::new("kill")
            .args(["-KILL", launches.trim()])
            .status()
            .await
            .unwrap()
            .success()
    );
    for process in &mut execs {
        assert!(
            timeout(WAIT, process.wait())
                .await
                .expect("the failed transport settles exec")
                .is_err()
        );
    }
    assert!(
        timeout(WAIT, oneshot.wait())
            .await
            .expect("the failed transport settles one-shot")
            .is_err()
    );
    timeout(WAIT, async {
        while !generation.is_closed() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    let (first, second) = tokio::join!(supervisor.current(), supervisor.current());
    let (first, second) = (first.unwrap(), second.unwrap());
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.number(), generation.number() + 1);
    assert_eq!(
        fs::read_to_string(directory.path().join("launches"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    let recovered = router.acquire(&scope, &ctx).await.unwrap();
    assert_eq!(
        container_id(&name).await.as_deref(),
        Some(original.as_str())
    );
    let environment = recovered.exec();
    assert_eq!(
        environment
            .read_file(Path::new("before"))
            .await
            .unwrap()
            .unwrap(),
        b"kept"
    );
    assert_eq!(
        environment
            .read_file(Path::new("calls"))
            .await
            .unwrap()
            .unwrap(),
        b"first\nsecond\n"
    );
    assert_eq!(
        environment
            .read_file(Path::new("oneshot"))
            .await
            .unwrap()
            .unwrap(),
        b"oneshot\n"
    );
    // Recovery removes abandoned one-shots before resuming the sandbox.
    let prefix = router.container_prefix().await.unwrap();
    assert_eq!(list_containers(&prefix).await, vec![name]);
    assert!(
        router
            .release(handle, ScopeOutcome::Failed)
            .await
            .is_clean()
    );
    assert!(
        router
            .release(recovered, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    // Prune through a dead generation must launch a fresh plugin too.
    let launches = fs::read_to_string(directory.path().join("launches")).unwrap();
    assert!(
        Command::new("kill")
            .args(["-KILL", launches.lines().last().unwrap()])
            .status()
            .await
            .unwrap()
            .success()
    );
    timeout(WAIT, async {
        while !first.is_closed() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    router
        .delete_recorded(lease, scope.workspace_id.as_str())
        .await
        .unwrap();
    assert!(list_containers(&prefix).await.is_empty());
    supervisor.shutdown().await;
}

#[tokio::test]
async fn host_actions_recover_after_the_docker_plugin_restarts() {
    if !is_docker_ready().await {
        return;
    }
    let directory = RunDir::new("host-action-plugin-recovery");
    let supervisor = Arc::new(PluginSource::new(
        PluginSettings::from_env("docker", Some(true)).expect("settings"),
    ));
    let router = RoutingExecutor::with_provider_source(
        supervisor.clone(),
        directory.path(),
        Retention::Never,
    );
    let scope = ScopeSpec::new(ScopeId::new(0), "scope-0");
    let handle = router
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("host scope");
    let runner = handle.container_runner().expect("action runner");
    for (index, command) in [
        "echo before > /workspace/kept",
        "test $(cat /workspace/kept) = before",
    ]
    .into_iter()
    .enumerate()
    {
        let mut process = runner
            .run(OneShotContainer::registry("alpine:3.20").with_args(&["sh", "-c", command]))
            .await
            .expect("action starts through the current plugin");
        let mut lines = process.lines().expect("action lines");
        while lines.recv().await.is_some() {}
        assert!(process.wait().await.expect("action finishes").is_success());
        if index == 0 {
            supervisor.shutdown().await;
        }
    }
    assert_eq!(supervisor.current().await.unwrap().number(), 2);
    let prefix = router.container_prefix().await.unwrap();
    assert_eq!(list_containers(&prefix).await.len(), 1);
    assert!(
        router
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    assert!(list_containers(&prefix).await.is_empty());
    router.shutdown().await;
    supervisor.shutdown().await;
}
