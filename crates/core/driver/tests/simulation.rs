//! Deterministic simulation testing of the driver (engine-spec §10).
//!
//! Each seed builds a workflow over two scopes, `for_each` expansions and
//! steps that ask the host a question included, a plan of host stops, a plan
//! of crashes, and a host: hooks that delay, block, skip or fail result
//! preparation; a decision resolver that sometimes fails; an observer whose
//! finish fails; a sibling execution that takes the shared attempt slot. It
//! runs the real driver in a simulated world on a single-threaded runtime
//! whose clock starts paused. The host answers every question its store
//! shows, again after each crash. A crash drops the running driver: its own
//! tasks end, its processes run on in the world, and its executor does
//! nothing more. A new driver resumes from the records its
//! observer had stored, sometimes with the tail of the last host event's
//! records lost. The oracles:
//!
//! - every run ends, well within a bound from its grace periods: a cancel ends
//!   it within its cleanup grace, and a kill within its hard deadline;
//! - every acquired environment is released once, or fenced by a later
//!   acquisition of its scope, and no process outlives the run;
//! - no process runs beside an unfenced one from an earlier acquisition of its
//!   scope, and no attempt runs again after the log recorded its finish or
//!   twice in one driver's lifetime, and a firing a stop reached is never
//!   started again after a crash;
//! - observers see each driver's records in order, without gaps, and a resumed
//!   driver re-delivers exactly the records it regenerated;
//! - the stored records are the final log, which replays byte for byte, and the
//!   run's status is its log's;
//! - no attempt runs while a sibling holds the slot; every block comes from a
//!   hook or a failed decision, and every result preparation failure from a
//!   fatal one; a failing observer is reported, and changes nothing else.
//!
//! `PETRI_DST_SEEDS` sets how many seeds run (128 by default);
//! `PETRI_DST_SEED` runs one, to replay a failure, and `PETRI_DST_TRACE=1`
//! prints what it did.

mod support;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use driver::lifecycle::RESULT_PREPARATION_CLASS;
use driver::{
    AdmissionResolution, AdmitRequest, DecisionError, DecisionResolver, Driver, EventObserver,
    ExecutionReport, ExecutionSlot, HookContext, ObserveError, RecordingClock, RoutingRequest,
    RoutingResolution, RunConfig, RunHandle, SeededDecisionResolver,
};
use engine::{
    Admission, CANCEL_ESCALATION_KEY, EngineExit, EngineState, Event, EventLog, EventOrigin,
    EventRecord, LOG_VERSION, RouteDecision,
};
use executor::MapSecrets;
use ir::{
    Backoff, Budget, CancelScopeId, ExecutionId, ExpandTarget, FiringId, Graph, GraphBuilder,
    InvocationId, JoinPolicy, NodeId, RetryPolicy, Scope, ScopeId, StepRef, parallel_for_each,
    validate,
};
use serde_json::json;
use smol_str::SmolStr;
use steps::{Answer, Question};
use store::RunKey;
use support::RunDir;
use testkit::sim::{
    Dice, Faults, HOOK_BLOCK, MemoryLogs, PREPARATION_FAILURE, SANDBOXED, World, WorldHooks,
    sandboxed_registry,
};
use tokio::runtime::Builder;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{self, Instant};

/// How long a run may take, in virtual time, before it counts as wedged.
const DEADLINE: Duration = Duration::from_secs(60);
/// How long after a kill, or a resume after one, the run may take to end.
const KILL_BOUND: Duration = Duration::from_secs(1);
const GRACE: Duration = Duration::from_millis(100);
const HARD_DEADLINE_SLACK: Duration = Duration::from_millis(300);
const CLEANUP_GRACE: Duration = Duration::from_millis(500);

/// Print a seed's story to stderr when `PETRI_DST_TRACE` is set.
#[expect(
    clippy::print_stderr,
    reason = "the trace is for a person replaying one failing seed"
)]
fn trace(line: impl FnOnce() -> String) {
    if env::var_os("PETRI_DST_TRACE").is_some() {
        eprintln!("{}", line());
    }
}

