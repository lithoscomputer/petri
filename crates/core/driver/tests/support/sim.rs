//! A simulated host for the driver: an executor with no sandbox, a step that
//! only waits on the runtime's clock, and a runner that drives a whole run on
//! a single-threaded runtime whose clock starts paused. Nothing touches a
//! process, a file or the wall clock, so a run is a function of its inputs.

use std::future::Future;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use driver::{
    Driver, EventObserver, ExecutionHooks, HookContext, RecordingClock, RunConfig, RunHandle,
    SeededDecisionResolver,
};
use engine::{EngineState, EventRecord};
use executor::{
    AcquireContext, EnvError, EnvHandle, ExecEnv, Executor, MapSecrets, ProcessHandle, ProcessSpec,
    ReleaseReport, ScopeOutcome, ScopeSpec,
};
use ir::{
    Control, ExecutionId, Graph, InvocationId, Outcome, RunStatus, SandboxInstance, Status,
    StepKindId, Value,
};
use serde::Deserialize;
use steps::{Registry, Step, StepCtx};
use store::RunKey;
use tokio::runtime::Builder;
use tokio::time;

// ── A sandbox that runs nothing ───────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct NoEnv;

#[async_trait::async_trait]
impl ExecEnv for NoEnv {
    async fn spawn(&self, _spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        Err(EnvError::backend(
            "fake",
            "spawn",
            "this environment runs nothing",
        ))
    }

    fn workspace_path(&self) -> &'static str {
        "/work"
    }

    async fn read_file(&self, _relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
        Ok(None)
    }

    async fn write_file(&self, _relative: &Path, _: &[u8]) -> Result<(), EnvError> {
        Ok(())
    }

    fn grace(&self) -> Duration {
        Duration::from_secs(1)
    }
}

/// An executor whose environments are [`NoEnv`], acquired after `acquire`
/// of virtual time.
#[derive(Default)]
pub(crate) struct NoExecutor {
    pub acquire: Duration,
}

#[async_trait::async_trait]
impl Executor for NoExecutor {
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        _ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        if !self.acquire.is_zero() {
            time::sleep(self.acquire).await;
        }
        Ok(EnvHandle::new(
            scope.id,
            scope.environment.as_str().into(),
            SandboxInstance {
                provider:          "test".into(),
                instance:          scope.environment.as_str().into(),
                image:             None,
                snapshot:          None,
                working_directory: "/".into(),
            },
            Arc::new(NoEnv),
            (),
        ))
    }

    async fn release(&self, _env: EnvHandle, _outcome: ScopeOutcome) -> ReleaseReport {
        ReleaseReport::default()
    }
}

// ── A step that only waits ────────────────────────────────────────────────

pub(crate) const SCRIPTED: StepKindId = StepKindId::new_static("scripted");

#[derive(Deserialize)]
pub(crate) struct ScriptedConfig {
    /// Virtual milliseconds of work per attempt, first attempt first; the
    /// last repeats. None is no work.
    #[serde(default)]
    work_ms:    Vec<u64>,
    /// `success`, `failure` or `timed_out` per attempt; the last repeats.
    /// None is success.
    #[serde(default)]
    outcomes:   Vec<String>,
    /// Report `cancelled` on a cancel signal. Otherwise the step finishes its
    /// work and reports its outcome, as one that finished before the signal
    /// reached it would. A kill always ends it.
    #[serde(default = "honor_by_default")]
    honor_stop: bool,
}

fn honor_by_default() -> bool {
    true
}

/// Waits its scripted work on the runtime's clock, then reports its scripted
/// outcome.
pub(crate) struct ScriptedStep;

#[async_trait::async_trait]
impl Step for ScriptedStep {
    type Config = ScriptedConfig;

    const NAME: &'static str = "scripted";

