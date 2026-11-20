//! The sandbox executor over the Daytona plugin: a process scope in the
//! runner sandbox, a container job nested inside it, exec exit codes, signals
//! and timeouts, workspace I/O over the wire, output after idle and under a
//! burst, a route to a port, crash recovery onto the same sandbox, retention
//! and reattachment, and the failure modes. Every live test skips without a
//! Daytona credential and a plugin whose backend accepts it, unless
//! `PETRI_REQUIRE_DAYTONA` says the tier must run. `DAYTONA.md` beside this
//! crate maps Fabro's former live suite onto these tests.
//!
//! A live test creates one sandbox from the shared runner snapshot, which the
//! first run of the day may have to build (up to fifteen minutes); the tests
//! after that reuse it. Each test releases what it made and checks the
//! provider holds nothing more for its run.

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, mem};

use executor::{
    AcquireContext, EnvError, ExecEnv, Executor, OneShotContainer, ProcessSpec, Retention,
    SandboxLeaseId, ScopeOutcome, ScopeSpec, Sig,
};
use executor_sandbox::{
    DaytonaResources, RoutingExecutor, SandboxBackend, SandboxCatalog, SandboxOptions,
};
use sandbox_driver::{
    Environment, EnvironmentName, Error, ProviderEntry, ProviderName, SandboxSpec, SandboxState,
    SecretResolver, Source, StaticEnvironments,
};
use testkit::{DaytonaObserver, RunDir, is_daytona_ready};
use tokio::time;

/// The scope every test here uses; a bare executor keys its sandbox by the
/// scope id, so this is lease 1.
const SCOPE: ir::ScopeId = ir::ScopeId::new(1);
const LEASE: u64 = 1;
const INSTANCE: &str = "env-1";
const ALPINE: &str = "alpine:3.20";
/// How long a step inside a fresh sandbox gets to start and be seen.
const START: Duration = Duration::from_secs(120);
/// The lines a fast burst of output prints.
const BURST_LINES: usize = 5000;
/// The port a server inside the sandbox listens on for the preview route.
const PREVIEW_PORT: u16 = 3100;

fn options() -> SandboxOptions {
    SandboxOptions {
        backend: SandboxBackend::Daytona,
        ..Default::default()
    }
}

fn executor(dir: &RunDir, retention: Retention) -> RoutingExecutor {
    RoutingExecutor::with_options(dir.path(), retention, options()).with_run_id(dir.run_id())
}

/// A process scope: the runner sandbox itself, no nested container.
fn process_scope() -> ScopeSpec {
    ScopeSpec::new(SCOPE, INSTANCE)
}

/// A container job, nested inside the runner runner's Docker.
fn container_scope(image: &str) -> ScopeSpec {
    ScopeSpec::new(SCOPE, INSTANCE).with_runtime(ir::RuntimeSpec::container(image))
}

/// Runs `script` under `sh` and returns (stdout and stderr lines in order,
/// status).
async fn run_sh(env: &dyn ExecEnv, script: &str) -> (Vec<String>, executor::ExitStatus) {
    let mut process = env
        .spawn(ProcessSpec::new("sh", &["-c", script]))
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
        time::sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn workspace_len(env: &dyn ExecEnv, relative: &str) -> usize {
    env.read_file(Path::new(relative))
        .await
        .expect("read")
        .map_or(0, |bytes| bytes.len())
}

/// Fails the test when the provider still holds a sandbox of this run.
async fn assert_no_leftovers(observer: &DaytonaObserver, dir: &RunDir) {
    // Daytona accepts deletion before its list endpoint reflects the new
    // state. Bound the wait so a real leak still fails the live test.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let leftovers = observer.sandboxes(&dir.run_id()).await;
        if leftovers.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sandboxes were left behind: {:?}",
            leftovers
                .iter()
                .map(|status| format!("{} ({:?})", status.reference.id, status.state))
                .collect::<Vec<_>>()
        );
        time::sleep(Duration::from_millis(250)).await;
    }
}

