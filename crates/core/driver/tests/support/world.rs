//! A simulated world for the driver, for deterministic simulation testing.
//!
//! A sandbox provider whose environments run simulated processes on the
//! runtime's clock, and a step that runs one process per attempt, as the
//! process step does. The world outlives any one driver. A crash ends the
//! driver's own tasks, but its processes keep running, as real ones do, and
//! the next acquisition of the same scope fences them. An acquisition the
//! driver drops while alive removes the sandbox it was creating, as the stock
//! executors' lease records make sure; one a crash cuts short leaves it for
//! the next fence. A dead driver's executor does nothing more: its releases
//! never happen. The world keeps a
//! ledger of every acquisition, release and process across driver lifetimes,
//! and notes each rule a run broke, for the simulation's oracles to read.
//!
//! Every roll comes from the world's seed, in the order the driver asks, so
//! a run is a function of its seed.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use driver::StepLogStore;
use executor::{
    AcquireContext, EnvError, EnvHandle, ExecEnv, Executor, ExitStatus, LogLine, ProcessHandle,
    ProcessSpec, ReleaseReport, ScopeOutcome, ScopeSpec, Sig,
};
use ir::{Control, LogStream, Outcome, SandboxInstance, ScopeId, StepKindId, Value};
use serde::Deserialize;
use smol_str::SmolStr;
use steps::{Registry, Step, StepCtx};
use tokio::sync::{Notify, mpsc};
use tokio::time;

/// `SplitMix64`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Dice(pub u64);

impl Dice {
    pub(crate) fn roll(&mut self, sides: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % sides.max(1)
    }

    pub(crate) fn chance(&mut self, percent: u64) -> bool {
        self.roll(100) < percent
    }
}

/// How often the world's own operations go wrong.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Faults {
    /// Percent of acquisitions that fail.
    pub acquire_failure: u64,
    /// The longest an acquisition takes, in milliseconds.
    pub acquire_ms:      u64,
    /// The longest a release takes, in milliseconds.
    pub release_ms:      u64,
}

/// One acquisition of a scope's environment, from the moment the provider
/// starts creating it.
#[derive(Clone, Debug)]
pub(crate) struct Acquired {
    pub scope:      ScopeId,
    pub generation: u32,
    pub lifetime:   u32,
    /// The acquisition failed, and left nothing behind.
    pub failed:     bool,
    /// The driver dropped the acquisition before it returned, and the
    /// sandbox it was creating is gone.
    pub abandoned:  bool,
    pub releases:   u32,
    /// A later acquisition of the same scope fenced it.
    pub fenced:     bool,
}

/// One process a step ran.
#[derive(Clone, Debug)]
pub(crate) struct Ran {
    pub scope:      ScopeId,
    pub generation: u32,
    pub lifetime:   u32,
    pub firing:     u64,
    pub attempt:    u32,
    state:          Arc<ProcessState>,
}

impl Ran {
    /// Whether the process still runs at `now`. One nobody waits on any
    /// more ends on time all the same.
    pub(crate) fn running(&self, now: time::Instant) -> bool {
        self.state.exit().is_none() && now < self.state.ends_at
    }
}

#[derive(Debug)]
struct ProcessState {
    exit:       Mutex<Option<ExitStatus>>,
    changed:    Notify,
    ends_at:    time::Instant,
    code:       i32,
    honor_term: bool,
}

