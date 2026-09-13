//! Shared test scaffolding.
//!
//! What every end-to-end harness needs and none should copy: a run directory
//! that cleans itself up, readers over a [`ExecutionReport`], the replay canary
//! as an assertion, a step that ignores cancellation, and a step that needs a
//! capability. Dev-dependency only; never published.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{env, fs, process};

use driver::ExecutionReport;
use executor::Retention;
use executor_sandbox::{PluginSettings, PluginSource};
use ir::{Graph, GraphBuilder, NodeId, ScopeId, StepRef, Value};
use serde::Deserialize;
use serde_json::json;
use steps::PROCESS_KIND;
use tokio::process::Command;
use tokio::time;

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
            engine::Event::StepProgress {
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

/// Whether the Docker plugin can be launched from this process's environment
/// and reports a healthy daemon: the same path every container scope takes.
/// The plugin comes from `PETRI_SANDBOX_DOCKER_PLUGIN`, which `mise run
/// plugins:build` installs under `target/plugins` and the test tasks point
/// at.
pub async fn is_docker_available() -> bool {
    let Ok(settings) = PluginSettings::from_env("docker", None) else {
        return false;
    };
    let supervisor = PluginSource::new(settings);
    let ready = supervisor.current().await.is_ok();
    supervisor.shutdown().await;
    ready
}

/// The run id an executor recorded under `run_dir`, once one has.
pub fn recorded_run_id(run_dir: &Path) -> String {
    fs::read_to_string(run_dir.join(executor_sandbox::RUN_ID_FILE))
        .expect("the run id is recorded under the run dir")
        .trim()
        .to_owned()
}

/// The name of the container sandbox for `lease` of the run under
/// `run_dir`: `petri-<run id>-l<lease>`. A bare driver keys each scope's
/// sandbox by the scope id, so scope 0 is lease 0; a coordinator mints
/// leases in scope order from 0.
pub fn sandbox_name(run_dir: &Path, lease: u64) -> String {
    format!("petri-{}-l{lease}", recorded_run_id(run_dir))
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
        "PETRI_REQUIRE_DOCKER is set, but the Docker plugin is missing or reports no daemon"
    );
    eprintln!("skipping: no Docker plugin with a reachable daemon");
    false
}

/// Scope env as a plain map, for building scope specs in tests.
pub fn env(pairs: &[(&str, &str)]) -> BTreeMap<smol_str::SmolStr, ir::ExprOrValue> {
    pairs
        .iter()
        .map(|(k, v)| (smol_str::SmolStr::new(*k), ir::ExprOrValue::Value(json!(v))))
        .collect()
}

pub const RETAIN: Retention = Retention::Always;