/// The runner sandbox: a process step runs in it, its exit code and output
/// cross the wire, the environment facts a step relies on are read at acquire,
/// the acquired sandbox is recorded with the runner snapshot it came from, and
/// the provider holds it with the resources Petri asked for.
#[tokio::test(flavor = "multi_thread")]
async fn a_process_step_runs_in_the_runner_sandbox() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("sandbox-daytona-step");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&process_scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();
    assert_eq!(env.workspace_path(), "/workspace");
    assert!(
        !env.shares_host_filesystem(),
        "a remote sandbox shares nothing with this machine"
    );
    if env::var_os("PETRI_SANDBOX_DAYTONA_HOST_ADDRESS").is_none() {
        assert!(
            matches!(env.host_address(), Err(EnvError::HostUnreachable)),
            "Daytona has no inferred route back to this machine"
        );
    }
    let path = env
        .ambient_env("PATH")
        .expect("the runner image has a PATH");
    assert!(path.contains("/bin"), "{path}");

    let (seen, status) = run_sh(
        &*env,
        "echo hello world | wc -w; echo written > /workspace/out.txt; echo done; exit 5",
    )
    .await;
    assert_eq!(seen, vec!["2".to_owned(), "done".to_owned()]);
    assert_eq!(status.code, Some(5));
    let bytes = env
        .read_file(Path::new("out.txt"))
        .await
        .expect("read over the wire")
        .expect("the step wrote it");
    assert_eq!(String::from_utf8_lossy(&bytes).trim(), "written");
    assert!(
        !dir.path().join("scopes").exists(),
        "no host workspace directory is created for a Daytona scope"
    );

    // What the driver records for the scope, and what the provider holds.
    let sandbox = handle.sandbox();
    assert_eq!(sandbox.provider, "daytona");
    assert_eq!(sandbox.working_directory, "/workspace");
    assert!(
        sandbox
            .snapshot
            .as_deref()
            .is_some_and(|snapshot| snapshot.starts_with("petri-runner-")),
        "the sandbox comes from a shared runner snapshot: {sandbox:?}"
    );
    let status = observer
        .sandbox(&dir.run_id(), LEASE)
        .await
        .expect("the provider lists the run's sandbox by its labels");
    assert_eq!(status.reference.id.as_str(), sandbox.instance.as_str());
    assert_eq!(status.state, SandboxState::Running);
    let wanted = DaytonaResources::default();
    let resources = observer
        .status(&dir.run_id(), LEASE)
        .await
        .expect("Daytona reports the sandbox")
        .resources;
    assert_eq!(
        resources.cpu_cores.map(NonZeroU32::get),
        Some(wanted.cpu_cores)
    );
    assert_eq!(
        resources.memory_mb.map(NonZeroU64::get),
        Some(wanted.memory_mb)
    );
    assert!(resources.disk_mb.is_some());
    // sandbox-driver's status reports
    // no placement, so the configured DAYTONA_TARGET is not checked here.

    let report = executor.release(handle, ScopeOutcome::Failed).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(
        report.released_any("sandbox"),
        "never-retention deletes the sandbox on failure too: {report:?}"
    );
    assert_no_leftovers(&observer, &dir).await;
    executor.shutdown().await;
    observer.shutdown().await;
}