impl ProcessState {
    fn exit(&self) -> Option<ExitStatus> {
        *self.exit.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn end(&self, status: ExitStatus) {
        let mut exit = self.exit.lock().unwrap_or_else(PoisonError::into_inner);
        if exit.is_none() {
            *exit = Some(status);
            self.changed.notify_waiters();
        }
    }

    fn kill(&self, signal: i32) {
        self.end(ExitStatus {
            code:      None,
            signal:    Some(signal),
            timed_out: false,
        });
    }
}

#[derive(Debug)]
struct WorldState {
    dice:         Dice,
    faults:       Faults,
    lifetime:     u32,
    generations:  BTreeMap<ScopeId, u32>,
    acquisitions: Vec<Acquired>,
    processes:    Vec<Ran>,
    /// `(firing, attempt)` whose finish was in the log the current lifetime
    /// resumed from.
    finished:     BTreeSet<(u64, u32)>,
    /// Firings a stop had reached, still stopping, in that log.
    stopping:     BTreeSet<u64>,
    /// Processes a fence ended.
    fenced:       usize,
    violations:   Vec<String>,
}

/// The world a simulated run's drivers share.
#[derive(Debug)]
pub(crate) struct World {
    state: Mutex<WorldState>,
}

impl World {
    pub(crate) fn new(seed: u64, faults: Faults) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WorldState {
                dice: Dice(seed ^ 0xA11C_E5ED),
                faults,
                lifetime: 0,
                generations: BTreeMap::new(),
                acquisitions: Vec::new(),
                processes: Vec::new(),
                finished: BTreeSet::new(),
                stopping: BTreeSet::new(),
                fenced: 0,
                violations: Vec::new(),
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, WorldState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A driver resumes after a crash, from a log in which `finished`
    /// attempts had finished and `stopping` firings were still stopping.
    pub(crate) fn begin_lifetime(&self, finished: BTreeSet<(u64, u32)>, stopping: BTreeSet<u64>) {
        let mut state = self.lock();
        state.lifetime += 1;
        state.finished = finished;
        state.stopping = stopping;
    }

    pub(crate) fn violation(&self, message: String) {
        self.lock().violations.push(message);
    }

    pub(crate) fn violations(&self) -> Vec<String> {
        self.lock().violations.clone()
    }

    pub(crate) fn acquisitions(&self) -> Vec<Acquired> {
        self.lock().acquisitions.clone()
    }

    pub(crate) fn processes(&self) -> Vec<Ran> {
        self.lock().processes.clone()
    }

    /// The executor a driver of the current lifetime acquires through.
    pub(crate) fn executor(self: &Arc<Self>) -> WorldExecutor {
        WorldExecutor {
            world:    Arc::clone(self),
            lifetime: self.lock().lifetime,
        }
    }

    pub(crate) fn fenced(&self) -> usize {
        self.lock().fenced
    }

    /// The current driver lifetime: 0 until the first crash.
    pub(crate) fn lifetime(&self) -> u32 {
        self.lock().lifetime
    }
}

/// What a release finds: the acquisition it ends.
#[derive(Debug)]
struct Lease {
    scope:      ScopeId,
    generation: u32,
}

/// The world's sandbox provider, as one driver lifetime reaches it.
pub(crate) struct WorldExecutor {
    world:    Arc<World>,
    lifetime: u32,
}

#[async_trait::async_trait]
impl Executor for WorldExecutor {
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        _ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        let (generation, delay, fail) = {
            let mut state = self.world.lock();
            let generation = {
                let next = state.generations.entry(scope.id).or_insert(0);
                *next += 1;
                *next
            };
            // The fence: nothing from an earlier acquisition of this scope
            // runs on once this one starts.
            let now = time::Instant::now();
            let mut fenced = 0;
            for process in &state.processes {
                if process.scope == scope.id
                    && process.generation < generation
                    && process.running(now)
                {
                    process.state.kill(9);
                    fenced += 1;
                }
            }
            state.fenced += fenced;
            for earlier in &mut state.acquisitions {
                if earlier.scope == scope.id && earlier.generation < generation {
                    earlier.fenced = true;
                }
            }
            let faults = state.faults;
            let delay = state.dice.roll(faults.acquire_ms + 1);
            let fail = state.dice.chance(faults.acquire_failure);
            let lifetime = self.lifetime;
            // The sandbox exists from here, whether or not the call returns.
            state.acquisitions.push(Acquired {
                scope: scope.id,
                generation,
                lifetime,
                failed: fail,
                abandoned: false,
                releases: 0,
                fenced: false,
            });
            (generation, delay, fail)
        };
        let mut creating = Creating {
            world: Arc::clone(&self.world),
            scope: scope.id,
            generation,
            lifetime: self.lifetime,
            returned: false,
        };
        time::sleep(Duration::from_millis(delay)).await;
        creating.returned = true;
        if fail {
            return Err(EnvError::backend(
                "world",
                "acquire",
                "simulated acquire failure",
            ));
        }
        let instance: SmolStr = format!("{}/{generation}", scope.environment.as_str()).into();
        Ok(EnvHandle::new(
            scope.id,
            instance.clone(),
            SandboxInstance {
                provider: "world".into(),
                instance,
                image: None,
                snapshot: None,
                working_directory: "/".into(),
            },
            Arc::new(WorldEnv {
                world: Arc::clone(&self.world),
                scope: scope.id,
                generation,
            }),
            Lease {
                scope: scope.id,
                generation,
            },
        ))
    }