fn seeds() -> Vec<u64> {
    if let Some(seed) = env::var("PETRI_DST_SEED").ok().and_then(|v| v.parse().ok()) {
        return vec![seed];
    }
    let count = env::var("PETRI_DST_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128);
    (0..count).collect()
}

// ── The workload ──────────────────────────────────────────────────────────

/// A step's script. A wedged step ignores every stop signal and works two
/// seconds, past the driver's hard deadline, so only the driver ends it.
fn sandboxed(dice: &mut Dice) -> StepRef {
    let wedged = dice.chance(10);
    let mut work = || {
        if wedged {
            2_000
        } else {
            [0, 5, 10, 20, 40][usize::try_from(dice.roll(5)).expect("below 5")]
        }
    };
    let work_ms = [work(), work(), work()];
    let exits: Vec<i32> = (0..3).map(|_| i32::from(dice.chance(30))).collect();
    StepRef::new(
        SANDBOXED,
        json!({
            "work_ms": work_ms,
            "exits": exits,
            "honor_term": !dice.chance(30),
            "wedged": wedged,
            "lines": dice.roll(4),
            "asks": dice.chance(15),
        }),
    )
}

/// An entry, one to three layers of one to three steps, and an exit, over
/// two scopes. Each step feeds one or two steps of the next layer, and a
/// middle step is sometimes a `for_each` over one to three items. Steps draw
/// their work, exits, retries, timeouts, how they answer a stop, how much
/// they print and whether they ask a question; the exit is sometimes a
/// `run_on_cancel` cleanup.
fn workflow(dice: &mut Dice) -> Graph {
    let mut b = GraphBuilder::new();
    let second = b.add_scope(Scope::new(ScopeId::new(0)));
    let scopes = [ScopeId::new(0), second];
    let mut count = 0;
    let mut add = |b: &mut GraphBuilder, dice: &mut Dice| {
        let scope = scopes[usize::try_from(dice.roll(2)).expect("below 2")];
        let step = sandboxed(dice);
        let node = b.add_node(&format!("n{count}"), scope, step);
        count += 1;
        if dice.chance(50) {
            let attempts = u32::try_from(dice.roll(3) + 1).expect("small");
            b.node_mut(node).retry = RetryPolicy::attempts(attempts).with_backoff(Backoff {
                initial: Duration::from_millis(2),
                factor:  2.0,
                max:     Duration::from_millis(50),
                jitter:  true,
            });
        }
        if dice.chance(20) {
            b.set_budget(node, Budget::new(1, Duration::from_millis(15)));
        }
        node
    };
    let entry = add(&mut b, dice);
    let mut previous = vec![entry];
    let layers = dice.roll(3) + 1;
    for _ in 0..layers {
        let width = dice.roll(3) + 1;
        let layer: Vec<NodeId> = (0..width).map(|_| add(&mut b, dice)).collect();
        for node in &layer {
            if dice.chance(15) {
                let count = dice.roll(3) + 1;
                let items = b.exprs().lit(json!((0..count).collect::<Vec<_>>()));
                let max_parallel =
                    [None, Some(1), Some(2)][usize::try_from(dice.roll(3)).expect("small")];
                let fail_fast = dice.chance(30);
                parallel_for_each(
                    &mut b,
                    *node,
                    items,
                    ExpandTarget::Node,
                    max_parallel,
                    fail_fast,
                );
            }
        }
        feed(&mut b, dice, &previous, &layer);
        previous = layer;
    }
    let exit = add(&mut b, dice);
    b.node_mut(exit).run_on_cancel = dice.chance(40);
    feed(&mut b, dice, &previous, &[exit]);
    let graph = b.build();
    validate(&graph).expect("the workload builds valid graphs");
    graph
}

/// Route each node of `from` to one or two nodes of `to`, reaching every
/// node of `to`, which then joins with `Any`, or sometimes `All`.
fn feed(b: &mut GraphBuilder, dice: &mut Dice, from: &[NodeId], to: &[NodeId]) {
    let mut reached = BTreeSet::new();
    for (index, node) in from.iter().enumerate() {
        let mut targets = BTreeSet::from([to[index % to.len()]]);
        if to.len() > 1 && dice.chance(40) {
            targets.insert(to[usize::try_from(dice.roll(to.len() as u64)).expect("small")]);
        }
        reached.extend(targets.iter().copied());
        b.fan_out(*node, &targets.into_iter().collect::<Vec<_>>());
    }
    for node in to {
        if !reached.contains(node) {
            let last = from[from.len() - 1];
            let mut targets: Vec<NodeId> = b.node_mut(last).routing.edges().map(|e| e.to).collect();
            targets.push(*node);
            b.fan_out(last, &targets);
        }
        let join = if dice.chance(30) {
            JoinPolicy::All
        } else {
            JoinPolicy::Any
        };
        b.set_join(*node, join);
    }
}

/// When the host cancels the run, and whether it cancels again, which kills
/// it.
#[derive(Clone, Copy, Debug)]
struct Stops {
    cancel: Option<Duration>,
    kill:   Option<Duration>,
}

fn stops(dice: &mut Dice) -> Stops {
    if dice.chance(50) {
        return Stops {
            cancel: None,
            kill:   None,
        };
    }
    let cancel = Duration::from_millis(dice.roll(40));
    let kill = dice
        .chance(60)
        .then(|| cancel + Duration::from_millis(dice.roll(15)));
    Stops {
        cancel: Some(cancel),
        kill,
    }
}

/// The host around the driver.
#[derive(Clone, Debug)]
struct Hosting {
    hooks:             bool,
    failing_decisions: bool,
    failing_observer:  bool,
    /// When a sibling execution takes the shared attempt slot, and for how
    /// long; none means no shared slot.
    sibling:           Vec<(Duration, Duration)>,
    /// How long the host takes to answer a question.
    answer_after:      Duration,
}

fn hosting(dice: &mut Dice) -> Hosting {
    let windows = if dice.chance(30) { dice.roll(2) + 1 } else { 0 };
    let mut sibling: Vec<(Duration, Duration)> = (0..windows)
        .map(|_| {
            (
                Duration::from_millis(dice.roll(60)),
                Duration::from_millis(dice.roll(25) + 5),
            )
        })
        .collect();
    sibling.sort_unstable();
    Hosting {
        hooks: dice.chance(50),
        failing_decisions: dice.chance(50),
        failing_observer: dice.chance(30),
        sibling,
        answer_after: Duration::from_millis(dice.roll(10)),
    }
}

/// When the driver crashes, and whether the crash loses the tail of the last
/// host event's records. Some crashes land just after the host's cancel,
/// while firings are still stopping.
fn crashes(dice: &mut Dice, stops: Stops) -> Vec<(Duration, bool)> {
    let count = [0, 0, 1, 1, 2][usize::try_from(dice.roll(5)).expect("below 5")];
    let mut at: Vec<Duration> = (0..count)
        .map(|_| Duration::from_millis(dice.roll(90)))
        .collect();
    if let Some(cancel) = stops.cancel
        && dice.chance(40)
    {
        at.push(cancel + Duration::from_millis(dice.roll(10)));
    }
    if let Some(kill) = stops.kill
        && dice.chance(30)
    {
        at.push(kill);
    }
    at.sort_unstable();
    at.into_iter().map(|when| (when, dice.chance(50))).collect()
}

// ── The host's services ───────────────────────────────────────────────────

/// The message a failed decision carries.
const DECISION_FAILURE: &str = "simulated decision failure";

/// A decision resolver that fails 3 decisions in 100, which the driver turns
/// into a durable block. It answers only asynchronously, so the driver's
/// decision tasks run.
struct FailingDecisions {
    inner: SeededDecisionResolver,
    dice:  Mutex<Dice>,
}

impl FailingDecisions {
    fn fails(&self) -> bool {
        self.dice
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .chance(3)
    }
}

#[async_trait::async_trait]
impl DecisionResolver for FailingDecisions {
    async fn admit(&self, request: AdmitRequest) -> Result<AdmissionResolution, DecisionError> {
        if self.fails() {
            return Err(DecisionError::new(DECISION_FAILURE));
        }
        self.inner.admit(request).await
    }

    async fn route(&self, request: RoutingRequest) -> Result<RoutingResolution, DecisionError> {
        if self.fails() {
            return Err(DecisionError::new(DECISION_FAILURE));
        }
        self.inner.route(request).await
    }
}

/// The observer name a failing sink reports under.
const FAILING_OBSERVER: &str = "failing sink";

/// An observer whose finish always fails.
struct FailingObserver;

#[async_trait::async_trait]
impl EventObserver for FailingObserver {
    fn on_record(&self, _record: &EventRecord, _recorded_at: u64, _state: &EngineState) {}

    async fn finish(&self) -> Result<(), ObserveError> {
        Err(ObserveError::new(
            FAILING_OBSERVER,
            "simulated sink failure",
        ))
    }
}

// ── The durable store ─────────────────────────────────────────────────────

/// The records the run's observers stored, one per seq.
#[derive(Default)]
struct Store {
    records:    Mutex<Vec<EventRecord>>,
    violations: Mutex<Vec<String>>,
}

impl Store {
    fn violation(&self, message: String) {
        self.violations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(message);
    }
}

/// One driver's observer: it stores records, and tells the host about each
/// question a step asked.
struct Stored {
    store:    Arc<Store>,
    lifetime: u32,
    last:     Mutex<Option<u64>>,
    asked:    mpsc::UnboundedSender<(FiringId, String)>,
}

/// The question a record carries, with the firing that asked it.
fn question(record: &EventRecord) -> Option<(FiringId, String)> {
    let Event::StepProgressRecorded { firing, ev } = &record.event else {
        return None;
    };
    Question::from_event(ev).map(|question| (*firing, question.id))
}

impl EventObserver for Stored {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _state: &EngineState) {
        if let Some(asked) = question(record) {
            let _ = self.asked.send(asked);
        }
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = *last
            && record.seq != previous + 1
        {
            self.store.violation(format!(
                "driver lifetime {} delivered seq {} after {previous}",
                self.lifetime, record.seq
            ));
        }
        *last = Some(record.seq);
        let mut records = self
            .store
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let at = usize::try_from(record.seq).expect("a log fits in memory");
        match at.cmp(&records.len()) {
            Ordering::Equal => records.push(record.clone()),
            Ordering::Less if bytes(&records[at]) != bytes(record) => {
                self.store.violation(format!(
                    "driver lifetime {} re-delivered seq {} differently",
                    self.lifetime, record.seq
                ));
            }
            Ordering::Less => {}
            Ordering::Greater => self.store.violation(format!(
                "driver lifetime {} delivered seq {} with {} stored",
                self.lifetime,
                record.seq,
                records.len()
            )),
        }
    }
}