/// `SIGTERM` ends a step and reaches its whole process group in the sandbox, so
/// a backgrounded grandchild dies with it; a step's own deadline ends it as a
/// timeout, promptly, whatever the provider observed on the way.
#[tokio::test(flavor = "multi_thread")]
async fn term_and_a_timeout_end_a_step_in_the_runner_sandbox() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("sandbox-daytona-term");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&process_scope(), &AcquireContext::bare())
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
        wait_for_workspace_file(&*env, "ready", START).await,
        "the step never started"
    );
    assert!(
        wait_for_workspace_file(&*env, "heartbeat", START).await,
        "the grandchild never ticked"
    );
    process.signal(Sig::Term).await.expect("signal");
    let status = time::timeout(Duration::from_secs(60), process.wait())
        .await
        .expect("TERM ends the step")
        .expect("wait");
    assert!(!status.is_success(), "{status:?}");
    assert_eq!(
        status.signal,
        Some(15),
        "the signal is reported: {status:?}"
    );
    time::sleep(Duration::from_secs(1)).await;
    let before = workspace_len(&*env, "heartbeat").await;
    time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        workspace_len(&*env, "heartbeat").await,
        before,
        "the grandchild survived: the signal did not reach the process group"
    );

    let started = Instant::now();
    let mut process = env
        .spawn(ProcessSpec::new("sleep", &["300"]).with_timeout(Some(Duration::from_secs(2))))
        .await
        .expect("spawn with a deadline");
    let status = time::timeout(Duration::from_secs(60), process.wait())
        .await
        .expect("the deadline ends the step")
        .expect("wait");
    assert!(status.timed_out, "{status:?}");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the step outlived its deadline by {:?}",
        started.elapsed()
    );

    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert_no_leftovers(&observer, &dir).await;
    executor.shutdown().await;
    observer.shutdown().await;
}

/// Workspace files in the sandbox: parents are created for a write, a missing
/// file is `None`, the read limit is enforced before the whole file crosses
/// the wire, a directory lists, bytes round-trip exactly (every byte value,
/// and a file past 100 KiB), the step sees the same files, and a cwd that
/// does not exist yet is created.
#[tokio::test(flavor = "multi_thread")]
async fn workspace_files_go_over_the_wire() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("sandbox-daytona-fs");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&process_scope(), &AcquireContext::bare())
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

    let entries = env
        .list_directory(Path::new("."), 1)
        .await
        .expect("the workspace lists");
    assert!(
        entries
            .iter()
            .any(|entry| entry.is_dir && entry.path.trim_end_matches('/').ends_with("dir")),
        "{entries:?}"
    );

    let every_byte: Vec<u8> = (0..=255).collect();
    env.write_file(Path::new("bytes.bin"), &every_byte)
        .await
        .expect("binary write");
    assert_eq!(
        env.read_file(Path::new("bytes.bin"))
            .await
            .expect("binary read")
            .expect("present"),
        every_byte,
        "binary content round-trips exactly"
    );
    let large = "x".repeat(150 * 1024);
    env.write_file(Path::new("large.json"), large.as_bytes())
        .await
        .expect("a file past the offload threshold uploads");
    assert_eq!(
        env.read_file(Path::new("large.json"))
            .await
            .expect("large read")
            .expect("present"),
        large.as_bytes()
    );

    let (seen, status) = run_sh(&*env, "cat dir/deeper/a.txt; echo; pwd; wc -c < large.json").await;
    assert!(status.is_success(), "{status:?}");
    assert_eq!(seen, vec![
        "hello".to_owned(),
        "/workspace".to_owned(),
        (150 * 1024).to_string()
    ]);
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
    assert_no_leftovers(&observer, &dir).await;
    executor.shutdown().await;
    observer.shutdown().await;
}

