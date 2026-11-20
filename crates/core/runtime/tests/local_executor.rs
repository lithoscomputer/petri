//! The routing executor, at the executor level: routing, the scope-bound
//! one-shot runner in every execution mode, and the crash and cancel sweeps
//! that keep one-shot containers from leaking. A host scope's actions run
//! beside an **action host** — a small sandbox that binds the scope's host
//! workspace — created on the first action and ended with the scope.

use std::path::Path;
use std::time::{Duration, Instant};

use executor::{
    AcquireContext, Executor as _, OneShotContainer, Retention, ScopeOutcome, ScopeSpec,
    ServiceSpec, Sig,
};
use executor_sandbox::{HostExecutor, RoutingExecutor};
use ir::{RuntimeSpec, ScopeId};
use testkit::{
    RunDir, container_id, is_docker_ready, list_containers, list_one_shots, wait_for_file,
};
use tokio::time;

const IMAGE: &str = "alpine:3.20";

fn host_spec() -> ScopeSpec {
    ScopeSpec::new(ScopeId::new(0), "scope-0")
}

fn container_spec() -> ScopeSpec {
    host_spec().with_runtime(RuntimeSpec::container(IMAGE))
}

fn local(dir: &RunDir) -> RoutingExecutor {
    RoutingExecutor::local(dir.path(), Retention::default()).with_run_id(dir.run_id())
}

/// Every log line of a one-shot handle, drained to completion.
async fn drain(handle: &mut Box<dyn executor::ProcessHandle>) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(mut stream) = handle.lines() {
        while let Some(line) = stream.recv().await {
            lines.push(line.line);
        }
    }
    lines
}

/// The action host's container name for scope 0 of the run under `dir`:
/// the run's prefix, `a-`, and the workspace id.
fn action_host(dir: &RunDir) -> String {
    format!("{}a-scope-0", dir.container_prefix())
}

/// Services require a containerized job: a bare host process has no route to a
/// sidecar's network alias, so the host executor refuses and names the fix.
#[tokio::test]
async fn the_host_executor_refuses_services() {
    let dir = RunDir::new("host-refuses-services");
    let executor = HostExecutor::new(dir.path());
    let spec = host_spec().with_services(vec![ServiceSpec::new("redis", "redis:7")]);
    let error = executor
        .acquire(&spec, &AcquireContext::bare())
        .await
        .expect_err("the host executor cannot realize services");
    assert!(error.to_string().contains("containerized job"), "{error}");
}