    async fn release(&self, env: EnvHandle, _outcome: ScopeOutcome) -> ReleaseReport {
        // A dead driver releases nothing: its safety net never runs, and a
        // release it had started dies with it.
        if self.dead() {
            return ReleaseReport::default();
        }
        let delay = {
            let mut state = self.world.lock();
            let most = state.faults.release_ms;
            state.dice.roll(most + 1)
        };
        time::sleep(Duration::from_millis(delay)).await;
        if self.dead() {
            return ReleaseReport::default();
        }
        let Some(lease) = env.teardown::<Lease>() else {
            self.world.violation(format!(
                "released {}, which the world never acquired",
                env.instance()
            ));
            return ReleaseReport::default();
        };
        let mut state = self.world.lock();
        // A release ends whatever still runs in the environment.
        for process in &state.processes {
            if process.scope == lease.scope && process.generation == lease.generation {
                process.state.kill(9);
            }
        }
        let released = state
            .acquisitions
            .iter_mut()
            .find(|acquired| {
                acquired.scope == lease.scope && acquired.generation == lease.generation
            })
            .map(|acquired| {
                acquired.releases += 1;
                acquired.releases
            });
        if released != Some(1) {
            let (scope, generation) = (lease.scope, lease.generation);
            state.violations.push(format!(
                "{scope} generation {generation} released {released:?} times"
            ));
        }
        ReleaseReport::default()
    }
}

impl WorldExecutor {
    fn dead(&self) -> bool {
        self.world.lock().lifetime > self.lifetime
    }
}

/// An acquisition in progress. Dropped before it returns by a live driver,
/// it removes the sandbox it was creating; a dead driver runs no cleanup.
struct Creating {
    world:      Arc<World>,
    scope:      ScopeId,
    generation: u32,
    lifetime:   u32,
    returned:   bool,
}

impl Drop for Creating {
    fn drop(&mut self) {
        if self.returned {
            return;
        }
        let mut state = self.world.lock();
        if state.lifetime > self.lifetime {
            return;
        }
        if let Some(acquired) = state
            .acquisitions
            .iter_mut()
            .find(|a| a.scope == self.scope && a.generation == self.generation)
        {
            acquired.abandoned = true;
        }
    }
}

/// One acquired environment.
#[derive(Debug)]
struct WorldEnv {
    world:      Arc<World>,
    scope:      ScopeId,
    generation: u32,
}

/// A process's scripted parameter, from its environment.
fn var<T: FromStr>(spec: &ProcessSpec, key: &str) -> Option<T> {
    spec.env.get(key).and_then(|value| value.parse().ok())
}

#[async_trait::async_trait]
impl ExecEnv for WorldEnv {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let firing: u64 = var(&spec, "SIM_FIRING").unwrap_or_default();
        let attempt: u32 = var(&spec, "SIM_ATTEMPT").unwrap_or_default();
        let work: u64 = var(&spec, "SIM_WORK_MS").unwrap_or_default();
        let lines: u32 = var(&spec, "SIM_LINES").unwrap_or_default();
        let mut state = self.world.lock();
        let lifetime = state.lifetime;
        let now = time::Instant::now();
        let current = state.generations.get(&self.scope) == Some(&self.generation);
        if current
            && state.processes.iter().any(|earlier| {
                earlier.scope == self.scope
                    && earlier.generation < self.generation
                    && earlier.running(now)
            })
        {
            state.violations.push(format!(
                "firing {firing} attempt {attempt} ran in {} generation {} beside an unfenced \
                 earlier process",
                self.scope, self.generation
            ));
        }
        if lifetime > 0 && state.stopping.contains(&firing) {
            state.violations.push(format!(
                "firing {firing} was stopping at the crash and ran again (attempt {attempt})"
            ));
        }
        if lifetime > 0 && state.finished.contains(&(firing, attempt)) {
            state.violations.push(format!(
                "firing {firing} attempt {attempt} finished before the crash and ran again"
            ));
        }
        if state
            .processes
            .iter()
            .any(|ran| ran.lifetime == lifetime && ran.firing == firing && ran.attempt == attempt)
        {
            state.violations.push(format!(
                "firing {firing} attempt {attempt} ran twice in driver lifetime {lifetime}"
            ));
        }
        let process = Arc::new(ProcessState {
            exit:       Mutex::new(None),
            changed:    Notify::new(),
            ends_at:    time::Instant::now() + Duration::from_millis(work),
            code:       var(&spec, "SIM_EXIT").unwrap_or_default(),
            honor_term: var(&spec, "SIM_HONOR_TERM").unwrap_or(true),
        });
        // A fenced environment runs nothing: its process is dead on arrival.
        if !current {
            process.kill(9);
        }
        state.processes.push(Ran {
            scope: self.scope,
            generation: self.generation,
            lifetime,
            firing,
            attempt,
            state: Arc::clone(&process),
        });
        drop(state);
        let (tx, rx) = mpsc::channel(usize::try_from(lines).unwrap_or(0) + 1);
        for index in 0..lines {
            let _ = tx.try_send(LogLine {
                stream:     LogStream::Stdout,
                line:       format!("firing {firing} attempt {attempt} line {index}"),
                dropped:    0,
                terminated: true,
            });
        }
        Ok(Box::new(WorldProcess {
            state: process,
            lines: Some(rx),
        }))
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
        Duration::from_millis(100)
    }
}

