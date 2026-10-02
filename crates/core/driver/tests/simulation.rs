//! Deterministic simulation testing of the driver (engine-spec §10).
//!
//! Each seed builds a workflow over two scopes, a plan of host stops, and a
//! plan of crashes, then runs the real driver in a simulated world on a
//! single-threaded runtime whose clock starts paused. A crash drops the
//! running driver: its own tasks end, its processes run on in the world, and
//! its executor does nothing more. A new driver resumes from the records its
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
//!   run's status is its log's.
//!
//! `PETRI_DST_SEEDS` sets how many seeds run (128 by default);
//! `PETRI_DST_SEED` runs one, to replay a failure, and `PETRI_DST_TRACE=1`
//! prints what it did.

mod support;

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use driver::{Driver, EventObserver, ExecutionReport, RecordingClock, RunConfig, RunHandle};
use engine::{
    CANCEL_ESCALATION_KEY, EngineExit, EngineState, Event, EventLog, EventOrigin, EventRecord,
    LOG_VERSION,
};
use executor::MapSecrets;
use ir::{
    Backoff, Budget, CancelScopeId, Graph, GraphBuilder, JoinPolicy, NodeId, RetryPolicy, Scope,
    ScopeId, StepRef, validate,
};
use serde_json::json;
use support::RunDir;
use support::world::{Dice, Faults, MemoryLogs, SANDBOXED, World, sandboxed_registry};
use tokio::runtime::Builder;
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
        }),
    )
}

/// An entry, one to three layers of one to three steps, and an exit, over
/// two scopes. Each step feeds one or two steps of the next layer. Steps
/// draw their work, exits, retries, timeouts, how they answer a stop and how
/// much they print; the exit is sometimes a `run_on_cancel` cleanup.
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

/// One driver's observer.
struct Stored {
    store:    Arc<Store>,
    lifetime: u32,
    last:     Mutex<Option<u64>>,
}

impl EventObserver for Stored {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _state: &EngineState) {
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
    graph:    Graph,
    world:    Arc<World>,
    store:    Arc<Store>,
    config:   RunConfig,
    lifetime: u32,
}

impl Lives {
    fn observe(&self, driver: Driver) -> Driver {
        driver.observe(Arc::new(Stored {
            store:    Arc::clone(&self.store),
            lifetime: self.lifetime,
            last:     Mutex::new(None),
        }))
    }

    fn first(&self) -> Driver {
        self.observe(Driver::new(
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
        let log =
            EventLog::try_from_records(LOG_VERSION, loaded).expect("stored records are contiguous");
        let finished = log
            .events()
            .filter_map(|event| match event {
                Event::StepFinished {
                    firing, attempt, ..
                } => Some((firing.raw(), attempt.raw())),
                _ => None,
            })
            .collect();
        let replayed = engine::replay(self.graph.clone(), &log);
        let stopping = replayed
            .live_firings()
            .filter(|firing| firing.cancelling)
            .map(|firing| firing.id.raw())
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
        (self.observe(driver), info.redispatched.len())
    }
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
    trace(|| format!("seed {seed}: stops {stops:?}, crashes {crashes:?}"));
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
        let mut lives = Lives {
            graph: graph.clone(),
            world: Arc::clone(&world),
            store: Arc::clone(&store),
            config,
            lifetime: 0,
        };
        let current = Arc::new(Mutex::new(None));
        let host = host(stops, Arc::clone(&current), epoch);
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
                a.scope == process.scope
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
        check_report(&graph, &report, &store, &world, &mut stats, &mut violations);
        log = serde_json::to_string(&report.state.log).expect("a log always encodes");
        for event in report.state.log.events() {
            let seen = match event {
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
    stats: &mut BTreeMap<&'static str, u64>,
    violations: &mut Vec<String>,
) {
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
        .map(|seed| (seed, simulate_world(&dir, seed)))
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
            "cancelled before resume",
            "cancels",
            "crashes",
            "crashes losing records",
            "forced finishes",
            "kills",
            "processes fenced",
            "re-dispatched firings",
            "retries",
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