/// A container job runs in its own image nested inside the runner's Docker, the
/// scope's environment reaches it, its workspace is the runner's, and a
/// one-shot action container shares that workspace both ways.
#[tokio::test(flavor = "multi_thread")]
async fn a_container_job_runs_nested_inside_the_runner_sandbox() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("sandbox-daytona-nested");
    let executor = executor(&dir, Retention::Never);
    let scope =
        container_scope(ALPINE).with_env(BTreeMap::from([("SCOPE_VALUE".into(), "scope".into())]));
    let handle = executor
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();
    assert_eq!(handle.sandbox().provider, "daytona");
    let (seen, status) = run_sh(
        &*env,
        "cat /etc/alpine-release >/dev/null && echo alpine; echo \"$SCOPE_VALUE\"; pwd; echo job > from-job.txt",
    )
    .await;
    assert!(status.is_success(), "{status:?}");
    assert_eq!(seen, vec![
        "alpine".to_owned(),
        "scope".to_owned(),
        "/workspace".to_owned()
    ]);

    let runner = handle
        .container_runner()
        .expect("a container scope binds a one-shot runner");
    assert_eq!(runner.workspace_path(), "/workspace");
    let spec = OneShotContainer::registry(ALPINE).with_args(&[
        "sh",
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
    let bytes = env
        .read_file(Path::new("action.txt"))
        .await
        .expect("read")
        .expect("the action wrote it");
    assert_eq!(String::from_utf8_lossy(&bytes).trim(), "from-action");

    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert_no_leftovers(&observer, &dir).await;
    executor.shutdown().await;
    observer.shutdown().await;
}

/// Retention keeps the sandbox, stopped, with its workspace inside; a later
/// executor over the same run dir attaches it, starts it, and finds the
/// workspace as it was left; its release deletes it at last.
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_sandbox_is_stopped_and_reattached_later() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("sandbox-daytona-keep");
    let keeper = executor(&dir, Retention::Always);
    let handle = keeper
        .acquire(&process_scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    handle
        .exec()
        .write_file(Path::new("keep.txt"), b"kept")
        .await
        .expect("write");
    let kept_id = handle.sandbox().instance.clone();
    let report = keeper.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(report.kept_any("sandbox"), "{report:?}");
    keeper.shutdown().await;
    // TODO(sandbox-driver): the legacy Daytona stop returned once the
    // sandbox read `stopped`; the contract runtime returns while Daytona
    // still lists it `stopping`. Wait for it to settle.
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        let status = observer
            .sandbox(&dir.run_id(), LEASE)
            .await
            .expect("the sandbox is kept");
        if status.state != SandboxState::Stopping || Instant::now() > deadline {
            break status;
        }
        time::sleep(Duration::from_secs(2)).await;
    };
    assert_eq!(status.state, SandboxState::Stopped, "kept, stopped");

    let later = executor(&dir, Retention::Never);
    let handle = later
        .acquire(&process_scope(), &AcquireContext::bare())
        .await
        .expect("reattach");
    assert_eq!(
        handle.sandbox().instance,
        kept_id,
        "the same sandbox was attached, not replaced"
    );
    let bytes = handle
        .exec()
        .read_file(Path::new("keep.txt"))
        .await
        .expect("read")
        .expect("the workspace survived the stop");
    assert_eq!(bytes, b"kept");
    let report = later.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert_no_leftovers(&observer, &dir).await;
    later.shutdown().await;
    observer.shutdown().await;
}

/// Crash recovery attaches the *same* sandbox and fences it with one stop
/// and one start, so whatever a dead execution left running is gone before
/// any holder resumes, and the workspace survives.
#[tokio::test(flavor = "multi_thread")]
async fn a_reacquire_attaches_and_fences_the_crashed_predecessor() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("sandbox-daytona-fence");
    let first = executor(&dir, Retention::Never);
    let handle = first
        .acquire(&process_scope(), &AcquireContext::bare())
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
        wait_for_workspace_file(&*env, "heartbeat", START).await,
        "the beater never started"
    );
    let crashed_id = handle.sandbox().instance.clone();

    // The crash: no release, and the process's plugin dies with it.
    mem::forget(handle);
    drop(env);
    first.shutdown().await;
    drop(first);
    assert!(
        observer.is_running(&dir.run_id(), LEASE).await,
        "the sandbox outlives the process that made it"
    );

    let second = executor(&dir, Retention::Never);
    let handle2 = second
        .acquire(&process_scope(), &AcquireContext::bare())
        .await
        .expect("second acquire");
    assert_eq!(
        handle2.sandbox().instance,
        crashed_id,
        "the same sandbox was attached, not replaced"
    );
    let env2 = handle2.exec();
    let before = workspace_len(&*env2, "heartbeat").await;
    assert!(before > 0, "the workspace survived the fence");
    time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        workspace_len(&*env2, "heartbeat").await,
        before,
        "the crashed execution's beater kept writing: the fence missed it"
    );

    let report = second.release(handle2, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert_no_leftovers(&observer, &dir).await;
    second.shutdown().await;
    observer.shutdown().await;
}