fn bytes(record: &EventRecord) -> Vec<u8> {
    serde_json::to_vec(record).expect("a record always encodes")
}

// ── The simulation ────────────────────────────────────────────────────────

/// What one seed's run showed.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    violations: Vec<String>,
    stats:      BTreeMap<&'static str, u64>,
    log:        String,
}

struct Lives {
    seed:     u64,
    graph:    Graph,
    world:    Arc<World>,
    store:    Arc<Store>,
    config:   RunConfig,
    hosting:  Hosting,
    asked:    mpsc::UnboundedSender<(FiringId, String)>,
    /// The attempt slot the driver shares with a sibling execution.
    slots:    Option<Arc<Semaphore>>,
    lifetime: u32,
}

impl Lives {
    /// Hand a driver its host: the observers, hooks, decisions and slot.
    fn equip(&self, driver: Driver) -> Driver {
        let mut driver = driver.observe(Arc::new(Stored {
            store:    Arc::clone(&self.store),
            lifetime: self.lifetime,
            last:     Mutex::new(None),
            asked:    self.asked.clone(),
        }));
        if self.hosting.failing_observer {
            driver = driver.observe(Arc::new(FailingObserver));
        }
        if self.hosting.hooks {
            let context =
                HookContext::new(RunKey::new("sim"), InvocationId::ROOT, ExecutionId::new(0));
            driver = driver.with_hooks(
                Arc::new(WorldHooks {
                    world: Arc::clone(&self.world),
                    seed:  self.seed,
                }),
                context,
            );
        }
        let decisions = SeededDecisionResolver::new(self.seed);
        driver = if self.hosting.failing_decisions {
            driver.with_decision_resolver(Arc::new(FailingDecisions {
                inner: decisions,
                dice:  Mutex::new(Dice(self.seed ^ u64::from(self.lifetime) ^ 0xDEC1_5105)),
            }))
        } else {
            driver.with_decision_resolver(Arc::new(decisions))
        };
        if let Some(slots) = &self.slots {
            driver = driver.with_attempt_slots(Arc::clone(slots), ExecutionSlot::empty());
        }
        driver
    }

