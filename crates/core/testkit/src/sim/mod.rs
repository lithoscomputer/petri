//! A simulated world for deterministic simulation testing of the driver and
//! the layers above it.
//!
//! A sandbox provider whose environments run simulated processes on the
//! runtime's clock, a step that runs one process per attempt, as the process
//! step does, sometimes after it asks the host a question, and a host's hooks
//! that delay, block, skip or fail attempts. The world outlives any one
//! driver. A crash ends the driver's own tasks, but its processes keep
//! running, as real ones do, and the next acquisition of the same scope
//! fences them. An acquisition the driver drops while alive removes the
//! sandbox it was creating, as the stock executors' lease records make sure;
//! one a crash cuts short leaves it for the next fence. A dead driver's
//! executor does nothing more: its releases never happen. The world keeps a
//! ledger of every acquisition, release and process across driver lifetimes,
//! and notes each rule a run broke, for the simulation's oracles to read.
//!
//! Every roll comes from the world's seed, in the order the driver asks, so
//! a run is a function of its seed.

use std::collections::{BTreeMap, BTreeSet};
use std::future::pending;
use std::io;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use driver::StepLogStore;
use driver::lifecycle::{
    AdmitAttempt, AttemptDecision, ExecutionHooks, HookContext, PrepareError, PrepareResult,
    Prepared,
};
use engine::Admission;
use executor::{
    AcquireContext, EnvError, EnvHandle, ExecEnv, Executor, ExitStatus, LogLine, ProcessHandle,
    ProcessSpec, ReleaseReport, ScopeOutcome, ScopeSpec, Sig,
};
use ir::{Control, LogStream, Outcome, SandboxInstance, ScopeId, StepKindId, Value};
use serde::Deserialize;
use smol_str::SmolStr;
use steps::{Answer, Question, QuestionExpired, Registry, Step, StepCtx};
use tokio::sync::{Notify, mpsc};
use tokio::time;

mod provider;

pub use provider::{
    Call, CallCrash, CallRecord, LeaseRecords, LeaseView, Moment, WORLD_KIND, WorldFactory,
    WorldSandbox,
};
pub use sandbox_driver::SandboxState;

/// `SplitMix64`.
#[derive(Clone, Copy, Debug)]
pub struct Dice(pub u64);

impl Dice {
    pub fn roll(&mut self, sides: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % sides.max(1)
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.roll(100) < percent
    }
}

/// How often the world's own operations go wrong.
#[derive(Clone, Copy, Debug)]
pub struct Faults {
    /// Percent of acquisitions that fail.
    pub acquire_failure: u64,
    /// The longest an acquisition takes, in milliseconds.
    pub acquire_ms:      u64,
    /// The longest a release takes, in milliseconds.
    pub release_ms:      u64,
    /// Percent of sandbox stops and deletes that fail, through the provider.
    pub release_failure: u64,
}

