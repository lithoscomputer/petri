//! The sandbox executor over the Docker plugin: a container scope whose
//! workspace lives in the sandbox, exec exit codes and signals, workspace
//! I/O over the wire, crash recovery onto the same sandbox, and retention.
//! Skips when no Docker plugin with a reachable daemon is available, unless
//! `PETRI_REQUIRE_DOCKER` says it must be there.

use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use executor::{
    AcquireContext, EnvError, ExecEnv, Executor, ProcessSpec, Retention, ScopeOutcome, ScopeSpec,
    Sig,
};
use executor_sandbox::{MemoryLedger, RoutingExecutor};
use testkit::{RunDir, container_id, container_is_running, is_docker_ready, list_containers};
use tokio::time;

const TEST_IMAGE: &str = "buildpack-deps:noble";
const ALPINE: &str = "alpine:3.20";
/// The scope every test here uses; a bare executor keys its sandbox by the
/// scope id, so this is lease 1.
const SCOPE: ir::ScopeId = ir::ScopeId::new(1);
const INSTANCE: &str = "env-1";

fn executor(dir: &RunDir, retention: Retention) -> RoutingExecutor {
    RoutingExecutor::local(dir.path(), retention).with_run_id(dir.run_id())
}

fn container_scope(image: &str) -> ScopeSpec {
    ScopeSpec::new(SCOPE, INSTANCE).with_runtime(ir::RuntimeSpec::container(image))
}

/// The sandbox's container name for this battery's scope.
fn name(dir: &RunDir) -> String {
    dir.sandbox_name(1)
}