    fn first(&self) -> Driver {
        self.equip(Driver::new(
            self.graph.clone(),
            Arc::new(self.world.executor()),
            sandboxed_registry(),
            Arc::new(MapSecrets::empty()),
            self.config.clone(),
        ))
    }

    /// The crash: what the store holds, sometimes less the tail of the last
    /// host event's records, is the log. The world's current driver is dead
    /// from here.
    fn crash(&mut self, lose_tail: bool, dice: &mut Dice) -> EventLog {
        let loaded = {
            let mut records = self
                .store
                .records
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if lose_tail
                && let Some(last) = records
                    .iter()
                    .rposition(|record| record.origin == EventOrigin::External)
            {
                let keep = last
                    + 1
                    + usize::try_from(dice.roll((records.len() - last) as u64)).expect("small");
                records.truncate(keep);
            }
            records.clone()
        };
        // The host re-sends what its store says a step asked.
        for record in &loaded {
            if let Some(asked) = question(record) {
                let _ = self.asked.send(asked);
            }
        }
        let log =
            EventLog::try_from_records(LOG_VERSION, loaded).expect("stored records are contiguous");
        let finished = log
            .events()
            .filter_map(|event| match event {
                Event::StepFinished {
                    firing, attempt, ..
                } => Some((SmolStr::default(), firing.raw(), attempt.raw())),
                _ => None,
            })
            .collect();
        let replayed = engine::replay(self.graph.clone(), &log);
        trace(|| {
            let live: Vec<String> = replayed
                .live_firings()
                .map(|firing| {
                    format!(
                        "{} (node {}, attempt {}, started {}, cancelling {}, retry {}, admission {})",
                        firing.id,
                        firing.node,
                        firing.attempt.raw(),
                        firing.started,
                        firing.cancelling,
                        firing.awaiting_retry,
                        replayed.is_awaiting_admission(firing.id)
                    )
                })
                .collect();
            let tail: Vec<String> = log
                .records()
                .iter()
                .rev()
                .take(12)
                .rev()
                .map(|record| {
                    let text = serde_json::to_string(&record.event).expect("encodes");
                    let text: String = text.chars().take(170).collect();
                    format!("    {} {:?} {text}", record.seq, record.origin)
                })
                .collect();
            format!("crash: live {live:?}\n{}", tail.join("\n"))
        });
        let stopping = replayed
            .live_firings()
            .filter(|firing| firing.cancelling)
            .map(|firing| (SmolStr::default(), firing.id.raw()))
            .collect();
        self.world.begin_lifetime(finished, stopping);
        self.lifetime += 1;
        log
    }