    async fn run(&self, config: ScriptedConfig, mut ctx: StepCtx) -> Outcome {
        let attempt = usize::try_from(ctx.attempt.raw()).map_or(0, |n| n.saturating_sub(1));
        let pick = |values: &[u64]| values.get(attempt).or(values.last()).copied();
        let work = time::sleep(Duration::from_millis(pick(&config.work_ms).unwrap_or(0)));
        tokio::pin!(work);
        loop {
            // In a fixed order, as everything in a simulation.
            tokio::select! {
                biased;
                () = &mut work => break,
                ctl = ctx.control.recv() => match ctl {
                    Some(Control::Deliver(_)) => {}
                    Some(Control::Cancel) if !config.honor_stop => {}
                    _ => return Outcome::cancelled(),
                },
            }
        }
        let outcome = config
            .outcomes
            .get(attempt)
            .or(config.outcomes.last())
            .map_or("success", String::as_str);
        match outcome {
            "failure" => Outcome::failure("scripted failure"),
            "timed_out" => Outcome::new(Status::TimedOut, Value::Null),
            _ => Outcome::success(Value::Null),
        }
    }
}

pub(crate) fn scripted_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(ScriptedStep);
    registry
}

// ── The simulated run ─────────────────────────────────────────────────────

/// The recording clock's reading at the simulation's start, in milliseconds
/// since the Unix epoch.
pub(crate) const SIMULATED_EPOCH_MS: u64 = 1_800_000_000_000;

/// Everything a run shows outside the driver: its status, every record its
/// observers saw with the stamp it carried, and the final log.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Trace {
    pub status:  RunStatus,
    /// `(seq, recorded_at, record)`, in the order observers saw them.
    pub records: Vec<(u64, u64, String)>,
    pub log:     String,
}

struct Recorder(Arc<Mutex<Vec<(u64, u64, String)>>>);

impl EventObserver for Recorder {
    fn on_record(&self, record: &EventRecord, recorded_at: u64, _: &EngineState) {
        let encoded = serde_json::to_string(record).expect("a record always encodes");
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push((
            record.seq,
            recorded_at,
            encoded,
        ));
    }
}

/// One simulated run's inputs besides the graph.
pub(crate) struct Simulation {
    /// Where the run directory would be. A scripted step writes nothing.
    pub run_dir: PathBuf,
    /// Seeds the weighted routing draws.
    pub seed:    u64,
    pub acquire: Duration,
    pub config:  Box<dyn FnOnce(RunConfig) -> RunConfig>,
    pub hooks:   Option<Arc<dyn ExecutionHooks>>,
}

impl Simulation {
    pub(crate) fn new(run_dir: impl Into<PathBuf>, seed: u64) -> Self {
        Self {
            run_dir: run_dir.into(),
            seed,
            acquire: Duration::from_millis(10),
            config: Box::new(|config| config),
            hooks: None,
        }
    }
}

/// Run `graph` to its end on a fresh single-threaded runtime whose clock
/// starts paused and moves only when every task waits. `host` runs beside the
/// driver with its handle, to stop the run at virtual times. The recording
/// clock reads the runtime's clock from [`SIMULATED_EPOCH_MS`].
pub(crate) fn simulate<H, F>(graph: Graph, sim: Simulation, host: H) -> Trace
where
    H: FnOnce(RunHandle) -> F,
    F: Future<Output = ()>,
{
    let runtime = Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a current-thread runtime builds");
    let records = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::new(Recorder(Arc::clone(&records)));
    let report = runtime.block_on(async move {
        let epoch = time::Instant::now();
        let clock = RecordingClock::new(move || {
            SIMULATED_EPOCH_MS + u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        let config = (sim.config)(RunConfig::new(sim.run_dir).with_recording_clock(clock));
        let mut driver = Driver::new(
            graph,
            Arc::new(NoExecutor {
                acquire: sim.acquire,
            }),
            scripted_registry(),
            Arc::new(MapSecrets::empty()),
            config,
        )
        .with_decision_resolver(Arc::new(SeededDecisionResolver::new(sim.seed)))
        .observe(recorder);
        if let Some(hooks) = sim.hooks {
            let context =
                HookContext::new(RunKey::new("sim"), InvocationId::ROOT, ExecutionId::new(0));
            driver = driver.with_hooks(hooks, context);
        }
        let handle = driver.handle();
        let (report, ()) = tokio::join!(driver.run(), host(handle));
        report
    });
    let log = serde_json::to_string(&report.state.log).expect("a log always encodes");
    let records = mem::take(&mut *records.lock().unwrap_or_else(PoisonError::into_inner));
    Trace {
        status: report.status,
        records,
        log,
    }
}