/// Runs `script` under bash and returns (lines, status).
async fn run_bash(env: &dyn ExecEnv, script: &str) -> (Vec<String>, executor::ExitStatus) {
    let mut process = env
        .spawn(ProcessSpec::new("bash", &["-c", script]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let mut seen = Vec::new();
    while let Some(line) = lines.recv().await {
        seen.push(line.line);
    }
    (seen, process.wait().await.expect("wait"))
}

/// Waits for a workspace file to appear through the environment's own reads.
async fn wait_for_workspace_file(env: &dyn ExecEnv, relative: &str, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if env
            .read_file(Path::new(relative))
            .await
            .expect("read")
            .is_some()
        {
            return true;
        }
        time::sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn workspace_len(env: &dyn ExecEnv, relative: &str) -> usize {
    env.read_file(Path::new(relative))
        .await
        .expect("read")
        .map_or(0, |bytes| bytes.len())
}

#[tokio::test]
async fn an_explicit_platform_requires_portable_provider_support() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-portable-platform");
    let executor = executor(&dir, Retention::Never);
    let mut scope = container_scope(ALPINE);
    let ir::RuntimeTarget::Container { options, .. } = &mut scope.runtime.target else {
        unreachable!("a container scope");
    };
    options.platform = Some("linux/amd64".into());
    // The locked Docker provider accepts its own platform option but does
    // not advertise ContainerSpec.platform. Petri must not bypass admission.
    let error = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect_err("portable platform support is not advertised");
    assert!(error.to_string().contains("container.platform"), "{error}");
    assert!(list_containers(&dir.container_prefix()).await.is_empty());
    executor.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_container_step_runs_and_its_workspace_lives_in_the_sandbox() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-step");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&container_scope(TEST_IMAGE), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();
    assert_eq!(env.workspace_path(), "/workspace");
    assert!(
        !env.shares_host_filesystem(),
        "a sandbox owns its workspace"
    );

    let (seen, status) = run_bash(
        &*env,
        "echo written > /workspace/out.txt; echo done; exit 5",
    )
    .await;
    assert_eq!(seen, vec!["done".to_owned()]);
    assert_eq!(status.code, Some(5));

    // The file is in the sandbox, and the only way to it is the sandbox's
    // filesystem facet: nothing on this machine mirrors it.
    let bytes = env
        .read_file(Path::new("out.txt"))
        .await
        .expect("read over the wire")
        .expect("the step wrote it");
    assert_eq!(String::from_utf8_lossy(&bytes).trim(), "written");
    assert!(
        !dir.path().join("scopes").exists(),
        "no host workspace directory is created for a container scope"
    );

    let prefix = dir.container_prefix();
    let report = executor.release(handle, ScopeOutcome::Failed).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(
        report.released_any("sandbox"),
        "never-retention deletes the sandbox on failure too: {report:?}"
    );
    assert!(list_containers(&prefix).await.is_empty());
}

/// Workspace files on an image without bash or base64: every operation goes
/// through the provider's archive path, parents are created for a write,
/// a missing file is `None`, and the read limit is enforced before the
/// whole file crosses the wire.
#[tokio::test(flavor = "multi_thread")]
async fn alpine_workspace_files_go_over_the_wire() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-alpine-fs");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&container_scope(ALPINE), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();

    env.write_file(Path::new("dir/deeper/a.txt"), b"hello")
        .await
        .expect("write creates the missing parents");
    let bytes = env
        .read_file(Path::new("dir/deeper/a.txt"))
        .await
        .expect("read")
        .expect("present");
    assert_eq!(bytes, b"hello");
    assert!(
        env.read_file(Path::new("missing.txt"))
            .await
            .expect("a missing file is not an error")
            .is_none()
    );
    let limited = env
        .read_file_limited(Path::new("dir/deeper/a.txt"), 5)
        .await
        .expect("exactly the limit is fine")
        .expect("present");
    assert_eq!(limited, b"hello");
    let error = env
        .read_file_limited(Path::new("dir/deeper/a.txt"), 2)
        .await
        .expect_err("over the limit is an error");
    assert!(matches!(error, EnvError::Workspace { .. }), "{error}");

    // The step sees the same file, and a cwd that does not exist yet is
    // created with the image's own `mkdir`.
    let mut process = env
        .spawn(ProcessSpec::new("sh", &[
            "-c",
            "cat dir/deeper/a.txt; echo; pwd",
        ]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let mut seen = Vec::new();
    while let Some(line) = lines.recv().await {
        seen.push(line.line);
    }
    assert!(process.wait().await.expect("wait").is_success());
    assert_eq!(seen, vec!["hello".to_owned(), "/workspace".to_owned()]);

    let mut process = env
        .spawn(ProcessSpec::new("pwd", &[]).with_cwd(Some(PathBuf::from("fresh/cwd"))))
        .await
        .expect("spawn in a new cwd");
    let mut lines = process.lines().expect("lines");
    let cwd = lines.recv().await.expect("pwd printed").line;
    assert!(process.wait().await.expect("wait").is_success());
    assert_eq!(cwd, "/workspace/fresh/cwd");

    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// The environment facts a step relies on are read at acquire: the image's
/// `PATH`, the host alias, and no host filesystem.
#[tokio::test(flavor = "multi_thread")]
async fn the_ambient_environment_is_read_at_acquire() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-environment");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&container_scope(ALPINE), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();
    let path = env.ambient_env("PATH").expect("the image has a PATH");
    assert!(path.contains("/bin"), "{path}");
    assert_eq!(env.host_address().unwrap(), "host.docker.internal");
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// `SIGTERM` reaches the step's whole process group inside the sandbox — a
/// backgrounded grandchild dies with it — and the handle reports the signal.
#[tokio::test(flavor = "multi_thread")]
async fn term_reaches_the_step_process_group() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-term-group");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&container_scope(ALPINE), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();
    let mut process = env
        .spawn(ProcessSpec::new("sh", &[
            "-c",
            "( while :; do echo tick >> heartbeat; sleep 0.05; done ) &\necho ready > ready\nsleep 300",
        ]))
        .await
        .expect("spawn");
    assert!(
        wait_for_workspace_file(&*env, "ready", Duration::from_secs(60)).await,
        "the step never started"
    );
    assert!(
        wait_for_workspace_file(&*env, "heartbeat", Duration::from_secs(30)).await,
        "the grandchild never ticked"
    );
    process.signal(Sig::Term).await.expect("signal");
    let status = time::timeout(Duration::from_secs(30), process.wait())
        .await
        .expect("TERM ends the step")
        .expect("wait");
    assert!(!status.is_success(), "{status:?}");
    assert_eq!(
        status.signal,
        Some(15),
        "the signal is reported: {status:?}"
    );

    time::sleep(Duration::from_millis(500)).await;
    let before = workspace_len(&*env, "heartbeat").await;
    time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        workspace_len(&*env, "heartbeat").await,
        before,
        "the grandchild survived: the signal did not reach the process group"
    );
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
}

/// Without a recorded reference, adoption needs the provider to identify
/// the conflicting sandbox with AlreadyExists, not InvalidSpec.
#[tokio::test(flavor = "multi_thread")]
async fn a_name_conflict_is_refused_without_provider_identification() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-fence");
    let first = executor(&dir, Retention::Never);
    let handle = first
        .acquire(&container_scope(ALPINE), &AcquireContext::bare())
        .await
        .expect("first acquire");
    let env = handle.exec();
    // A beater in its own session: what a dead process leaves behind.
    let _process = env
        .spawn(ProcessSpec::new("sh", &[
            "-c",
            "setsid sh -c 'while :; do echo tick >> heartbeat; sleep 0.05; done' &\necho ready > ready\nsleep 300",
        ]))
        .await
        .expect("spawn");
    assert!(
        wait_for_workspace_file(&*env, "heartbeat", Duration::from_secs(60)).await,
        "the beater never started"
    );
    // The crash: no release, and the process's plugin dies with it.
    mem::forget(handle);
    drop(env);
    drop(first);
    time::sleep(Duration::from_millis(300)).await;
    assert!(
        container_is_running(&name(&dir)).await,
        "the container outlives the process that made it"
    );

    let second = executor(&dir, Retention::Never);
    let error = second
        .acquire(&container_scope(ALPINE), &AcquireContext::bare())
        .await
        .expect_err("the provider must identify the conflicting sandbox");
    assert!(error.to_string().contains("name"), "{error}");
    assert!(
        container_id(&name(&dir)).await.is_none(),
        "failed acquire cleaned up"
    );
    second.shutdown().await;
}

/// Retention keeps the *sandbox*, stopped, with its workspace inside; a
/// later executor over the same run dir attaches it, starts it, and finds
/// the workspace as it was left.
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_sandbox_is_stopped_and_reattached_later() {
    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-keep");
    let ledger = Arc::new(MemoryLedger::default());
    let keeper = executor(&dir, Retention::Always);
    keeper.set_ledger(ledger.clone());
    let handle = keeper
        .acquire(&container_scope(ALPINE), &AcquireContext::bare())
        .await
        .expect("acquire");
    handle
        .exec()
        .write_file(Path::new("keep.txt"), b"kept")
        .await
        .expect("write");
    let report = keeper.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(report.kept_any("sandbox"), "{report:?}");
    assert!(
        container_id(&name(&dir)).await.is_some() && !container_is_running(&name(&dir)).await,
        "the sandbox is kept, stopped"
    );

    let later = executor(&dir, Retention::Never);
    later.set_ledger(ledger);
    let handle = later
        .acquire(&container_scope(ALPINE), &AcquireContext::bare())
        .await
        .expect("reattach");
    let bytes = handle
        .exec()
        .read_file(Path::new("keep.txt"))
        .await
        .expect("read")
        .expect("the workspace survived the stop");
    assert_eq!(bytes, b"kept");
    let report = later.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(container_id(&name(&dir)).await.is_none(), "deleted at last");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_container_scope_realizes_and_sweeps_its_service() {
    use std::process::Command;

    use executor::ServiceSpec;

    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-service");
    let executor = executor(&dir, Retention::Never);
    // A declared service becomes a sidecar on a per-sandbox network; the
    // sidecar carries an env var the workload could read by alias. (The
    // image is not a daemon, so this checks the realize/sweep lifecycle, not
    // DNS liveness — sandbox-driver's own conformance covers a live service.)
    let mut scope = container_scope(TEST_IMAGE);
    let mut service = ServiceSpec::new("svc", TEST_IMAGE);
    service.env.insert("SVC_TOKEN".into(), "value".into());
    scope.services = vec![service];

    let handle = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("acquire with a service");

    // The sidecar network is named after this run's job container, so the
    // check is specific and immune to any leftover networks on the daemon.
    let network = format!("{}-net", name(&dir));
    let exists = |name: &str| {
        let out = Command::new("docker")
            .args(["network", "ls", "--format", "{{.Name}}"])
            .output()
            .expect("docker network ls")
            .stdout;
        String::from_utf8(out)
            .expect("utf8")
            .lines()
            .any(|line| line == name)
    };
    assert!(
        exists(&network),
        "the sidecar network is present during the scope"
    );

    executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(!exists(&network), "release swept the sidecar network");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_shot_action_container_shares_the_workspace() {
    use executor::OneShotContainer;

    if !is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("sandbox-docker-one-shot");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&container_scope(TEST_IMAGE), &AcquireContext::bare())
        .await
        .expect("acquire");
    let runner = handle
        .container_runner()
        .expect("a container scope binds a one-shot runner");
    assert_eq!(runner.workspace_path(), "/workspace");
    assert_eq!(runner.host_address().unwrap(), "host.docker.internal");

    // The job writes a file the action reads; the action writes one the
    // job reads: one workspace volume, shared both ways.
    handle
        .exec()
        .write_file(Path::new("from-job.txt"), b"job")
        .await
        .expect("write");
    let spec = OneShotContainer::registry(TEST_IMAGE)
        .with_entrypoint("bash")
        .with_args(&[
            "-c",
            "cat /workspace/from-job.txt; echo from-action > /workspace/action.txt; exit 4",
        ]);
    let mut process = runner.run(spec).await.expect("run one-shot");
    let mut lines = process.lines().expect("lines");
    let mut seen = Vec::new();
    while let Some(line) = lines.recv().await {
        seen.push(line.line);
    }
    let status = process.wait().await.expect("wait");
    assert_eq!(status.code, Some(4));
    assert_eq!(seen, vec!["job".to_owned()]);

    let bytes = handle
        .exec()
        .read_file(Path::new("action.txt"))
        .await
        .expect("read")
        .expect("the action wrote it");
    assert_eq!(String::from_utf8_lossy(&bytes).trim(), "from-action");
    let sandbox_id = container_id(&name(&dir)).await.expect("the sandbox");
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(
        testkit::list_one_shots(&sandbox_id).await.is_empty(),
        "the sandbox's one-shots went with it"
    );
}