    /// A new driver from the log. Returns it with how many firings it
    /// re-dispatches.
    fn resume(&self, log: EventLog) -> (Driver, usize) {
        let (driver, info) = Driver::resume(
            self.graph.clone(),
            log,
            Arc::new(self.world.executor()),
            sandboxed_registry(),
            Arc::new(MapSecrets::empty()),
            self.config.clone(),
        )
        .expect("a stored prefix resumes");
        (self.equip(driver), info.redispatched.len())
    }
}

/// The host's answers: to each question it hears of, after its delay,
/// against whichever driver is running.
fn answerer(
    mut asked: mpsc::UnboundedReceiver<(FiringId, String)>,
    current: Arc<Mutex<Option<RunHandle>>>,
    after: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some((firing, id)) = asked.recv().await {
            time::sleep(after).await;
            let handle = current
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            if let Some(handle) = handle {
                let answer = Answer::text("yes").for_question(&id).to_control();
                let _ = handle.deliver(firing, answer).await;
            }
        }
    })
}

/// A sibling execution: in each window it waits for the shared slot, holds
/// it, and gives it back.
fn sibling(
    windows: Vec<(Duration, Duration)>,
    slots: Arc<Semaphore>,
    world: Arc<World>,
    epoch: Instant,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        for (at, hold) in windows {
            time::sleep_until(epoch + at).await;
            let Ok(permit) = Arc::clone(&slots).acquire_owned().await else {
                return;
            };
            world.sibling_holds_slot(true);
            time::sleep(hold).await;
            world.sibling_holds_slot(false);
            drop(permit);
        }
    })
}

/// The host: its stops, against whichever driver is running.
fn host(stops: Stops, current: Arc<Mutex<Option<RunHandle>>>, epoch: Instant) -> JoinHandle<()> {
    let handle = move || {
        current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    };
    tokio::spawn(async move {
        for at in [stops.cancel, stops.kill].into_iter().flatten() {
            time::sleep_until(epoch + at).await;
            if let Some(handle) = handle() {
                handle.cancel(CancelScopeId::ROOT).await;
            }
        }
    })
}