/// One acquisition of a scope's environment, from the moment the provider
/// starts creating it.
#[derive(Clone, Debug)]
pub struct Acquired {
    pub scope:      ScopeId,
    /// The sandbox: its lease under a coordinator, else its scope.
    pub key:        SmolStr,
    /// The execution that acquired it, as its environment names it; empty
    /// for a driver on its own.
    pub execution:  SmolStr,
    /// Counts this sandbox's acquisitions.
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
pub struct Ran {
    pub key:        SmolStr,
    pub execution:  SmolStr,
    pub generation: u32,
    pub lifetime:   u32,
    pub firing:     u64,
    pub attempt:    u32,
    state:          Arc<ProcessState>,
}

impl Ran {
    /// Whether the process still runs at `now`. One nobody waits on any
    /// more ends on time all the same.
    pub fn running(&self, now: time::Instant) -> bool {
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
    dice:          Dice,
    faults:        Faults,
    lifetime:      u32,
    generations:   BTreeMap<SmolStr, u32>,
    acquisitions:  Vec<Acquired>,
    processes:     Vec<Ran>,
    /// `(execution, firing, attempt)` whose finish was in the logs the
    /// current lifetime resumed from.
    finished:      BTreeSet<(SmolStr, u64, u32)>,
    /// `(execution, firing)` a stop had reached, still stopping, in those
    /// logs.
    stopping:      BTreeSet<(SmolStr, u64)>,
    /// A sibling execution holds the attempt slot the driver shares with it.
    sibling:       bool,
    /// How often the sibling took the slot.
    sibling_turns: usize,
    /// `(firing, attempt)` whose result preparation the host failed, fatally.
    fatal:         BTreeSet<(u64, u32)>,
    /// Processes a fence ended.
    fenced:        usize,
    violations:    Vec<String>,
    /// The provider's sandboxes, every one ever created, in creation order.
    sandboxes:     Vec<WorldSandbox>,
    /// Every provider call that changed a sandbox.
    calls:         Vec<CallRecord>,
    /// Calls of each kind this lifetime, for the crash plan.
    counts:        BTreeMap<Call, usize>,
    /// Crash the lifetime at the `nth` call of a kind.
    crash_at:      Option<(CallCrash, Arc<Notify>)>,
    /// Where provider calls read the run's lease records.
    records:       Option<Arc<dyn LeaseRecords>>,
}

/// The world a simulated run's drivers share.
#[derive(Debug)]
pub struct World {
    state: Mutex<WorldState>,
}

impl World {
    pub fn new(seed: u64, faults: Faults) -> Arc<Self> {
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
                sibling: false,
                sibling_turns: 0,
                fatal: BTreeSet::new(),
                fenced: 0,
                violations: Vec::new(),
                sandboxes: Vec::new(),
                calls: Vec::new(),
                counts: BTreeMap::new(),
                crash_at: None,
                records: None,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, WorldState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The drivers resume after a crash, from logs in which `finished`
    /// attempts had finished and `stopping` firings were still stopping. A
    /// firing is named by its execution, as its environment names it (empty
    /// for a driver on its own), and its id.
    pub fn begin_lifetime(
        &self,
        finished: BTreeSet<(SmolStr, u64, u32)>,
        stopping: BTreeSet<(SmolStr, u64)>,
    ) {
        let mut state = self.lock();
        state.lifetime += 1;
        state.finished = finished;
        state.stopping = stopping;
        state.counts.clear();
        state.crash_at = None;
    }

    pub fn violation(&self, message: String) {
        self.lock().violations.push(message);
    }

    pub fn violations(&self) -> Vec<String> {
        self.lock().violations.clone()
    }

    pub fn acquisitions(&self) -> Vec<Acquired> {
        self.lock().acquisitions.clone()
    }

    pub fn processes(&self) -> Vec<Ran> {
        self.lock().processes.clone()
    }

    /// The executor a driver of the current lifetime acquires through.
    pub fn executor(self: &Arc<Self>) -> WorldExecutor {
        WorldExecutor {
            world:    Arc::clone(self),
            lifetime: self.lock().lifetime,
        }
    }

    pub fn fenced(&self) -> usize {
        self.lock().fenced
    }

    /// A sibling execution took, or gave back, the shared attempt slot.
    pub fn sibling_holds_slot(&self, holds: bool) {
        let mut state = self.lock();
        state.sibling = holds;
        state.sibling_turns += usize::from(holds);
    }

    pub fn sibling_turns(&self) -> usize {
        self.lock().sibling_turns
    }

    /// The attempts whose result preparation the host failed, fatally.
    pub fn fatal(&self) -> BTreeSet<(u64, u32)> {
        self.lock().fatal.clone()
    }

    /// The current driver lifetime: 0 until the first crash.
    pub fn lifetime(&self) -> u32 {
        self.lock().lifetime
    }
}

/// What a release finds: the acquisition it ends.
#[derive(Debug)]
struct Lease {
    key:        SmolStr,
    generation: u32,
}

/// The execution an environment belongs to: the prefix a coordinator puts
/// before `-scope-<n>`, empty for a driver on its own.
fn execution_of(environment: &str) -> SmolStr {
    environment
        .rsplit_once("-scope-")
        .map_or_else(SmolStr::default, |(prefix, _)| SmolStr::new(prefix))
}

/// The world's sandbox provider, as one driver lifetime reaches it.
pub struct WorldExecutor {
    world:    Arc<World>,
    lifetime: u32,
}

#[async_trait::async_trait]
impl Executor for WorldExecutor {
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        let key: SmolStr = ctx.lease().map_or_else(
            || format!("scope-{}", scope.id.raw()).into(),
            |lease| format!("lease-{}", lease.raw()).into(),
        );
        let execution = execution_of(scope.environment.as_str());
        let lifetime = self.lifetime;
        let (generation, delay, fail) = {
            let mut state = self.world.lock();
            let generation = {
                let next = state.generations.entry(key.clone()).or_insert(0);
                *next += 1;
                *next
            };
            // The fence: nothing a dead driver left in this sandbox runs on
            // once a live one acquires it. A second acquisition in the same
            // lifetime (a restarted execution, a child that inherits the
            // sandbox) shares it.
            let now = time::Instant::now();
            let mut fenced = 0;
            for process in &state.processes {
                if process.key == key && process.lifetime < lifetime && process.running(now) {
                    process.state.kill(9);
                    fenced += 1;
                }
            }
            state.fenced += fenced;
            for earlier in &mut state.acquisitions {
                if earlier.key == key && earlier.lifetime < lifetime {
                    earlier.fenced = true;
                }
            }
            let faults = state.faults;
            let delay = state.dice.roll(faults.acquire_ms + 1);
            let fail = state.dice.chance(faults.acquire_failure);
            // The sandbox exists from here, whether or not the call returns.
            state.acquisitions.push(Acquired {
                scope: scope.id,
                key: key.clone(),
                execution: execution.clone(),
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
            key: key.clone(),
            generation,
            lifetime,
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
                key: key.clone(),
                execution,
                generation,
            }),
            Lease { key, generation },
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
            if process.key == lease.key && process.generation == lease.generation {
                process.state.kill(9);
            }
        }
        let released = state
            .acquisitions
            .iter_mut()
            .find(|acquired| acquired.key == lease.key && acquired.generation == lease.generation)
            .map(|acquired| {
                acquired.releases += 1;
                acquired.releases
            });
        if released != Some(1) {
            let (key, generation) = (&lease.key, lease.generation);
            state.violations.push(format!(
                "{key} generation {generation} released {released:?} times"
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
    key:        SmolStr,
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
            .find(|a| a.key == self.key && a.generation == self.generation)
        {
            acquired.abandoned = true;
        }
    }
}

/// One acquired environment.
#[derive(Debug)]
struct WorldEnv {
    world:      Arc<World>,
    key:        SmolStr,
    execution:  SmolStr,
    generation: u32,
}

/// The `index`th line a process prints.
fn line(firing: u64, attempt: u32, index: u32) -> String {
    format!("firing {firing} attempt {attempt} line {index}")
}

/// Where a process starts: its sandbox, as the world keys it, and the
/// execution that ran it.
struct Place {
    key:        SmolStr,
    generation: u32,
    execution:  SmolStr,
    /// The sandbox is the one its holder should run in: not fenced by a
    /// later acquisition. A process in a fenced sandbox is dead on arrival.
    current:    bool,
}

/// A started process: its state, whose attempt it is, and the lines it
/// prints.
struct Started {
    state:   Arc<ProcessState>,
    firing:  u64,
    attempt: u32,
    lines:   u32,
}

impl World {
    /// Start a process at `place` with the parameters `var` reads (the
    /// `SIM_*` values the sandboxed step sets), noting each rule its start
    /// breaks.
    fn start_process(&self, place: Place, var: impl Fn(&str) -> Option<String>) -> Started {
        fn parse<T: FromStr>(value: Option<String>) -> Option<T> {
            value.and_then(|value| value.parse().ok())
        }
        let firing: u64 = parse(var("SIM_FIRING")).unwrap_or_default();
        let attempt: u32 = parse(var("SIM_ATTEMPT")).unwrap_or_default();
        let work: u64 = parse(var("SIM_WORK_MS")).unwrap_or_default();
        let lines: u32 = parse(var("SIM_LINES")).unwrap_or_default();
        let Place {
            key,
            generation,
            execution,
            current,
        } = place;
        let mut state = self.lock();
        let lifetime = state.lifetime;
        let now = time::Instant::now();
        if current
            && state.processes.iter().any(|earlier| {
                earlier.key == key && earlier.lifetime < lifetime && earlier.running(now)
            })
        {
            state.violations.push(format!(
                "firing {firing} attempt {attempt} ran in {key} generation {generation} beside \
                 an unfenced process a dead driver left"
            ));
        }
        if state.sibling {
            state.violations.push(format!(
                "firing {firing} attempt {attempt} ran while a sibling execution held the slot"
            ));
        }
        let at = if execution.is_empty() {
            String::new()
        } else {
            format!(" of {execution}")
        };
        if lifetime > 0 && state.stopping.contains(&(execution.clone(), firing)) {
            state.violations.push(format!(
                "firing {firing}{at} was stopping at the crash and ran again (attempt {attempt})"
            ));
        }
        if lifetime > 0
            && state
                .finished
                .contains(&(execution.clone(), firing, attempt))
        {
            state.violations.push(format!(
                "firing {firing}{at} attempt {attempt} finished before the crash and ran again"
            ));
        }
        if state.processes.iter().any(|ran| {
            ran.lifetime == lifetime
                && ran.execution == execution
                && ran.firing == firing
                && ran.attempt == attempt
        }) {
            state.violations.push(format!(
                "firing {firing}{at} attempt {attempt} ran twice in driver lifetime {lifetime}"
            ));
        }
        let process = Arc::new(ProcessState {
            exit:       Mutex::new(None),
            changed:    Notify::new(),
            ends_at:    time::Instant::now() + Duration::from_millis(work),
            code:       parse(var("SIM_EXIT")).unwrap_or_default(),
            honor_term: parse(var("SIM_HONOR_TERM")).unwrap_or(true),
        });
        // A fenced environment runs nothing: its process is dead on arrival.
        if !current {
            process.kill(9);
        }
        state.processes.push(Ran {
            key,
            execution,
            generation,
            lifetime,
            firing,
            attempt,
            state: Arc::clone(&process),
        });
        Started {
            state: process,
            firing,
            attempt,
            lines,
        }
    }
}

#[async_trait::async_trait]
impl ExecEnv for WorldEnv {
    async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let current = !self
            .world
            .lock()
            .acquisitions
            .iter()
            .any(|a| a.key == self.key && a.generation == self.generation && a.fenced);
        let started = self.world.start_process(
            Place {
                key: self.key.clone(),
                generation: self.generation,
                execution: self.execution.clone(),
                current,
            },
            |key| spec.env.get(key).map(ToString::to_string),
        );
        let (tx, rx) = mpsc::channel(usize::try_from(started.lines).unwrap_or(0) + 1);
        for index in 0..started.lines {
            let _ = tx.try_send(LogLine {
                stream:     LogStream::Stdout,
                line:       line(started.firing, started.attempt, index),
                dropped:    0,
                terminated: true,
            });
        }
        Ok(Box::new(WorldProcess {
            state: started.state,
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

pub const SANDBOXED: StepKindId = StepKindId::new_static("sandboxed");

#[derive(Deserialize)]
pub struct SandboxedConfig {
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
    /// Ask the host a question before the work, and wait for its answer.
    #[serde(default)]
    asks:       bool,
    /// How long the step waits for its answer, in milliseconds; past it, the
    /// step reports the question expired and goes on. It waits for ever
    /// when unset.
    #[serde(default)]
    ask_ms:     Option<u64>,
}

fn yes() -> bool {
    true
}

/// Spawns one process in the scope's environment, forwards its output, turns
/// a cancel into `SIGTERM` and a kill into `SIGKILL`, and reports its exit.
pub struct SandboxedStep;

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
            ("SIM_EXECUTION", ctx.environment.to_string()),
            ("SIM_FIRING", ctx.firing.raw().to_string()),
            ("SIM_ATTEMPT", ctx.attempt.raw().to_string()),
            ("SIM_WORK_MS", work.to_string()),
            ("SIM_EXIT", exit.to_string()),
            ("SIM_HONOR_TERM", config.honor_term.to_string()),
            ("SIM_LINES", config.lines.to_string()),
        ] {
            spec.env.insert(key.into(), value.into());
        }
        if config.asks {
            let id = format!("q{}.{}", ctx.firing.raw(), ctx.attempt.raw());
            let mut question = Question::new(&id, "Go on?");
            question.timeout_ms = config.ask_ms;
            let _ = ctx.logs.send(question.to_event()).await;
            let deadline = config
                .ask_ms
                .map(|ms| time::Instant::now() + Duration::from_millis(ms));
            loop {
                let expiry = async {
                    match deadline {
                        Some(at) => time::sleep_until(at).await,
                        None => pending().await,
                    }
                };
                // The answer first: one that lands as the deadline passes is
                // in time.
                let control = tokio::select! {
                    biased;
                    control = ctx.control.recv() => control,
                    () = expiry => {
                        let expired = QuestionExpired {
                            question:  id.clone(),
                            waited_ms: config.ask_ms.unwrap_or_default(),
                            default:   None,
                        };
                        let _ = ctx.logs.send(expired.to_event()).await;
                        break;
                    }
                };
                match control {
                    Some(Control::Deliver(value)) => {
                        if Answer::from_value(&value)
                            .and_then(|answer| answer.question)
                            .is_some_and(|question| question == id)
                        {
                            break;
                        }
                    }
                    Some(Control::Cancel | Control::Kill) if config.wedged => {}
                    // A stop, or anything newer, while waiting ends the attempt.
                    _ => return Outcome::cancelled(),
                }
            }
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

pub fn sandboxed_registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(SandboxedStep);
    registry
}

/// Step output kept in memory: the simulation writes no file.
#[derive(Debug, Default)]
pub struct MemoryLogs(Mutex<BTreeMap<String, Vec<String>>>);

impl MemoryLogs {
    /// How many lines the logs hold.
    pub fn lines(&self) -> usize {
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

// ── A host's hooks ────────────────────────────────────────────────────────

/// A roll for one attempt and purpose, independent of the order hooks run in,
/// so an attempt a resumed driver runs again gets the same verdict.
fn attempt_roll(seed: u64, firing: u64, attempt: u32, purpose: u64) -> u64 {
    let mut dice = Dice(
        seed ^ firing.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ u64::from(attempt).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ purpose.wrapping_mul(0x1656_67B1_9E37_79F9),
    );
    dice.roll(100)
}

/// A host whose hooks take time, and sometimes block or skip an attempt or
/// fail its result preparation, fatally or not.
pub struct WorldHooks {
    pub world: Arc<World>,
    pub seed:  u64,
}

/// The reason a hook's block carries.
pub const HOOK_BLOCK: &str = "simulated hook block";
/// The message a fatal result preparation failure carries.
pub const PREPARATION_FAILURE: &str = "simulated preparation failure";

#[async_trait::async_trait]
impl ExecutionHooks for WorldHooks {
    async fn before_attempt(
        &self,
        _context: &HookContext,
        request: AdmitAttempt,
    ) -> AttemptDecision {
        let (firing, attempt) = (request.view.firing.raw(), request.view.attempt.raw());
        time::sleep(Duration::from_millis(
            attempt_roll(self.seed, firing, attempt, 1) % 6,
        ))
        .await;
        let admission = match attempt_roll(self.seed, firing, attempt, 2) {
            0..=2 => Admission::Block {
                reason: HOOK_BLOCK.into(),
            },
            3..=5 => Admission::Skip {
                outcome: Outcome::success(Value::Null),
            },
            _ => Admission::Admit,
        };
        AttemptDecision {
            admission,
            notes: Vec::new(),
        }
    }

    async fn prepare_result(
        &self,
        _context: &HookContext,
        request: PrepareResult,
    ) -> Result<Prepared, PrepareError> {
        let (firing, attempt) = (request.view.firing.raw(), request.view.attempt.raw());
        time::sleep(Duration::from_millis(
            attempt_roll(self.seed, firing, attempt, 3) % 6,
        ))
        .await;
        match attempt_roll(self.seed, firing, attempt, 4) {
            0..=2 => {
                self.world.lock().fatal.insert((firing, attempt));
                Err(PrepareError::fatal(PREPARATION_FAILURE))
            }
            3..=5 => Err(PrepareError::best_effort("simulated preparation hiccup")),
            _ => Ok(Prepared::unchanged()),
        }
    }
}