/// The sandbox still answers after sitting idle, and a fast burst of output
/// arrives whole: every line, or the loss the provider counted reported on
/// the step's stderr with the exit status intact.
#[tokio::test(flavor = "multi_thread")]
async fn output_survives_idle_and_a_burst_is_delivered_or_its_loss_is_counted() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("sandbox-daytona-output");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&process_scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();

    for idle in [
        Duration::ZERO,
        Duration::from_secs(3),
        Duration::from_secs(5),
    ] {
        time::sleep(idle).await;
        let (seen, status) = run_sh(&*env, "echo alive").await;
        assert!(status.is_success(), "after {idle:?} idle: {status:?}");
        assert_eq!(seen, vec!["alive".to_owned()], "after {idle:?} idle");
    }

    let mut process = env
        .spawn(ProcessSpec::new("sh", &[
            "-c",
            &format!("seq 1 {BURST_LINES}; echo burst-done >&2; exit 3"),
        ]))
        .await
        .expect("spawn");
    let mut lines = process.lines().expect("lines");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(line) = lines.recv().await {
        match line.stream {
            ir::LogStream::Stdout => stdout.push(line.line),
            ir::LogStream::Stderr => stderr.push(line.line),
        }
    }
    let status = process.wait().await.expect("wait");
    assert_eq!(status.code, Some(3), "the loss never costs the exit status");
    let loss = stderr
        .iter()
        .find(|line| line.starts_with("[sandbox] ") && line.ends_with("dropped by the provider"));
    if let Some(loss) = loss {
        assert!(
            stdout.len() < BURST_LINES,
            "a counted loss with every line delivered: {loss}"
        );
    } else {
        let expected: Vec<String> = (1..=BURST_LINES).map(|n| n.to_string()).collect();
        assert_eq!(stdout, expected, "the burst arrived whole, in order");
    }
    assert!(stderr.contains(&"burst-done".to_owned()), "{stderr:?}");

    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert_no_leftovers(&observer, &dir).await;
    executor.shutdown().await;
    observer.shutdown().await;
}

/// A port inside the sandbox is reachable from this machine through the
/// provider's preview URL, with the headers it needs; the route is released
/// with the port and the sandbox with the scope.
#[tokio::test(flavor = "multi_thread")]
async fn a_preview_url_reaches_a_port_inside_the_sandbox() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("sandbox-daytona-preview");
    let executor = executor(&dir, Retention::Never);
    let handle = executor
        .acquire(&process_scope(), &AcquireContext::bare())
        .await
        .expect("acquire");
    let env = handle.exec();
    env.write_file(Path::new("site/index.html"), b"served from the sandbox")
        .await
        .expect("write");
    let mut server = env
        .spawn(
            ProcessSpec::new("python3", &[
                "-m",
                "http.server",
                &PREVIEW_PORT.to_string(),
                "--bind",
                "0.0.0.0",
            ])
            .with_cwd(Some(PathBuf::from("site"))),
        )
        .await
        .expect("the runner image has Python 3");

    let preview = env
        .preview_url(PREVIEW_PORT)
        .await
        .expect("the provider offers a route")
        .expect("Daytona routes ports through preview URLs");
    assert!(preview.url.starts_with("https://"), "{}", preview.url);
    let client = reqwest::Client::new();
    let deadline = Instant::now() + START;
    let body = loop {
        let mut request = client.get(&preview.url);
        for (name, value) in &preview.headers {
            request = request.header(name, value);
        }
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                break response.text().await.expect("body");
            }
            Ok(_) | Err(_) if Instant::now() < deadline => {
                time::sleep(Duration::from_secs(2)).await;
            }
            Ok(response) => panic!("the preview answered {}", response.status()),
            Err(error) => panic!("the preview never answered: {error}"),
        }
    };
    assert!(body.contains("served from the sandbox"), "{body}");
    env.release_preview_url(PREVIEW_PORT)
        .await
        .expect("a route is released");
    env.release_preview_url(PREVIEW_PORT)
        .await
        .expect("releasing twice succeeds");

    server.signal(Sig::Term).await.expect("signal");
    let _ = time::timeout(Duration::from_secs(60), server.wait()).await;
    let report = executor.release(handle, ScopeOutcome::Succeeded).await;
    assert!(report.is_clean(), "{report:?}");
    assert_no_leftovers(&observer, &dir).await;
    executor.shutdown().await;
    observer.shutdown().await;
}