/// A process that runs for its work time on the runtime's clock, unless a
/// signal or a fence ends it first.
struct WorldProcess {
    state: Arc<ProcessState>,
    lines: Option<mpsc::Receiver<LogLine>>,
}

#[async_trait::async_trait]
impl ProcessHandle for WorldProcess {
    fn lines(&mut self) -> Option<mpsc::Receiver<LogLine>> {
        self.lines.take()
    }

    async fn wait(&mut self) -> Result<ExitStatus, EnvError> {
        loop {
            let changed = self.state.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(exit) = self.state.exit() {
                return Ok(exit);
            }
            // In a fixed order, as everything in a simulation: a process that
            // reaches its end as a signal lands has ended.
            tokio::select! {
                biased;
                () = time::sleep_until(self.state.ends_at) => {
                    self.state.end(ExitStatus::code(self.state.code));
                }
                () = &mut changed => {}
            }
        }
    }

    async fn signal(&mut self, sig: Sig) -> Result<(), EnvError> {
        match sig {
            Sig::Term if self.state.honor_term => self.state.kill(15),
            Sig::Term => {}
            Sig::Kill => self.state.kill(9),
        }
        Ok(())
    }
}

// ── A step that runs one process per attempt ──────────────────────────────

pub(crate) const SANDBOXED: StepKindId = StepKindId::new_static("sandboxed");

