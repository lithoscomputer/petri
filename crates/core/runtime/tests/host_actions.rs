//! Shared host actions keep their caller's helper alive and their env separate.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::{env, process};

use executor::{
    AcquireContext, ContainerRunner, Executor as _, OneShotContainer, ProcessSpec, Retention,
    SandboxLeaseId, ScopeOutcome, ScopeSpec, WorkspaceId,
};
use executor_sandbox::{MemoryLedger, PluginSettings, PluginSource, RoutingExecutor};
use ir::ScopeId;
use testkit::{RunDir, container_id, is_docker_ready, list_containers, recorded_run_id};
use tokio::fs;
use tokio::process::Command;
use tokio::time::timeout;

#[tokio::test]
async fn remote_docker_actions_fail_without_blocking_host_processes() {
    const CHILD: &str = "PETRI_TEST_REMOTE_HOST_ACTION";
    if env::var_os(CHILD).is_none() {
        let output = timeout(
            Duration::from_secs(30),
            Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "remote_docker_actions_fail_without_blocking_host_processes",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("DOCKER_HOST", "tcp://192.0.2.1:2375")
                .env("PETRI_SANDBOX_DOCKER_HOST_ADDRESS", "petri.internal")
                .env_remove("PETRI_SANDBOX_DOCKER_PLUGIN")
                .stdin(process::Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("remote configuration must not cause a network wait")
        .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let dir = RunDir::new("host-actions-remote-refusal");
    let router = RoutingExecutor::local(dir.path(), Retention::Never);
    let handle = router
        .acquire(
            &ScopeSpec::new(ScopeId::new(0), "scope-0"),
            &AcquireContext::bare(),
        )
        .await
        .expect("ordinary Host scopes remain usable");
    let mut process = handle
        .exec()
        .spawn(ProcessSpec::new("true", &[]))
        .await
        .unwrap();
    assert!(process.wait().await.unwrap().is_success());
    let runner = handle.container_runner().expect("routable action refusal");
    assert!(
        runner
            .host_address()
            .unwrap_err()
            .to_string()
            .contains("local Docker daemon")
    );
    let Err(error) = runner.run(OneShotContainer::registry("alpine:3.20")).await else {
        panic!("a remote daemon cannot bind this Host workspace");
    };
    assert!(error.to_string().contains("--backend docker"), "{error}");
    assert!(
        router
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    router.shutdown().await;
}

async fn action_env(runner: &dyn ContainerRunner) -> String {
    let mut process = runner
        .run(OneShotContainer::registry("alpine:3.20").with_args(&[
            "sh",
            "-c",
            "printf '%s\\n' \"$SCOPE_VALUE\"",
        ]))
        .await
        .expect("action");
    let mut lines = process.lines().expect("lines");
    let mut output = Vec::new();
    while let Some(line) = lines.recv().await {
        output.push(line.line);
    }
    assert!(process.wait().await.expect("wait").is_success());
    output.join("\n")
}

#[tokio::test]
async fn inherited_host_scopes_share_the_helper_and_keep_their_own_env() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("host-actions-inherited");
    let router = RoutingExecutor::local(dir.path(), Retention::Always);
    let lease = SandboxLeaseId::new(7);
    let ledger = Arc::new(MemoryLedger::default());
    router.set_ledger(ledger);
    let ctx = AcquireContext::bare().with_lease(lease);
    let mut parent_spec =
        ScopeSpec::new(ScopeId::new(0), "parent").with_workspace_id(WorkspaceId::new("shared"));
    parent_spec
        .env
        .insert("SCOPE_VALUE".into(), "parent".into());
    let parent = router.acquire(&parent_spec, &ctx).await.expect("parent");
    let parent_runner = parent.container_runner().expect("runner");
    assert_eq!(action_env(&*parent_runner).await, "parent");
    let name = format!("petri-{}-a-shared", recorded_run_id(dir.path()));
    let original = container_id(&name).await.expect("action host");

    let mut child_spec =
        ScopeSpec::new(ScopeId::new(1), "child").with_workspace_id(WorkspaceId::new("shared"));
    child_spec.env.insert("SCOPE_VALUE".into(), "child".into());
    let child = router.acquire(&child_spec, &ctx).await.expect("child");
    assert_eq!(
        container_id(&name).await.as_deref(),
        Some(original.as_str())
    );
    let child_runner = child.container_runner().expect("runner");
    assert_eq!(action_env(&*child_runner).await, "child");
    assert_eq!(action_env(&*parent_runner).await, "parent");
    assert_eq!(
        container_id(&name).await.as_deref(),
        Some(original.as_str())
    );

    assert!(
        router
            .release(child, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    assert!(
        router
            .release(parent, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    assert_eq!(
        container_id(&name).await.as_deref(),
        Some(original.as_str())
    );
    let report = router.release_lease(lease, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(report.released_any("action host"));
    let prefix = format!("petri-{}-", recorded_run_id(dir.path()));
    assert!(list_containers(&prefix).await.is_empty());
}

#[tokio::test]
async fn releasing_a_host_lease_needs_no_container_provider() {
    let dir = RunDir::new("host-actions-no-plugin");
    let unavailable =
        PluginSettings::at_path("docker", dir.path().join("no-docker-plugin")).expect("settings");
    let router = RoutingExecutor::with_provider_source(
        Arc::new(PluginSource::new(unavailable)),
        dir.path(),
        Retention::Never,
    );
    let ledger = Arc::new(MemoryLedger::default());
    let lease = SandboxLeaseId::new(0);
    router.set_ledger(ledger);
    let scope = ScopeSpec::new(ScopeId::new(0), "scope-0");
    let env = router
        .acquire(&scope, &AcquireContext::bare().with_lease(lease))
        .await
        .expect("host acquire");
    assert!(
        router
            .release(env, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    let report = router.release_lease(lease, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(dir.path().join(executor_sandbox::RUN_ID_FILE).is_file());
}

#[tokio::test]
async fn failed_action_cleanup_preserves_the_host_workspace_until_retry() {
    assert_failed_action_cleanup_preserves_workspace(Some(SandboxLeaseId::new(0))).await;
}

#[tokio::test]
async fn failed_standalone_action_cleanup_preserves_the_host_workspace_until_retry() {
    assert_failed_action_cleanup_preserves_workspace(None).await;
}

async fn assert_failed_action_cleanup_preserves_workspace(lease: Option<SandboxLeaseId>) {
    let dir = RunDir::new("host-actions-cleanup-retry");
    let unavailable =
        PluginSettings::at_path("docker", dir.path().join("no-docker-plugin")).expect("settings");
    let router = RoutingExecutor::with_provider_source(
        Arc::new(PluginSource::new(unavailable)),
        dir.path(),
        Retention::Never,
    );
    router.set_ledger(Arc::new(MemoryLedger::default()));
    let ctx = lease.map_or_else(AcquireContext::bare, |lease| {
        AcquireContext::bare().with_lease(lease)
    });
    let scope = ScopeSpec::new(ScopeId::new(0), "scope-0");
    let handle = router.acquire(&scope, &ctx).await.expect("host acquire");
    handle
        .exec()
        .write_file(Path::new("retained"), b"work")
        .await
        .expect("write workspace contents before failed cleanup");
    let marker = dir
        .path()
        .join("scopes")
        .join(scope.workspace_id.as_str())
        .join("action-host");
    fs::write(&marker, "docker:original-provider")
        .await
        .expect("record the previous action provider");
    let mut report = router.release(handle, ScopeOutcome::Succeeded).await;
    if let Some(lease) = lease {
        assert!(report.is_clean(), "{report:?}");
        report = router.release_lease(lease, ScopeOutcome::Succeeded).await;
    }
    assert!(!report.is_clean(), "{report:?}");
    assert_eq!(
        fs::read(dir.workspace().join("retained"))
            .await
            .expect("failed cleanup retains the workspace"),
        b"work"
    );
    // No action was created in this fixture. Removing its mismatched marker
    // lets the original lease complete its normal cleanup on retry.
    fs::remove_file(marker)
        .await
        .expect("remove the test action provider marker");
    let report = if let Some(lease) = lease {
        router.release_lease(lease, ScopeOutcome::Succeeded).await
    } else {
        let handle = router.acquire(&scope, &ctx).await.expect("retry acquire");
        router.release(handle, ScopeOutcome::Succeeded).await
    };
    assert!(report.is_clean(), "{report:?}");
    assert!(!dir.workspace().exists());
    router.shutdown().await;
}