fn simulate_world(dir: &RunDir, seed: u64) -> Outcome {
    let mut dice = Dice(seed);
    let graph = workflow(&mut dice);
    let stops = stops(&mut dice);
    let crashes = crashes(&mut dice, stops);
    let hosting = hosting(&mut dice);
    trace(|| format!("seed {seed}: stops {stops:?}, crashes {crashes:?}, {hosting:?}"));
    trace(|| {
        graph
            .nodes
            .iter()
            .map(|node| {
                let targets: Vec<String> = node
                    .routing
                    .edges()
                    .map(|edge| edge.to.to_string())
                    .collect();
                format!(
                    "  {} {} in {}, {:?} join, {} attempts, {:?} -> [{}]",
                    node.id,
                    node.name,
                    node.scope,
                    node.join,
                    node.retry.max_attempts,
                    node.step.config,
                    targets.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    let world = World::new(seed, Faults {
        acquire_failure: 8,
        acquire_ms:      15,
        release_ms:      5,
    });
    let store = Arc::new(Store::default());
    let logs = Arc::new(MemoryLogs::default());
    let mut stats: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut violations = Vec::new();

    let runtime = Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a current-thread runtime builds");
    let report = runtime.block_on(async {
        let epoch = Instant::now();
        let clock = RecordingClock::new(move || {
            u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
        });
        let config = RunConfig::new(dir.path())
            .with_grace(GRACE)
            .with_cleanup_grace(CLEANUP_GRACE)
            .with_recording_clock(clock)
            .with_step_logs(logs.clone());
        let config = RunConfig {
            hard_deadline_slack: HARD_DEADLINE_SLACK,
            ..config
        };
        let (asked, questions) = mpsc::unbounded_channel();
        let slots = (!hosting.sibling.is_empty()).then(|| Arc::new(Semaphore::new(1)));
        let mut lives = Lives {
            seed,
            graph: graph.clone(),
            world: Arc::clone(&world),
            store: Arc::clone(&store),
            config,
            hosting: hosting.clone(),
            asked,
            slots: slots.clone(),
            lifetime: 0,
        };
        let current = Arc::new(Mutex::new(None));
        let host = host(stops, Arc::clone(&current), epoch);
        let answers = answerer(questions, Arc::clone(&current), hosting.answer_after);
        let sibling = slots.map(|slots| {
            sibling(hosting.sibling.clone(), slots, Arc::clone(&world), epoch)
        });
        let mut driver = lives.first();
        let mut plan = crashes.into_iter();
        let mut resumed_at = Duration::ZERO;
        let report = loop {
            *current.lock().unwrap_or_else(PoisonError::into_inner) = Some(driver.handle());
            let mut run = Box::pin(driver.run());
            let Some((at, lose_tail)) = plan.next() else {
                if let Ok(report) = time::timeout_at(epoch + DEADLINE, &mut run).await {
                    break Some(report);
                }
                violations.push(format!("the run did not end within {DEADLINE:?}"));
                break None;
            };
            // A run that ends as the crash lands has ended.
            tokio::select! {
                biased;
                report = &mut run => break Some(report),
                () = time::sleep_until(epoch + at) => {}
            }
            // The crash: the driver is dead, and its own tasks end with it.
            let log = lives.crash(lose_tail, &mut dice);
            drop(run);
            *stats.entry("crashes").or_default() += 1;
            *stats.entry("crashes losing records").or_default() += u64::from(lose_tail);
            let (next, redispatched) = lives.resume(log);
            *stats.entry("re-dispatched firings").or_default() += redispatched as u64;
            resumed_at = epoch.elapsed();
            trace(|| {
                format!(
                    "crash at {:?}{}: lifetime {} resumes from {} records, re-dispatching {redispatched}",
                    epoch.elapsed(),
                    if lose_tail { ", losing records" } else { "" },
                    lives.lifetime,
                    store.records.lock().unwrap_or_else(PoisonError::into_inner).len()
                )
            });
            driver = next;
        };
        host.abort();
        answers.abort();
        if let Some(sibling) = sibling {
            sibling.abort();
        }
        let ended = epoch.elapsed();
        trace(|| format!("ended at {ended:?}"));
        if let Some(kill) = stops.kill
            && report.is_some()
            && ended > kill.max(resumed_at) + KILL_BOUND
        {
            violations.push(format!(
                "a kill at {kill:?} left the run going until {ended:?}"
            ));
        }
        if let Some(cancel) = stops.cancel
            && report.is_some()
            && ended > cancel.max(resumed_at) + CLEANUP_GRACE + KILL_BOUND
        {
            violations.push(format!(
                "a cancel at {cancel:?} left the run going until {ended:?}"
            ));
        }
        // A process still running is a leak, unless it runs in a dead
        // driver's sandbox that nothing fenced: that one is crash recovery's.
        let now = Instant::now();
        let acquisitions = world.acquisitions();
        let last = world.lifetime();
        for process in world.processes() {
            if !process.running(now) {
                continue;
            }
            let recovery = acquisitions.iter().any(|a| {
                a.key == process.key
                    && a.generation == process.generation
                    && a.lifetime < last
                    && !a.fenced
                    && a.releases == 0
            });
            if recovery {
                *stats.entry("processes left to crash recovery").or_default() += 1;
            } else {
                violations.push(format!(
                    "firing {} attempt {} (driver lifetime {}) outlived the run",
                    process.firing, process.attempt, process.lifetime
                ));
            }
        }
        report
    });
    drop(runtime);

    trace(|| {
        world
            .acquisitions()
            .iter()
            .map(|a| format!("  acquired {a:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    });
    if let Some(report) = &report {
        trace(|| {
            report
                .state
                .log
                .records()
                .iter()
                .map(|record| {
                    let text = serde_json::to_string(&record.event).expect("encodes");
                    format!(
                        "  {} {:?} {}",
                        record.seq,
                        record.origin,
                        &text[..text.len().min(160)]
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
    }
    violations.extend(world.violations());
    violations.extend(
        store
            .violations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..),
    );
    let mut log = String::new();
    if let Some(report) = report {
        check_report(
            &graph,
            &report,
            &store,
            &world,
            &hosting,
            &mut stats,
            &mut violations,
        );
        log = serde_json::to_string(&report.state.log).expect("a log always encodes");
        for event in report.state.log.events() {
            let seen = match event {
                Event::StepProgressRecorded { ev, .. } if Question::from_event(ev).is_some() => {
                    "questions asked"
                }
                Event::ControlRequested { .. } => "answers delivered",
                Event::NodeExpanded { .. } => "expansions",
                Event::AdmissionDecided {
                    decision: Admission::Skip { .. },
                    ..
                } => "skipped attempts",
                Event::AdmissionDecided {
                    decision: Admission::Block { reason },
                    ..
                } if reason.contains(HOOK_BLOCK) => "hook blocks",
                Event::AdmissionDecided {
                    decision: Admission::Block { .. },
                    ..
                } => "decision blocks",
                Event::KillRequested { .. } => "kills",
                Event::CancelRequested { .. } => "cancels",
                Event::RetryElapsed { .. } => "retries",
                Event::ScopeFailed { .. } => "acquire failures",
                Event::StepFinished { outcome, .. } => {
                    match outcome
                        .output
                        .get(CANCEL_ESCALATION_KEY)
                        .and_then(|v| v.as_str())
                    {
                        Some("cancel_forced") => "forced finishes",
                        Some("cancelled_before_resume") => "cancelled before resume",
                        Some("killed_before_resume") => "killed before resume",
                        _ if outcome.status.tag() == "timed_out" => "timeouts",
                        _ => continue,
                    }
                }
                _ => continue,
            };
            *stats.entry(seen).or_default() += 1;
        }
    }
    *stats.entry("processes fenced").or_default() += world.fenced() as u64;
    *stats.entry("sibling turns").or_default() += world.sibling_turns() as u64;
    *stats.entry("fatal preparations").or_default() += world.fatal().len() as u64;
    *stats.entry("step log lines").or_default() += logs.lines() as u64;
    let acquisitions = world.acquisitions().len() as u64;
    *stats.entry("acquisitions").or_default() += acquisitions;
    Outcome {
        violations,
        stats,
        log,
    }
}

/// The oracles over a finished run.
fn check_report(
    graph: &Graph,
    report: &ExecutionReport,
    store: &Store,
    world: &World,
    hosting: &Hosting,
    stats: &mut BTreeMap<&'static str, u64>,
    violations: &mut Vec<String>,
) {
    // Every block comes from the host, and every result preparation failure
    // from a fatal one.
    let from_host = |reason: &str| reason.contains(HOOK_BLOCK) || reason.contains(DECISION_FAILURE);
    let fatal = world.fatal();
    for event in report.state.log.events() {
        match event {
            Event::AdmissionDecided {
                decision: Admission::Block { reason },
                ..
            } if !from_host(reason) => {
                violations.push(format!("an admission was blocked for `{reason}`"));
            }
            Event::RoutingResolved { groups, .. } => {
                for group in groups {
                    if let RouteDecision::Block { reason } = &group.decision
                        && !from_host(reason)
                    {
                        violations.push(format!("a routing was blocked for `{reason}`"));
                    }
                }
            }
            Event::StepFinished {
                firing,
                attempt,
                outcome,
            } if outcome
                .status
                .failure_info()
                .is_some_and(|info| info.class == RESULT_PREPARATION_CLASS) =>
            {
                if !fatal.contains(&(firing.raw(), attempt.raw())) {
                    violations.push(format!(
                        "firing {firing} attempt {attempt} failed its result preparation, which \
                         the host never failed"
                    ));
                }
                if !outcome
                    .status
                    .failure_info()
                    .is_some_and(|info| info.message.contains(PREPARATION_FAILURE))
                {
                    violations.push(format!(
                        "firing {firing} attempt {attempt}'s preparation failure lost its message"
                    ));
                }
            }
            _ => {}
        }
    }
    // A failing observer is reported, once.
    *stats.entry("failing observers").or_default() += u64::from(hosting.failing_observer);
    let failing = report
        .observer_errors
        .iter()
        .filter(|error| error.observer == FAILING_OBSERVER)
        .count();
    if failing != usize::from(hosting.failing_observer) || report.observer_errors.len() != failing {
        violations.push(format!(
            "the observers reported {:?}",
            report.observer_errors
        ));
    }
    let log = &report.state.log;
    let stored = store.records.lock().unwrap_or_else(PoisonError::into_inner);
    if stored.len() != log.len()
        || stored
            .iter()
            .zip(log.records())
            .any(|(ours, theirs)| bytes(ours) != bytes(theirs))
    {
        violations.push(format!(
            "the store holds {} records that are not the final log's {}",
            stored.len(),
            log.len()
        ));
    }
    if let Err(mismatch) = engine::verify_replay(graph.clone(), log) {
        violations.push(format!("the final log does not replay: {mismatch}"));
    }
    match report.state.exit() {
        Some(EngineExit::Terminal { status }) if *status == report.status => {}
        other => violations.push(format!(
            "the run reported {:?} but its log ended {other:?}",
            report.status
        )),
    }
    // Every sandbox is released, or gone with its acquisition. A dead
    // driver's that a later acquisition fenced is gone too; one nothing
    // fenced is crash recovery's.
    let last = world.lifetime();
    for acquired in world.acquisitions() {
        if acquired.failed || acquired.abandoned || acquired.releases > 0 {
            continue;
        }
        if acquired.lifetime < last {
            if !acquired.fenced {
                *stats.entry("sandboxes left to crash recovery").or_default() += 1;
            }
            continue;
        }
        violations.push(format!(
            "{} generation {} (driver lifetime {}) was never released",
            acquired.scope, acquired.generation, acquired.lifetime
        ));
    }
}

/// [`simulate_world`], with a panic anywhere in the run as a violation of
/// its seed, so one seed's panic names the seed and the rest still run.
fn simulate_or_panic(dir: &RunDir, seed: u64) -> Outcome {
    panic::catch_unwind(AssertUnwindSafe(|| simulate_world(dir, seed))).unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        Outcome {
            violations: vec![format!("panicked: {message}")],
            stats:      BTreeMap::new(),
            log:        String::new(),
        }
    })
}

fn failures(outcomes: &[(u64, Outcome)]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|(_, outcome)| !outcome.violations.is_empty())
        .map(|(seed, outcome)| {
            format!(
                "seed {seed} (replay with PETRI_DST_SEED={seed}): {:#?}",
                outcome.violations
            )
        })
        .collect()
}

/// Every seed keeps every rule, through crashes, lost records, stops, failed
/// acquisitions, retries and timeouts.
#[test]
fn seeded_worlds_keep_the_driver_rules() {
    let dir = RunDir::new("simulation");
    let outcomes: Vec<(u64, Outcome)> = seeds()
        .into_iter()
        .map(|seed| (seed, simulate_or_panic(&dir, seed)))
        .collect();
    let failed = failures(&outcomes);
    assert!(
        failed.is_empty(),
        "{} seeds broke a rule:\n{}",
        failed.len(),
        failed.join("\n")
    );
    let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
    for (_, outcome) in &outcomes {
        for (key, count) in &outcome.stats {
            *totals.entry(key).or_default() += count;
        }
    }
    trace(|| format!("{} seeds: {totals:#?}", outcomes.len()));
    if outcomes.len() >= 64 {
        for key in [
            "acquire failures",
            "answers delivered",
            "cancelled before resume",
            "cancels",
            "crashes",
            "crashes losing records",
            "decision blocks",
            "expansions",
            "failing observers",
            "fatal preparations",
            "forced finishes",
            "hook blocks",
            "kills",
            "processes fenced",
            "questions asked",
            "re-dispatched firings",
            "retries",
            "sibling turns",
            "skipped attempts",
            "step log lines",
            "timeouts",
        ] {
            assert!(
                totals.get(key).copied().unwrap_or_default() > 0,
                "no seed reached {key}: {totals:?}"
            );
        }
    }
}

/// A seed's world, crashes included, runs the same way twice.
#[test]
fn a_seeded_world_replays_byte_for_byte() {
    let dir = RunDir::new("simulation-replay");
    for seed in 0..16 {
        assert_eq!(
            simulate_world(&dir, seed),
            simulate_world(&dir, seed),
            "seed {seed} ran two ways"
        );
    }
}