/// A nested job whose image does not exist fails the scope routably at
/// acquire, and the lease's release leaves nothing on the provider.
#[tokio::test(flavor = "multi_thread")]
async fn a_bad_image_fails_a_nested_job_routably() {
    if !is_daytona_ready().await {
        return;
    }
    let observer = DaytonaObserver::from_env();
    let dir = RunDir::new("sandbox-daytona-bad-image");
    let executor = executor(&dir, Retention::Never);
    let error = executor
        .acquire(
            &container_scope("petri-tests/no-such-image:never"),
            &AcquireContext::bare(),
        )
        .await
        .expect_err("a missing image cannot be a job");
    assert!(
        matches!(error, EnvError::Backend { .. }),
        "the failure is the backend's, routable: {error}"
    );
    assert!(!error.to_string().trim().is_empty());
    let report = executor
        .release_lease(SandboxLeaseId::new(LEASE), ScopeOutcome::Failed)
        .await;
    assert!(report.is_clean(), "{report:?}");
    assert_no_leftovers(&observer, &dir).await;
    executor.shutdown().await;
    observer.shutdown().await;
}

/// No daemon, no key: a Daytona scope whose entry names a credential the
/// application does not have fails at acquire with the credential named, as
/// a step's failure and not a crash. Runs everywhere.
#[tokio::test]
async fn a_missing_daytona_credential_fails_the_scope_routably() {
    struct NoSecrets;
    #[async_trait::async_trait]
    impl SecretResolver for NoSecrets {
        async fn resolve(&self, _: &str) -> Result<Option<String>, Error> {
            Ok(None)
        }
    }
    const SECRET: &str = "PETRI_TEST_ABSENT_DAYTONA_KEY";
    let dir = RunDir::new("sandbox-daytona-no-credential");
    let name = ProviderName::try_new("daytona").expect("name");
    let entry: ProviderEntry =
        serde_json::from_value(serde_json::json!({ "plugin": "daytona", "secrets": [SECRET] }))
            .expect("entry");
    let catalog = SandboxCatalog::new(
        [(name.clone(), entry)],
        Arc::new(StaticEnvironments(BTreeMap::from([(
            EnvironmentName::try_new("daytona").expect("name"),
            Environment {
                provider: name,
                spec:     SandboxSpec::new(Source::Image(ALPINE.to_owned())),
            },
        )]))),
        SandboxCatalog::builtin_registry(),
        Arc::new(NoSecrets),
    );
    let executor = RoutingExecutor::with_catalog(catalog, dir.path(), Retention::Never, options());
    let error = executor
        .acquire(&process_scope(), &AcquireContext::bare())
        .await
        .expect_err("no credential, no sandbox");
    let message = error.to_string();
    assert!(
        matches!(error, EnvError::Backend { .. }),
        "routable, not a panic: {message}"
    );
    assert!(
        message.contains(SECRET),
        "the failure names the credential: {message}"
    );
    executor.shutdown().await;
}