#[derive(Deserialize)]
pub(crate) struct SandboxedConfig {
    /// Virtual milliseconds of work per attempt; the last repeats.
    #[serde(default)]
    work_ms:    Vec<u64>,
    /// Exit code per attempt; the last repeats. Zero is success.
    #[serde(default)]
    exits:      Vec<i32>,
    /// The process ends on `SIGTERM`. Otherwise only `SIGKILL` ends it.
    #[serde(default = "yes")]
    honor_term: bool,
    /// The step ignores every stop signal, so only the driver's hard
    /// deadline ends it.
    #[serde(default)]
    wedged:     bool,
    /// Lines the process prints.
    #[serde(default)]
    lines:      u32,
}

fn yes() -> bool {
    true
}

/// Spawns one process in the scope's environment, forwards its output, turns
/// a cancel into `SIGTERM` and a kill into `SIGKILL`, and reports its exit.
pub(crate) struct SandboxedStep;

#[async_trait::async_trait]
impl Step for SandboxedStep {
    type Config = SandboxedConfig;

    const NAME: &'static str = "sandboxed";

    async fn run(&self, config: SandboxedConfig, mut ctx: StepCtx) -> Outcome {
        let index = usize::try_from(ctx.attempt.raw()).map_or(0, |n| n.saturating_sub(1));
        let work = config
            .work_ms
            .get(index)
            .or(config.work_ms.last())
            .copied()
            .unwrap_or(0);
        let exit = config
            .exits
            .get(index)
            .or(config.exits.last())
            .copied()
            .unwrap_or(0);
        let mut spec = ProcessSpec::new("sim", &[]);
        for (key, value) in [
            ("SIM_FIRING", ctx.firing.raw().to_string()),
            ("SIM_ATTEMPT", ctx.attempt.raw().to_string()),
            ("SIM_WORK_MS", work.to_string()),
            ("SIM_EXIT", exit.to_string()),
            ("SIM_HONOR_TERM", config.honor_term.to_string()),
            ("SIM_LINES", config.lines.to_string()),
        ] {
            spec.env.insert(key.into(), value.into());
        }
        let mut process = match ctx.env.spawn(spec).await {
            Ok(process) => process,
            Err(error) => return Outcome::failure(format!("spawn failed: {error}")),
        };
        if let Some(mut lines) = process.lines() {
            while let Some(line) = lines.recv().await {
                ctx.log(line.stream, line.line).await;
            }
        }
        let mut controls = true;
        loop {
            // The exit first: a signal that lands as the process ends is too
            // late.
            tokio::select! {
                biased;
                exit = process.wait() => return match exit {
                    Ok(ExitStatus { code: Some(0), .. }) => Outcome::success(Value::Null),
                    Ok(ExitStatus { code: Some(code), .. }) => Outcome::failure(format!("exit {code}")),
                    Ok(_) => Outcome::cancelled(),
                    Err(error) => Outcome::failure(format!("wait failed: {error}")),
                },
                ctl = ctx.control.recv(), if controls => match ctl {
                    _ if config.wedged => {}
                    Some(Control::Cancel) => {
                        let _ = process.signal(Sig::Term).await;
                    }
                    Some(Control::Kill) => {
                        let _ = process.signal(Sig::Kill).await;
                    }
                    // A delivered value, or anything newer, is no stop.
                    Some(_) => {}
                    None => controls = false,
                },
            }
        }
    }
}

pub(crate) fn sandboxed_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(SandboxedStep);
    registry
}

/// Step output kept in memory: the simulation writes no file.
#[derive(Debug, Default)]
pub(crate) struct MemoryLogs(Mutex<BTreeMap<String, Vec<String>>>);

impl MemoryLogs {
    /// How many lines the logs hold.
    pub(crate) fn lines(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(Vec::len)
            .sum()
    }
}

#[async_trait::async_trait]
impl StepLogStore for MemoryLogs {
    async fn append(&self, name: &str, _stream: LogStream, line: &str) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(name.to_owned())
            .or_default()
            .push(line.to_owned());
        Ok(())
    }
}
