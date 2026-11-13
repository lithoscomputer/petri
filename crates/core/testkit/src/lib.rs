//! Shared test scaffolding.
//!
//! What every end-to-end harness needs and none should copy: a run directory
//! that cleans itself up, readers over a [`ExecutionReport`], the replay canary
//! as an assertion, a step that ignores cancellation, and a step that needs a
//! capability. Dev-dependency only; never published.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{env, fs, process, thread};

use driver::ExecutionReport;
use executor::Retention;
use executor_sandbox::{LEASE_LABEL, RUN_LABEL, SandboxBackend, SandboxCatalog, SandboxOptions};
use ir::{Graph, GraphBuilder, NodeId, ScopeId, StepRef, Value};
use sandbox_driver::contract::{LabelFilter, SandboxState, SandboxSummary, Sandboxes, Status};
use serde::Deserialize;
use serde_json::json;
use steps::PROCESS_KIND;
use store::{Access, OwnerId, RunDirStore, RunLogs};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::task::yield_now;
use tokio::time;

pub mod in_process;
pub mod run_store;
pub mod sim;

/// A process-unique counter, for run ids and directory names.
pub fn unique_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// A run directory that cleans itself up.
pub struct RunDir {
    path: PathBuf,
}

impl RunDir {
    pub fn new(label: &str) -> Self {
        let unique = format!("{label}-{}-{}", process::id(), unique_id());
        let path = env::temp_dir().join("petri-tests").join(unique);
        fs::create_dir_all(&path).unwrap_or_else(|e| {
            panic!(
                "could not create the test run dir `{}`: {e}",
                path.display()
            )
        });
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The workspace the host executor gives scope 0.
    pub fn workspace(&self) -> PathBuf {
        self.workspace_of(ScopeId::new(0))
    }

    pub fn workspace_of(&self, scope: ScopeId) -> PathBuf {
        self.path
            .join("scopes")
            .join(format!("scope-{}", scope.raw()))
            .join("work")
    }

    pub fn logs(&self) -> PathBuf {
        self.path.join("logs")
    }

    /// The run id a router with no coordinator uses over this directory.
    pub fn run_id(&self) -> String {
        executor_sandbox::RunIdentity::for_run_dir(self.path.clone())
            .run_id()
            .to_owned()
    }

    /// `petri-<run id>-`, for a router built with [`RunDir::run_id`].
    pub fn container_prefix(&self) -> String {
        format!("petri-{}-", self.run_id())
    }

    /// The sandbox name of `lease` for a router built with
    /// [`RunDir::run_id`].
    pub fn sandbox_name(&self, lease: u64) -> String {
        sandbox_name_of(&self.run_id(), lease)
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A `run:` script step config.
pub fn script(run: &str) -> Value {
    json!({ "run": run })
}

pub fn script_with(run: &str, extra: &Value) -> Value {
    let mut config = json!({ "run": run });
    if let (Some(base), Some(extra)) = (config.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    config
}

/// Add a process node running `run`.
pub fn add_script(b: &mut GraphBuilder, name: &str, scope: ScopeId, run: &str) -> NodeId {
    b.add_node(name, scope, StepRef::new(PROCESS_KIND, script(run)))
}

/// A step kind that ignores `Control::Cancel` and never returns, so the
/// driver's hard deadline is the only thing that can end it.
pub struct WedgedStep;

pub const WEDGED_KIND: ir::StepKindId = ir::StepKindId::new_static("wedged");

impl ir::StepKind for WedgedStep {
    fn id(&self) -> ir::StepKindId {
        WEDGED_KIND
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "wedged"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for WedgedStep {
    async fn run(&self, mut ctx: steps::StepCtx) -> ir::Outcome {
        ctx.log(ir::LogStream::Stdout, "wedged step is running")
            .await;
        // Receive the cancel and deliberately do nothing about it.
        let _ = ctx.control.recv().await;
        loop {
            time::sleep(Duration::from_secs(3600)).await;
        }
    }
}

/// A step kind that returns configured [`ir::SpliceRequest`]s, so batteries
/// drive uploads with no component. Config shape:
/// `{ "requests": [<SpliceRequest>...], "output": <value?> }` — `requests`
/// deserialized as-is, `output` returned as the outcome's output.
pub struct SpliceStep;

pub const SPLICE_KIND: ir::StepKindId = ir::StepKindId::new_static("splice");

/// The config a [`SpliceStep`] node takes, for building graphs in tests.
pub fn splice_config(requests: &[ir::SpliceRequest], output: &Value) -> Value {
    json!({
        "requests": requests,
        "output": output,
    })
}

/// Typed config for [`SpliceStep`].
#[doc(hidden)]
#[derive(Deserialize)]
pub struct SpliceConfig {
    #[serde(default)]
    requests: Vec<ir::SpliceRequest>,
    #[serde(default)]
    output:   Value,
}

#[async_trait::async_trait]
impl steps::Step for SpliceStep {
    const NAME: &'static str = "splice";

    type Config = SpliceConfig;

    async fn run(&self, config: Self::Config, _ctx: steps::StepCtx) -> ir::Outcome {
        ir::Outcome::success(config.output).with_splices(config.requests)
    }
}

/// The handle a host registers as a capability: a concrete type over whatever
/// it wraps. [`GreetStep`] requires it.
pub struct Greeting(pub &'static str);

/// A step kind that requires the [`Greeting`] capability and outputs its text —
/// or, with no host having registered one, fails routably with
/// `capability_unavailable`.
pub struct GreetStep;

pub const GREET_KIND: ir::StepKindId = ir::StepKindId::new_static("greet");

impl ir::StepKind for GreetStep {
    fn id(&self) -> ir::StepKindId {
        GREET_KIND
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the `StepKind` trait fixes this signature; an impl cannot widen the lifetime"
    )]
    fn name(&self) -> &str {
        "greet"
    }
}

#[async_trait::async_trait]
impl steps::StepRunner for GreetStep {
    async fn run(&self, ctx: steps::StepCtx) -> ir::Outcome {
        match ctx.require_capability::<Greeting>() {
            Ok(greeting) => ir::Outcome::success(json!(greeting.0)),
            Err(failure) => failure.into(),
        }
    }
}

/// Wait for a file to appear, so a test can act once a step is really running.
pub async fn wait_for_file(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        time::sleep(Duration::from_millis(20)).await;
    }
    false
}

pub fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| m.len())
}

/// Every log line the run recorded, in order.
pub fn log_lines(report: &ExecutionReport) -> Vec<String> {
    report
        .state
        .log
        .events()
        .filter_map(|e| match e {
            engine::Event::StepProgressRecorded {
                ev: ir::StepEvent::Log { line, .. },
                ..
            } => Some(line.clone()),
            _ => None,
        })
        .collect()
}

/// The status a node ended with.
pub fn status_of(report: &ExecutionReport, name: &str) -> Option<String> {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .map(|r| r.outcome.status.tag().to_string())
}

pub fn output_of(report: &ExecutionReport, name: &str) -> Value {
    report
        .state
        .history()
        .iter()
        .find(|r| r.name == name)
        .map_or(Value::Null, |r| r.outcome.output.clone())
}

/// Names of the nodes that actually started, in order.
pub fn started(report: &ExecutionReport) -> Vec<String> {
    report
        .state
        .log
        .records()
        .iter()
        .filter_map(|r| match &r.event {
            engine::Event::StepStarted { firing, .. } => Some(*firing),
            _ => None,
        })
        .filter_map(|firing| {
            report
                .state
                .history()
                .iter()
                .find(|h| h.firing == firing)
                .map(|h| h.name.to_string())
        })
        .collect()
}

/// Replay the run's log and assert it comes back byte-identical.
pub fn assert_replay_identical(graph: &Graph, report: &ExecutionReport) {
    if let Err(mismatch) = engine::verify_replay(graph.clone(), &report.state.log) {
        panic!("replay was not byte-identical: {mismatch}");
    }
}

/// Exactly one terminal `StepFinished` per firing, however many cancels, kills,
/// timeouts, or step returns raced to produce one.
pub fn assert_one_terminal_per_firing(report: &ExecutionReport) {
    let mut finishes: BTreeMap<u64, usize> = BTreeMap::new();
    for event in report.state.log.events() {
        if let engine::Event::StepFinished { firing, .. } = event {
            *finishes.entry(firing.raw()).or_default() += 1;
        }
    }
    assert!(
        finishes.values().all(|n| *n == 1),
        "one terminal event per firing: {finishes:?}"
    );
}

/// The catalog a run with `backend` and no catalog file places scopes on,
/// opened from this process's environment.
fn standard_sandboxes(backend: SandboxBackend) -> Result<Sandboxes, String> {
    SandboxCatalog::from_options(&SandboxOptions {
        backend,
        ..SandboxOptions::default()
    })?
    .open()
    .map_err(|error| error.to_string())
}

/// Whether `entry` of the standard catalog activates from this process's
/// environment and reports itself healthy.
async fn entry_health(backend: SandboxBackend, entry: &str) -> Result<(), String> {
    let sandboxes = standard_sandboxes(backend)?;
    let health = match sandboxes.provider(entry) {
        Ok(provider) => provider.health().await.map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let _ = sandboxes.shutdown().await;
    match health? {
        health if health.healthy => Ok(()),
        health => Err(health
            .message
            .unwrap_or_else(|| format!("the {entry} provider reports itself unhealthy"))),
    }
}

/// Whether the built-in Docker provider reaches a healthy daemon from this
/// process's environment (`DOCKER_HOST`, or the default socket): the same
/// path every container scope takes.
pub async fn is_docker_available() -> bool {
    entry_health(SandboxBackend::Docker, "docker").await.is_ok()
}

/// The run under `run_dir`, opened for reading: no lease, so a live run
/// can be read too.
pub async fn read_run_dir(run_dir: &Path) -> Arc<dyn RunLogs> {
    RunDirStore::new(run_dir)
        .open_stored(Access::Read)
        .await
        .unwrap_or_else(|error| panic!("the run under `{}` opens: {error}", run_dir.display()))
}

/// The run under `run_dir`, opened for writing under a fresh owner: what a
/// test does to change a stored run between two coordinators. Drop the
/// handle before the next coordinator opens the run.
pub async fn write_run_dir(run_dir: &Path) -> Arc<dyn RunLogs> {
    RunDirStore::new(run_dir)
        .open_stored(Access::Write {
            owner: OwnerId::mint(),
        })
        .await
        .unwrap_or_else(|error| panic!("the run under `{}` opens: {error}", run_dir.display()))
}

/// Wait until the run under `run_dir` can be opened for writing: a
/// coordinator dropped mid-run (a crash test) releases the run's lease when
/// the last of its tasks is dropped, which takes a turn of the scheduler.
/// Panics when the lease is still held after `limit`.
pub async fn wait_for_run_dir_release(run_dir: &Path, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        yield_now().await;
        match RunDirStore::new(run_dir)
            .open_stored(Access::Write {
                owner: OwnerId::mint(),
            })
            .await
        {
            Ok(handle) => {
                drop(handle);
                return;
            }
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the run under `{}` is still held: {error}",
                    run_dir.display()
                );
                time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

/// The run id every sandbox of the run under `run_dir` is labelled with:
/// the run key the coordinator recorded in `run.json`, or, for a driver
/// with no coordinator, the id its router derives from the directory.
///
/// The store creates `run.json` before it fills it and truncates it before
/// every rewrite, so a reader that polls while a run starts or ends can see
/// the file empty or cut short. Such a read is retried briefly; only a file
/// that stays unreadable is an error.
pub fn recorded_run_id(run_dir: &Path) -> String {
    let path = run_dir.join(store::RUN_FILE);
    let mut attempts = 0;
    loop {
        let Ok(bytes) = fs::read(&path) else {
            return executor_sandbox::RunIdentity::for_run_dir(run_dir.to_path_buf())
                .run_id()
                .to_owned();
        };
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(run) => {
                return run["key"]
                    .as_str()
                    .expect("run.json names the run key")
                    .to_owned();
            }
            Err(error) => {
                assert!(attempts < 20, "run.json is JSON: {error}");
                attempts += 1;
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// The name of the container sandbox for `lease` of the run under
/// `run_dir`: `petri-<run id>-l<lease>`. A bare driver keys each scope's
/// sandbox by the scope id, so scope 0 is lease 0; a coordinator mints
/// leases in scope order from 0.
pub fn sandbox_name(run_dir: &Path, lease: u64) -> String {
    sandbox_name_of(&recorded_run_id(run_dir), lease)
}

/// The name of the container sandbox for `lease` of the run with `run_id`.
pub fn sandbox_name_of(run_id: &str, lease: u64) -> String {
    format!("petri-{run_id}-l{lease}")
}

/// Runs a Docker inspection command, bounding daemon waits and ending the
/// client when a caller stops waiting.
async fn docker_output<'a>(args: impl IntoIterator<Item = &'a str>) -> Option<Vec<u8>> {
    let output = time::timeout(
        Duration::from_secs(30),
        Command::new("docker")
            .args(args)
            .stdin(process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    output.status.success().then_some(output.stdout)
}

/// `docker exec` a command in a running container; its stdout on success.
async fn container_exec(container: &str, args: &[&str]) -> Option<Vec<u8>> {
    docker_output(["exec", container].into_iter().chain(args.iter().copied())).await
}

/// The bytes of `path` inside a running container, through the `docker`
/// CLI: how a test sees a workspace that lives in its sandbox. `None`
/// when the file or the container is not there.
pub async fn container_read(container: &str, path: &str) -> Option<Vec<u8>> {
    container_exec(container, &["cat", path]).await
}

/// Writes `contents` to `path` inside a running container.
pub async fn container_write(container: &str, path: &str, contents: &str) -> bool {
    let script = "printf '%s' \"$1\" > \"$2\"";
    container_exec(container, &["sh", "-c", script, "sh", contents, path])
        .await
        .is_some()
}

/// Waits for `path` to exist inside a running container.
pub async fn wait_for_container_file(container: &str, path: &str, limit: Duration) -> bool {
    time::timeout(limit, async {
        loop {
            if container_exec(container, &["test", "-e", path])
                .await
                .is_some()
            {
                return;
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .is_ok()
}

/// The daemon's id for the container named `name`, when it exists.
pub async fn container_id(name: &str) -> Option<String> {
    let output = docker_output(["inspect", "--format", "{{.Id}}", name]).await?;
    Some(String::from_utf8_lossy(&output).trim().to_owned())
}

/// Whether the container named `name` is running.
pub async fn container_is_running(name: &str) -> bool {
    docker_output(["inspect", "--format", "{{.State.Running}}", name])
        .await
        .is_some_and(|output| String::from_utf8_lossy(&output).trim() == "true")
}

/// The one-shot containers the Docker provider ran beside the sandbox with
/// container id `sandbox_id`, by the label the provider stamps on them.
pub async fn list_one_shots(sandbox_id: &str) -> Vec<String> {
    let Some(output) = docker_output([
        "ps",
        "-a",
        "--filter",
        &format!("label=sh.sandbox-driver.one-shot={sandbox_id}"),
        "--format",
        "{{.Names}}",
    ])
    .await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(String::from)
        .collect()
}

/// The names of every container on the local daemon that start with
/// `prefix`, through the `docker` CLI: the oracle a leak check compares
/// Petri's own accounting against. Empty when there is no CLI or daemon.
pub async fn list_containers(prefix: &str) -> Vec<String> {
    let Some(output) = docker_output(["ps", "-a", "--format", "{{.Names}}"]).await else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output)
        .lines()
        .map(str::trim)
        .filter(|name| name.starts_with(prefix))
        .map(String::from)
        .collect()
}

/// The skip-or-require convention every Docker battery shares: skip loudly
/// without a daemon, unless `PETRI_REQUIRE_DOCKER` says a silent skip must be
/// a failure (CI cannot tell a skipped battery from a passing one).
#[expect(
    clippy::print_stderr,
    reason = "the skip notice belongs to the test runner's output, which no subscriber reads"
)]
pub async fn is_docker_ready() -> bool {
    if is_docker_available().await {
        return true;
    }
    assert!(
        !env::var("PETRI_REQUIRE_DOCKER").is_ok_and(|v| !v.is_empty()),
        "PETRI_REQUIRE_DOCKER is set, but the Docker provider reaches no daemon"
    );
    eprintln!("skipping: no reachable Docker daemon");
    false
}

/// The variable that turns a skipped Daytona battery into a failure.
pub const REQUIRE_DAYTONA: &str = "PETRI_REQUIRE_DAYTONA";

/// Why the live Daytona tier cannot run from this process's environment, or
/// `Ok` once the built-in Daytona provider activated and its backend accepted
/// the credentials, `DAYTONA_API_KEY` or `DAYTONA_JWT_TOKEN`. The credential
/// check comes first so a machine without one never contacts Daytona.
pub async fn daytona_availability() -> Result<(), String> {
    let credential = ["DAYTONA_API_KEY", "DAYTONA_JWT_TOKEN"]
        .iter()
        .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()));
    if !credential {
        return Err(
            "no Daytona credential (DAYTONA_API_KEY or DAYTONA_JWT_TOKEN) in the environment"
                .to_owned(),
        );
    }
    entry_health(SandboxBackend::Daytona, "daytona").await
}

/// The skip-or-require convention of the live Daytona tier, the Docker
/// battery's ([`is_docker_ready`]) with its own variable: skip loudly
/// without a credential or a plugin whose backend accepts it, unless
/// `PETRI_REQUIRE_DAYTONA` says a silent skip must be a failure. The failure
/// names the reason, so a CI run with the wrong plugin path or a rejected
/// key reads as that and not as a missing test.
#[expect(
    clippy::print_stderr,
    reason = "the skip notice belongs to the test runner's output, which no subscriber reads"
)]
pub async fn is_daytona_ready() -> bool {
    match daytona_availability().await {
        Ok(()) => true,
        Err(reason) => {
            assert!(
                !env::var(REQUIRE_DAYTONA).is_ok_and(|v| !v.is_empty()),
                "{REQUIRE_DAYTONA} is set, but {reason}"
            );
            eprintln!("skipping: {reason}");
            false
        }
    }
}

/// A view of a run's Daytona sandboxes from outside the executor under test:
/// the oracle the Daytona battery compares against, as `docker inspect`
/// through [`container_id`] is for the Docker one. It opens its own catalog
/// from this process's environment, so it reaches exactly what the
/// executor's Daytona entry reaches, and finds by the labels every sandbox of
/// a run carries. Shut it down when done.
pub struct DaytonaObserver {
    sandboxes: Sandboxes,
}

impl DaytonaObserver {
    /// Opens the catalog. Call after [`is_daytona_ready`] said yes.
    pub fn from_env() -> Self {
        Self {
            sandboxes: standard_sandboxes(SandboxBackend::Daytona)
                .expect("the Daytona catalog opened once for the gate, so it opens again"),
        }
    }

    /// Every sandbox of the run with `run_id` that still exists on Daytona,
    /// by id: the leak check after a release.
    pub async fn sandboxes(&self, run_id: &str) -> Vec<SandboxSummary> {
        let found = self
            .sandboxes
            .find(LabelFilter::eq(RUN_LABEL, run_id))
            .await
            .expect("the catalog searches its entries");
        if let Some(failure) = found
            .failures
            .iter()
            .find(|failure| failure.entry.as_str() == "daytona")
        {
            panic!("Daytona lists the run's sandboxes: {}", failure.error);
        }
        found
            .sandboxes
            .into_iter()
            .filter(|summary| summary.reference.provider.as_str() == "daytona")
            .filter(|summary| {
                !matches!(
                    summary.state,
                    SandboxState::Deleted | SandboxState::Deleting
                )
            })
            .collect()
    }

    /// The sandbox of `lease` in the run with `run_id`, when it exists.
    pub async fn sandbox(&self, run_id: &str, lease: u64) -> Option<SandboxSummary> {
        let lease = lease.to_string();
        self.sandboxes(run_id)
            .await
            .into_iter()
            .find(|summary| summary.labels.get(LEASE_LABEL) == Some(&lease))
    }

    /// Whether the sandbox of `lease` exists and is running.
    pub async fn is_running(&self, run_id: &str, lease: u64) -> bool {
        self.sandbox(run_id, lease)
            .await
            .is_some_and(|summary| summary.state == SandboxState::Running)
    }

    /// Whether the sandbox of `lease` exists and is stopped: what retention
    /// leaves behind.
    pub async fn is_stopped(&self, run_id: &str, lease: u64) -> bool {
        self.sandbox(run_id, lease)
            .await
            .is_some_and(|summary| summary.state == SandboxState::Stopped)
    }

    /// Whether the sandbox of `lease` is stopped within 30 seconds. Daytona
    /// accepts a stop before its list endpoint reports the new state.
    pub async fn eventually_stopped(&self, run_id: &str, lease: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.is_stopped(run_id, lease).await {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// The run's sandboxes still listed after up to 30 seconds: Daytona
    /// accepts a delete before its list endpoint stops reporting the
    /// sandbox, so an empty list may take a moment.
    pub async fn eventually_gone(&self, run_id: &str) -> Vec<SandboxSummary> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = self.sandboxes(run_id).await;
            if left.is_empty() || Instant::now() >= deadline {
                return left;
            }
            time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// What Daytona reports of the sandbox of `lease`, through a second
    /// attachment: its image, snapshot and allocation.
    pub async fn status(&self, run_id: &str, lease: u64) -> Option<Status> {
        let summary = self.sandbox(run_id, lease).await?;
        let sandbox = self.sandboxes.attach(&summary.reference).await.ok()?;
        sandbox.status().await.ok()
    }

    /// The bytes of `path` inside the running sandbox of `lease`, through a
    /// second attachment: what a test reads while the executor under test
    /// holds the sandbox, or after it crashed. `None` when the sandbox or
    /// the file is absent.
    pub async fn read(&self, run_id: &str, lease: u64, path: &str) -> Option<Vec<u8>> {
        let summary = self.sandbox(run_id, lease).await?;
        let sandbox = self.sandboxes.attach(&summary.reference).await.ok()?;
        let mut stream = sandbox.files().get(path, None).await.ok()?;
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.ok()?;
        Some(bytes)
    }

    /// Writes `contents` to `path` inside the running sandbox of `lease`.
    pub async fn write(&self, run_id: &str, lease: u64, path: &str, contents: &[u8]) -> bool {
        let Some(summary) = self.sandbox(run_id, lease).await else {
            return false;
        };
        let Ok(sandbox) = self.sandboxes.attach(&summary.reference).await else {
            return false;
        };
        sandbox
            .files()
            .put(path, Box::pin(Cursor::new(contents.to_vec())))
            .await
            .is_ok()
    }

    /// Waits for `path` to appear inside the sandbox of `lease`.
    pub async fn wait_for_file(
        &self,
        run_id: &str,
        lease: u64,
        path: &str,
        limit: Duration,
    ) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if self.read(run_id, lease, path).await.is_some() {
                return true;
            }
            time::sleep(Duration::from_millis(500)).await;
        }
        false
    }

    /// Shuts the observer's catalog down.
    pub async fn shutdown(self) {
        let _ = self.sandboxes.shutdown().await;
    }
}

/// Scope env as a plain map, for building scope specs in tests.
pub fn env(pairs: &[(&str, &str)]) -> BTreeMap<smol_str::SmolStr, ir::ExprOrValue> {
    pairs
        .iter()
        .map(|(k, v)| (smol_str::SmolStr::new(*k), ir::ExprOrValue::Value(json!(v))))
        .collect()
}

pub const RETAIN: Retention = Retention::Always;

/// A backend's own event as a step records it in a custom progress
/// payload: an object with a string `kind` naming the backend and an
/// `event` object, the backend's envelope (for the native agent backend:
/// Pebble's `CodingAgentEvent`, with `seq`, `stream_id`, `session_id`,
/// `parent_session_id`, `tool_call_id`, `timestamp`, `event`). Petri
/// forwards the payload as recorded and reads nothing into it; tests that
/// want the envelope's identities read them here.
#[derive(Clone, Debug, PartialEq)]
pub struct BackendEvent {
    pub backend:        String,
    pub session:        Option<String>,
    pub parent_session: Option<String>,
    pub tool_call:      Option<String>,
    pub stream:         Option<String>,
    pub stream_seq:     Option<u64>,
    pub envelope:       Value,
}

/// Read a backend's envelope out of a custom progress payload. A step's own
/// payload may carry a string `event` (a hook report names its hook event);
/// only an object is a backend envelope.
pub fn backend_event(value: &Value) -> Option<BackendEvent> {
    let object = value.as_object()?;
    let backend = object.get("kind")?.as_str()?;
    let envelope = object.get("event").filter(|event| event.is_object())?;
    let text = |key: &str| envelope.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(BackendEvent {
        backend:        backend.to_owned(),
        session:        text("session_id"),
        parent_session: text("parent_session_id"),
        tool_call:      text("tool_call_id"),
        stream:         text("stream_id"),
        stream_seq:     envelope.get("seq").and_then(Value::as_u64),
        envelope:       envelope.clone(),
    })
}