/// A host scope acquired through the router carries a runner, without touching
/// any daemon. Docker-free machines still run host scopes.
#[tokio::test]
async fn a_local_host_scope_is_bound_to_a_runner() {
    let dir = RunDir::new("local-host-runner");
    let executor = local(&dir);
    let handle = executor
        .acquire(&host_spec(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle
        .container_runner()
        .expect("a runner rides the handle");
    assert_eq!(runner.workspace_path(), "/workspace");
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// Host actions must not run in an unrelated workspace while portable
/// host-workspace mounts are unavailable.
#[tokio::test]
async fn a_host_action_refuses_the_missing_workspace_mount() {
    let dir = RunDir::new("local-one-shot");
    let executor = local(&dir);
    let handle = executor
        .acquire(&host_spec(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle
        .container_runner()
        .expect("a runner rides the handle");

    let spec = OneShotContainer::registry(IMAGE).with_args(&[
        "sh",
        "-c",
        "echo one-shot ran; echo mark > /workspace/mark",
    ]);
    let Err(error) = runner.run(spec).await else {
        panic!("a Host action must not run without its workspace mount");
    };
    assert!(
        error.to_string().contains("host-workspace mount"),
        "{error}"
    );
    assert!(!dir.workspace().join("mark").exists());
    assert!(!dir.path().join("scopes/scope-0/action-host").exists());

    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// Container scope: the handle carries a runner too, and its one-shots see the
/// same workspace as the job container.
#[tokio::test]
async fn a_pure_docker_scope_is_bound_to_a_runner() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("docker-scope-runner");
    let executor = local(&dir);
    let handle = executor
        .acquire(&container_spec(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle
        .container_runner()
        .expect("a runner rides the handle");

    let spec = OneShotContainer::registry(IMAGE).with_args(&[
        "sh",
        "-c",
        "echo from-one-shot > /workspace/shared",
    ]);
    let mut process = runner.run(spec).await.expect("docker run");
    let _ = drain(&mut process).await;
    let status = process.wait().await.expect("wait");
    assert!(status.is_success(), "{status:?}");
    // The job container and the one-shot share the workspace bind mount.
    let shared = handle
        .exec()
        .read_file(Path::new("shared"))
        .await
        .expect("read through the job environment")
        .expect("the file the one-shot wrote");
    assert_eq!(String::from_utf8_lossy(&shared).trim(), "from-one-shot");

    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// The cancel path: a signalled one-shot dies, `--rm` removes it, and nothing
/// of it is left under the scope's one-shot prefix.
#[tokio::test]
#[ignore = "sandbox-driver lacks portable host-workspace mounts"]
async fn a_signalled_one_shot_dies_and_leaves_nothing() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("local-one-shot-cancel");
    let executor = local(&dir);
    let handle = executor
        .acquire(&host_spec(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle.container_runner().expect("a runner");

    let spec = OneShotContainer::registry(IMAGE).with_args(&[
        "sh",
        "-c",
        "echo ready > /workspace/ready; sleep 300",
    ]);
    let mut process = runner.run(spec).await.expect("docker run");
    assert!(
        wait_for_file(&dir.workspace().join("ready"), Duration::from_secs(60)).await,
        "the one-shot never started"
    );
    process.signal(Sig::Term).await.expect("signal");
    let status = time::timeout(Duration::from_secs(30), process.wait())
        .await
        .expect("the signalled container ends")
        .expect("wait");
    assert!(!status.is_success(), "TERM ended it: {status:?}");

    let host = container_id(&action_host(&dir))
        .await
        .expect("the action host exists while the scope lives");
    assert!(
        list_one_shots(&host).await.is_empty(),
        "nothing of the signalled one-shot is left"
    );
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(
        container_id(&action_host(&dir)).await.is_none(),
        "the action host went with the scope"
    );
}

/// The crash path: a one-shot whose process died keeps running beside its
/// action host; the next acquire of the same scope over the same run dir
/// sweeps both away, and release sweeps whatever a live run abandons.
#[tokio::test]
#[ignore = "sandbox-driver lacks portable host-workspace mounts"]
async fn crash_leftovers_are_fenced_by_acquire_and_swept_by_release() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("local-one-shot-crash");

    // "Crash": drop the handle without releasing. The process's plugin dies
    // with its executor, but the action host and the one-shot keep running —
    // exactly what a dead driver leaves behind.
    let crashed_host = {
        let executor = local(&dir);
        let handle = executor
            .acquire(&host_spec(), &AcquireContext::bare())
            .await
            .expect("acquire");
        let runner = handle.container_runner().expect("a runner");
        let spec = OneShotContainer::registry(IMAGE).with_args(&[
            "sh",
            "-c",
            "echo ready > /workspace/ready; sleep 300",
        ]);
        let process = runner.run(spec).await.expect("docker run");
        assert!(
            wait_for_file(&dir.workspace().join("ready"), Duration::from_secs(60)).await,
            "the one-shot never started"
        );
        let host = container_id(&action_host(&dir))
            .await
            .expect("the action host exists");
        drop(process);
        drop(handle);
        host
    };
    // Process death is not container death.
    time::sleep(Duration::from_millis(300)).await;
    assert!(
        !list_one_shots(&crashed_host).await.is_empty(),
        "the leftover one-shot survives its process"
    );

    // A resuming process over the same run dir finds the marker and the
    // label: the fence removes the action host, and its one-shot with it,
    // before the scope is used again.
    let resumed = local(&dir);
    let handle = resumed
        .acquire(&host_spec(), &AcquireContext::bare())
        .await
        .expect("re-acquire");
    assert!(
        list_one_shots(&crashed_host).await.is_empty()
            && container_id(&action_host(&dir)).await.is_none(),
        "acquire fenced the crashed action host and its one-shot away"
    );

    // And a live run's abandoned one-shot goes with the scope: release sweeps.
    let runner = handle.container_runner().expect("a runner");
    let spec = OneShotContainer::registry(IMAGE).with_args(&["sleep", "300"]);
    let _process = runner.run(spec).await.expect("docker run");
    let deadline = Instant::now() + Duration::from_secs(30);
    let host = loop {
        if let Some(host) = container_id(&action_host(&dir)).await
            && !list_one_shots(&host).await.is_empty()
        {
            break host;
        }
        assert!(
            Instant::now() < deadline,
            "the abandoned one-shot never appeared"
        );
        time::sleep(Duration::from_millis(100)).await;
    };
    let report = resumed.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(
        list_one_shots(&host).await.is_empty() && container_id(&action_host(&dir)).await.is_none(),
        "release swept the action host and the abandoned one-shot"
    );
    let prefix = dir.container_prefix();
    assert!(
        list_containers(&prefix).await.is_empty(),
        "nothing of the run is left"
    );
}
