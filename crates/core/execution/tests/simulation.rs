//! Deterministic simulation testing of the execution layer (engine-spec §10).
//!
//! Each seed builds a root graph and up to three child graphs: layers of
//! sandboxed steps, invoke steps that call child graphs (one call or a fork,
//! in the caller's sandbox or their own, some through a fork gate), and
//! restart arms. Some root graphs turn on the circuit breaker, and some set
//! a low invocation limit. The run keeps its sandboxes as the seed's
//! retention says, and refuses or replaces a sandbox lost from outside. It
//! plans the host's cancels (the root, a second root cancel that kills, a
//! child invocation) and up to three crashes: at a virtual time, right
//! after a chosen coordinator record or resource record, or at a provider
//! call, before its effect or after it. Then it runs the real coordinator
//! through the host wrappers on a single-threaded runtime whose clock
//! starts paused, with the runtime's own lease router in front of the
//! world's sandbox provider. A crash drops the host future: the
//! coordinator, its drivers and their tasks end, their processes run on in
//! the world's sandboxes, and the dead lifetime's provider does nothing
//! more. Between lifetimes, someone may delete some of the world's
//! sandboxes. The next lifetime resumes over the same in-memory store, and
//! its router reconciles, fences and releases what the last one left. The
//! oracles:
//!
//! - the run ends, well within a bound, with a result, not an error;
//! - the coordinator log replays, and `run.finished` appears once, with the
//!   root result's status;
//! - every declared execution finishes, its engine log replays byte for byte,
//!   and its engine exit is the one the coordinator recorded;
//! - every invocation finishes, after all of its descendants, within the
//!   invocation limit;
//! - a restart declares one successor, which carries its predecessor's firing
//!   counts;
//! - a fork gate never runs more children's drivers than it has slots, and its
//!   children start in the order they were declared;
//! - a cancel reaches every unfinished descendant at once, and no execution is
//!   cancelled twice or killed before it was cancelled;
//! - the result a caller received is the child's recorded result;
//! - every block comes from the breaker or the execution limit;
//! - the leases: no create, stop or delete reaches the provider before its
//!   intent is recorded, and no process starts in a sandbox its lease does not
//!   record live; every lease ends settled, its sandbox as its record says,
//!   kept only as the run's retention keeps it, and released once with a
//!   `scope.released` before `run.finished`; no sandbox outlives the run unless
//!   its lease keeps it or its last release failed;
//! - the world's rules hold: no process outlives the run, none runs beside an
//!   unfenced one a dead lifetime left, and no attempt runs twice in a
//!   lifetime, again after it finished, or while it was stopping;
//! - observers see each lifetime's coordinator records in order, without gaps.
//!
//! `PETRI_DST_SEEDS` sets how many seeds run (64 by default);
//! `PETRI_DST_SEED` runs one, to replay a failure, and `PETRI_DST_TRACE=1`
//! prints what it did.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::future::{self, Future};
use std::num::NonZeroU32;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use driver::ExecutionReport;
use engine::{Admission, EngineExit, EngineState, Event, EventLog, EventRecord, RouteDecision};
use execution::host::{self, HostError, HostRun};
use execution::{
    CoordinatorEvent, CoordinatorHandle, CoordinatorRecord, CoordinatorState, ExecutionId,
    ExecutionObserver, GraphDigest, InvocationId, LeaseState, LogId, ResourceLogRecord,
    RunStore as _, SandboxResourceRecord, read_coordinator_log, read_execution_log,
};
use executor::Retention;
use executor_sandbox::LostSandbox;
use ir::{
    Arm, Backoff, Budget, CancelScopeId, EdgeTransition, Graph, GraphBuilder, JoinPolicy, NodeId,
    RetryPolicy, RunStatus, ScopeId, StepRef, validate,
};
use serde_json::{Value, json};
use smol_str::SmolStr;
use store::Access;
use support::{Call, INVOKE, Leases, SimHost, digest_of, received, run_key, run_paused};
use testkit::sim::{self, CallCrash, Dice, Faults, Moment, SANDBOXED, SandboxState, WorldSandbox};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{self, Instant};

/// How long a run may take, in virtual time, before it counts as wedged.
const DEADLINE: Duration = Duration::from_secs(600);

const FAULTS: Faults = Faults {
    acquire_failure: 3,
    acquire_ms:      10,
    release_ms:      10,
    release_failure: 3,
};

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
        .unwrap_or(64);
    (0..count).collect()
}

// ── The workload ──────────────────────────────────────────────────────────

/// A step's script, as the driver simulation writes them. A wedged step
/// ignores every stop signal and works two seconds, past the driver's hard
/// deadline, so only the driver ends it.
fn sandboxed(dice: &mut Dice) -> StepRef {
    let wedged = dice.chance(4);
    let mut work = || {
        if wedged {
            2_000
        } else {
            [0, 5, 10, 20, 40][usize::try_from(dice.roll(5)).expect("below 5")]
        }
    };
    let work_ms = [work(), work(), work()];
    let exits: Vec<i32> = (0..3).map(|_| i32::from(dice.chance(25))).collect();
    StepRef::new(
        SANDBOXED,
        json!({
            "work_ms": work_ms,
            "exits": exits,
            "honor_term": !dice.chance(30),
            "wedged": wedged,
            "lines": dice.roll(3),
        }),
    )
}

/// One graph of the workload, with what the oracles need to know about it.
struct Built {
    graph:   Graph,
    digest:  GraphDigest,
    /// Whether its nodes may retry. A gated call targets only a graph whose
    /// nodes never do: a child waiting out a backoff hands its gate slot
    /// back, which the gate oracle does not model.
    retries: bool,
}

/// An entry, one or two layers of one or two nodes, and an exit, in one
/// scope. A middle node is sometimes an invoke step that calls the graphs
/// in `callees` (one call, or a fork of two), in its own sandbox or the
/// caller's, sometimes through a gate. Some graphs restart: a later node
/// has an arm back to an earlier one that ends the execution and starts
/// its successor there, bounded by the nodes' budgets.
fn graph(dice: &mut Dice, index: usize, callees: &[&Built]) -> Built {
    let retries = dice.chance(60);
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let mut count = 0;
    let mut add = |b: &mut GraphBuilder, dice: &mut Dice, middle: bool| {
        let name = format!("g{index}n{count}");
        count += 1;
        let invoke = middle && !callees.is_empty() && dice.chance(40);
        if invoke {
            let fork = dice.chance(40);
            // One gate per invoke node, as a frontend lowers one fork: its
            // name and its slots. The calls to a graph that never retries
            // wait on it.
            let gate = dice
                .chance(50)
                .then(|| (format!("gate-{name}"), 1 + u32::from(dice.chance(40))));
            let calls: Vec<Call> = (0..if fork { 2 } else { 1 })
                .map(|_| {
                    let callee = callees[usize::try_from(dice.roll(callees.len() as u64))
                        .expect("an index into the callees")];
                    Call {
                        graph:   callee.digest,
                        inherit: dice.chance(30),
                        gate:    gate.clone().filter(|_| !callee.retries),
                    }
                })
                .collect();
            let node = b.add_node(
                &format!("call-{name}"),
                scope,
                StepRef::new(INVOKE, json!({ "calls": calls })),
            );
            if retries && dice.chance(15) {
                b.node_mut(node).retry = RetryPolicy::attempts(2);
            }
            return node;
        }
        let node = b.add_node(&name, scope, sandboxed(dice));
        if retries && dice.chance(40) {
            let attempts = u32::try_from(dice.roll(2) + 2).expect("small");
            b.node_mut(node).retry = RetryPolicy::attempts(attempts).with_backoff(Backoff {
                initial: Duration::from_millis(2),
                factor:  2.0,
                max:     Duration::from_millis(50),
                jitter:  true,
            });
        }
        if dice.chance(15) {
            b.set_budget(node, Budget::new(1, Duration::from_millis(15)));
        }
        node
    };
    let entry = add(&mut b, dice, false);
    let mut order = vec![entry];
    let mut previous = vec![entry];
    for _ in 0..=dice.roll(2) {
        let width = dice.roll(2) + 1;
        let layer: Vec<NodeId> = (0..width).map(|_| add(&mut b, dice, true)).collect();
        for &from in &previous {
            for &to in &layer {
                b.link(from, to);
            }
        }
        order.extend(&layer);
        previous = layer;
    }
    let exit = add(&mut b, dice, false);
    for &from in &previous {
        b.link(from, exit);
    }
    order.push(exit);
    if order.len() > 2 && dice.chance(30) {
        // A restart from the exit back to a node before it, taken on a
        // failure or always. The budgets carry into the successor and bound
        // the loop.
        let target = order[usize::try_from(dice.roll(order.len() as u64 - 1)).expect("an index")];
        let guard = if dice.chance(50) {
            let failure = b.exprs().call("failure", Vec::new());
            Arm::when(target, failure)
        } else {
            Arm::always(target)
        };
        // It closes a cycle, so it is a back arm too, as the Attractor
        // lowering marks a `loop_restart` edge.
        let guard = guard.with_back();
        b.fan_out_groups(exit, vec![vec![guard]]);
        // The target heads a loop: it joins on any token (§8 invariant 8),
        // and the entry stays the entry although a back arm reaches it.
        b.set_join(target, JoinPolicy::Any);
        b.mark_entry(entry);
        let groups = &mut b.node_mut(exit).routing.groups;
        let last = groups.len() - 1;
        groups[last].arms[0].transition = EdgeTransition::Restart;
        b.set_budget(exit, Budget::looped(2));
    }
    let graph = b.build();
    validate(&graph).unwrap_or_else(|errors| panic!("graph {index} is invalid: {errors:?}"));
    Built {
        digest: digest_of(&graph),
        graph,
        retries,
    }
}

struct Workload {
    root:      Graph,
    children:  Vec<Graph>,
    /// Every graph by its digest.
    graphs:    BTreeMap<GraphDigest, Graph>,
    breaker:   Option<u32>,
    max_calls: Option<u32>,
}

/// The graphs, built from the deepest child up so every caller knows its
/// callees' digests, and the root's run policy.
fn workload(dice: &mut Dice) -> Workload {
    let total = usize::try_from(dice.roll(3) + 2).expect("small");
    let mut built: Vec<Built> = Vec::new();
    for index in (0..total).rev() {
        let callees: Vec<&Built> = built.iter().collect();
        // A child calls only deeper children, to depth two below the root.
        let callees = if index == 0 || index + 2 < total {
            callees
        } else {
            Vec::new()
        };
        let next = graph(dice, index, &callees);
        built.push(next);
    }
    let mut root = built.pop().expect("the root is built last");
    let breaker = dice
        .chance(30)
        .then(|| u32::try_from(dice.roll(3) + 1).expect("small"));
    let max_calls = dice
        .chance(25)
        .then(|| u32::try_from(dice.roll(4) + 2).expect("small"));
    root.graph.policy.loop_restart_signature_limit = breaker.and_then(NonZeroU32::new);
    root.graph.policy.max_invocations = max_calls.and_then(NonZeroU32::new);
    let graphs = built
        .iter()
        .map(|child| (child.digest, child.graph.clone()))
        .chain([(digest_of(&root.graph), root.graph.clone())])
        .collect();
    Workload {
        root: root.graph,
        children: built.into_iter().map(|child| child.graph).collect(),
        graphs,
        breaker,
        max_calls,
    }
}

// ── The host's plan ───────────────────────────────────────────────────────

/// When the host cancels: the root, the root again (which kills), and a
/// child invocation by its id.
#[derive(Clone, Copy, Debug)]
struct Stops {
    cancel:       Option<Duration>,
    kill:         Option<Duration>,
    cancel_child: Option<(Duration, u64)>,
}

fn stops(dice: &mut Dice) -> Stops {
    let cancel = dice
        .chance(40)
        .then(|| Duration::from_millis(dice.roll(150)));
    let kill = cancel
        .filter(|_| dice.chance(60))
        .map(|at| at + Duration::from_millis(dice.roll(60)));
    let cancel_child = dice
        .chance(30)
        .then(|| (Duration::from_millis(dice.roll(150)), dice.roll(4) + 1));
    Stops {
        cancel,
        kill,
        cancel_child,
    }
}

/// The coordinator record kinds a crash can follow. `root.finished` is the
/// root's `invocation.finished`, before `run.finished`.
const RECORD_KINDS: [&str; 7] = [
    "invocation.declared",
    "execution.declared",
    "execution.finished",
    "invocation.finished",
    "invocation.cancel.requested",
    "root.finished",
    "scope.released",
];

/// The resource record kinds a crash can follow: a lease's allocation, the
/// sandbox live, each intent, and each end.
const RESOURCE_KINDS: [&str; 6] = [
    "allocating",
    "live",
    "pending stop",
    "stopped",
    "pending delete",
    "deleted",
];

/// The provider calls a crash can land on.
const CALLS: [sim::Call; 4] = [
    sim::Call::Create,
    sim::Call::Stop,
    sim::Call::Start,
    sim::Call::Delete,
];

/// What ends one coordinator lifetime early.
#[derive(Clone, Copy, Debug)]
enum Crash {
    /// At this virtual time.
    At(Duration),
    /// Right after the `nth` coordinator record of this kind the lifetime
    /// appends.
    After { kind: &'static str, nth: usize },
    /// At a provider call, before its effect or after it.
    Call(CallCrash),
    /// Right after the `nth` resource record of this kind the lifetime
    /// appends.
    Resource { kind: &'static str, nth: usize },
}

fn pick<T: Copy>(dice: &mut Dice, choices: &[T]) -> T {
    choices[usize::try_from(dice.roll(choices.len() as u64)).expect("an index")]
}

fn crashes(dice: &mut Dice) -> Vec<Crash> {
    let count = [0, 0, 1, 1, 2, 3][usize::try_from(dice.roll(6)).expect("below 6")];
    (0..count)
        .map(|_| {
            let nth = usize::try_from(dice.roll(3)).expect("small");
            match dice.roll(10) {
                0..=2 => Crash::At(Duration::from_millis(dice.roll(200))),
                3..=5 => Crash::After {
                    kind: pick(dice, &RECORD_KINDS),
                    nth,
                },
                6 | 7 => Crash::Call(CallCrash {
                    call:   pick(dice, &CALLS),
                    nth:    nth + 1,
                    moment: if dice.chance(50) {
                        Moment::Before
                    } else {
                        Moment::After
                    },
                }),
                _ => Crash::Resource {
                    kind: pick(dice, &RESOURCE_KINDS),
                    nth,
                },
            }
        })
        .collect()
}

/// How the seed's run keeps its sandboxes, and how often someone outside
/// the run deletes one between two lifetimes.
fn leases(dice: &mut Dice) -> (Leases, u64) {
    let retention = pick(dice, &[
        Retention::Never,
        Retention::OnFailure,
        Retention::Always,
    ]);
    let lost = if dice.chance(50) {
        LostSandbox::Replace
    } else {
        LostSandbox::Refuse
    };
    let lose = if dice.chance(35) { 60 } else { 0 };
    (Leases { retention, lost }, lose)
}

/// The kind a crash trigger names a record by.
fn kind_of(record: &CoordinatorRecord) -> &'static str {
    match &record.body {
        CoordinatorEvent::InvocationDeclared { .. } => "invocation.declared",
        CoordinatorEvent::ExecutionDeclared { .. } => "execution.declared",
        CoordinatorEvent::ExecutionFinished { .. } => "execution.finished",
        CoordinatorEvent::InvocationFinished { invocation, .. }
            if *invocation == InvocationId::ROOT =>
        {
            "root.finished"
        }
        CoordinatorEvent::InvocationFinished { .. } => "invocation.finished",
        CoordinatorEvent::InvocationCancelRequested { .. } => "invocation.cancel.requested",
        CoordinatorEvent::RunStarted { .. } => "run.started",
        CoordinatorEvent::RunFinished { .. } => "run.finished",
        CoordinatorEvent::GraphRegistered { .. } => "graph.registered",
        CoordinatorEvent::ScopeReleased { .. } => "scope.released",
        CoordinatorEvent::RunNoteRecorded { .. } => "run.note.recorded",
        _ => "other",
    }
}

// ── The observer ──────────────────────────────────────────────────────────

/// One lifetime's observer: it keeps the coordinator records it saw and
/// pulls the crash trigger.
/// `(lifetime, seq, kind)` of every coordinator record the observers saw.
type Seen = Arc<Mutex<Vec<(u32, u64, &'static str)>>>;

struct Watch {
    lifetime: u32,
    seen:     Seen,
    trigger:  Option<(&'static str, usize)>,
    count:    Mutex<usize>,
    crash:    Arc<Notify>,
    gates:    Mutex<Gates>,
    shared:   Arc<Mutex<Shared>>,
}

/// What the observers of every lifetime add up.
#[derive(Default)]
struct Shared {
    violations: Vec<String>,
    /// How often a gate ran as many children as it has slots.
    full_gates: u64,
}

/// Which gate each invocation waits on, and which executions of each gate's
/// children are running: a driver runs from its first engine record in a
/// lifetime until its `execution.finished`. A gate bounds running drivers,
/// not declarations: a restarted child's successor is declared before it
/// waits for a slot.
#[derive(Default)]
struct Gates {
    of_invocation: BTreeMap<InvocationId, ((ExecutionId, SmolStr), u32)>,
    of_execution:  BTreeMap<ExecutionId, InvocationId>,
    running:       BTreeMap<(ExecutionId, SmolStr), BTreeSet<ExecutionId>>,
}

impl Gates {
    fn declare_invocation(
        &mut self,
        invocation: InvocationId,
        call: Option<&execution::ParentCallKey>,
        admission: Option<&execution::AttemptAdmission>,
    ) {
        if let (Some(call), Some(admission)) = (call, admission) {
            self.of_invocation.insert(
                invocation,
                (
                    (call.parent, admission.gate.clone()),
                    admission.max_parallel,
                ),
            );
        }
    }

    fn gate_of(&self, execution: ExecutionId) -> Option<&((ExecutionId, SmolStr), u32)> {
        self.of_invocation.get(self.of_execution.get(&execution)?)
    }
}

#[async_trait::async_trait]
impl ExecutionObserver for Watch {
    fn on_engine_record(&self, execution: ExecutionId, _: &EventRecord, _: u64, _: &EngineState) {
        let mut gates = self.gates.lock().unwrap_or_else(PoisonError::into_inner);
        let Some((key, slots)) = gates.gate_of(execution).cloned() else {
            return;
        };
        let running = gates.running.entry(key.clone()).or_default();
        if !running.insert(execution) {
            return;
        }
        let count = running.len();
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if count > slots as usize {
            shared.violations.push(format!(
                "gate {} of execution {} ran {running:?} on {slots} slots",
                key.1, key.0
            ));
        } else if count == slots as usize {
            shared.full_gates += 1;
        }
    }

    fn on_resumed(&self, state: &CoordinatorState) {
        let mut gates = self.gates.lock().unwrap_or_else(PoisonError::into_inner);
        for (id, invocation) in &state.invocations {
            gates.declare_invocation(
                *id,
                invocation.declaration.call.as_ref(),
                invocation.declaration.admission.as_ref(),
            );
        }
        for (execution, declared) in &state.executions {
            gates
                .of_execution
                .insert(*execution, declared.declaration.invocation);
        }
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        {
            let mut gates = self.gates.lock().unwrap_or_else(PoisonError::into_inner);
            match &record.body {
                CoordinatorEvent::InvocationDeclared {
                    invocation,
                    call,
                    admission,
                    ..
                } => gates.declare_invocation(*invocation, call.as_ref(), admission.as_ref()),
                CoordinatorEvent::ExecutionDeclared {
                    execution,
                    invocation,
                    ..
                } => {
                    gates.of_execution.insert(*execution, *invocation);
                }
                CoordinatorEvent::ExecutionFinished { execution, .. } => {
                    for running in gates.running.values_mut() {
                        running.remove(execution);
                    }
                }
                _ => {}
            }
        }
        let kind = kind_of(record);
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((self.lifetime, record.seq, kind));
        if let Some((wanted, nth)) = self.trigger
            && wanted == kind
        {
            let mut count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
            if *count == nth {
                self.crash.notify_one();
            }
            *count += 1;
        }
    }
}

/// The host: its stops, against whichever coordinator is running.
fn host(
    stops: Stops,
    current: Arc<Mutex<Option<CoordinatorHandle>>>,
    epoch: Instant,
) -> JoinHandle<()> {
    let handle = move || {
        current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    };
    tokio::spawn(async move {
        let mut plan: Vec<(Duration, Option<u64>)> = Vec::new();
        plan.extend(stops.cancel.map(|at| (at, None)));
        plan.extend(stops.kill.map(|at| (at, None)));
        plan.extend(stops.cancel_child.map(|(at, id)| (at, Some(id))));
        plan.sort_unstable();
        for (at, child) in plan {
            time::sleep_until(epoch + at).await;
            // A host whose coordinator is down asks the next one.
            let handle = loop {
                if let Some(handle) = handle() {
                    break handle;
                }
                time::sleep(Duration::from_millis(1)).await;
            };
            match child {
                Some(id) => handle.cancel(InvocationId::new(id)),
                None => handle.cancel_root(),
            }
        }
    })
}

// ── One seed ──────────────────────────────────────────────────────────────

#[derive(Debug, Default, PartialEq, Eq)]
struct Outcome {
    violations: Vec<String>,
    stats:      BTreeMap<&'static str, u64>,
    /// Every stored log, for the replay-twice check.
    logs:       String,
}

type HostRunFuture<'a> = Pin<Box<dyn Future<Output = Result<ExecutionReport, HostError>> + 'a>>;

/// The attempts whose finish the stored logs hold, and the firings still
/// stopping in them, by execution: what the world checks a resumed lifetime
/// against. And whether the root's result was stored, so the resume replays
/// it.
async fn stored_progress(
    sim: &SimHost,
    workload: &Workload,
) -> Option<(
    BTreeSet<(SmolStr, u64, u32)>,
    BTreeSet<(SmolStr, u64)>,
    bool,
)> {
    let logs = sim.store.open(&run_key(), Access::Read).await.ok()?;
    let records = read_coordinator_log(&*logs)
        .await
        .expect("the coordinator log decodes");
    let state = CoordinatorState::replay(&records).expect("the stored log replays");
    let mut finished = BTreeSet::new();
    let mut stopping = BTreeSet::new();
    for (execution, declared) in &state.executions {
        let tag = SmolStr::new(execution.environment_prefix());
        let Ok(decoded) = read_execution_log(&*logs, *execution).await else {
            continue;
        };
        let invocation = &state.invocations[&declared.declaration.invocation];
        let graph = &workload.graphs[&invocation.declaration.graph];
        for event in decoded.log.events() {
            if let Event::StepFinished {
                firing, attempt, ..
            } = event
            {
                finished.insert((tag.clone(), firing.raw(), attempt.raw()));
            }
        }
        if !decoded.log.is_empty() {
            let replayed = engine::replay(graph.clone(), &decoded.log);
            for firing in replayed.live_firings().filter(|firing| firing.cancelling) {
                stopping.insert((tag.clone(), firing.id.raw()));
            }
        }
    }
    let root_done = state
        .invocations
        .get(&InvocationId::ROOT)
        .is_some_and(|root| root.result.is_some());
    Some((finished, stopping, root_done))
}

fn simulate_world(seed: u64) -> Outcome {
    let mut dice = Dice(seed);
    let workload = workload(&mut dice);
    let stops = stops(&mut dice);
    let crashes = crashes(&mut dice);
    let (leases, lose) = leases(&mut dice);
    trace(|| {
        format!(
            "seed {seed}: {} graphs, breaker {:?}, limit {:?}, stops {stops:?}, crashes \
             {crashes:?}, leases {leases:?}, lose {lose}%",
            workload.graphs.len(),
            workload.breaker,
            workload.max_calls
        )
    });
    run_paused(async move {
        let mut sim = SimHost::new("execution-simulation", seed, FAULTS);
        sim.leases = leases;
        let epoch = sim.epoch;
        let current = Arc::new(Mutex::new(None));
        let host = host(stops, Arc::clone(&current), epoch);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut stats: BTreeMap<&'static str, u64> = BTreeMap::new();
        let mut violations = Vec::new();
        let mut plan = crashes.iter().copied();
        let mut lifetime = 0_u32;
        let mut stored_at_start = BTreeMap::new();
        let result = loop {
            let crash = plan.next();
            let notify = Arc::new(Notify::new());
            let watch = Arc::new(Watch {
                lifetime,
                seen: Arc::clone(&seen),
                trigger: match crash {
                    Some(Crash::After { kind, nth }) => Some((kind, nth)),
                    _ => None,
                },
                count: Mutex::new(0),
                crash: Arc::clone(&notify),
                gates: Mutex::new(Gates::default()),
                shared: Arc::clone(&shared),
            });
            if let Some(Crash::Call(at)) = crash {
                sim.world.crash_at(at, Arc::clone(&notify));
            }
            sim.watched.arm(
                match crash {
                    Some(Crash::Resource { kind, nth }) => Some((kind, nth)),
                    _ => None,
                },
                Arc::clone(&notify),
            );
            let runtime = sim.runtime();
            let hand = {
                let current = Arc::clone(&current);
                move |handle: CoordinatorHandle, _| {
                    *current.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
                }
            };
            let stored = if lifetime == 0 {
                0
            } else {
                sim.stored_or_empty().await.coordinator.len() as u64
            };
            stored_at_start.insert(lifetime, stored);
            // A crash before the run was stored leaves nothing to resume: the
            // host starts it again.
            let created = lifetime > 0 && sim.store.open(&run_key(), Access::Read).await.is_ok();
            let mut run: HostRunFuture<'_> = if created {
                Box::pin(host::resume_configured(
                    &runtime,
                    Vec::new(),
                    vec![watch],
                    hand,
                ))
            } else {
                let run = HostRun::new(workload.root.clone())
                    .with_children(workload.children.clone())
                    .observe(watch);
                Box::pin(host::run_configured(&runtime, run, hand))
            };
            let crash_now = async {
                match crash {
                    Some(Crash::At(at)) => time::sleep_until(epoch + at).await,
                    Some(Crash::After { .. } | Crash::Call(_) | Crash::Resource { .. }) => {
                        notify.notified().await;
                    }
                    None => future::pending().await,
                }
            };
            let ended: Option<Result<ExecutionReport, String>> = tokio::select! {
                biased;
                () = crash_now => None,
                result = &mut run => Some(result.map_err(|error| error.to_string())),
                () = time::sleep_until(epoch + DEADLINE) => {
                    Some(Err(format!("the run did not end within {DEADLINE:?}")))
                }
            };
            if let Some(result) = ended {
                break result;
            }
            // The crash: the lifetime is dead before its drivers drop, so no
            // release of theirs reaches the world.
            *stats.entry("crashes").or_default() += 1;
            match crash {
                Some(Crash::After { kind, .. }) => *stats.entry(kind).or_default() += 1,
                Some(Crash::Call(_)) => *stats.entry("crashes at provider calls").or_default() += 1,
                Some(Crash::Resource { .. }) => {
                    *stats.entry("crashes after resource records").or_default() += 1;
                }
                _ => {}
            }
            let (finished, stopping, root_done) =
                stored_progress(&sim, &workload).await.unwrap_or_default();
            if root_done {
                *stats.entry("terminal replays").or_default() += 1;
            }
            sim.world.begin_lifetime(finished, stopping);
            *current.lock().unwrap_or_else(PoisonError::into_inner) = None;
            drop(run);
            if lose > 0 {
                *stats.entry("lost sandboxes").or_default() +=
                    sim.world.lose_sandboxes(lose) as u64;
            }
            trace(|| format!("crash at {:?} ({crash:?})", epoch.elapsed()));
            lifetime += 1;
        };
        host.abort();
        let ended = epoch.elapsed();
        trace(|| {
            format!(
                "ended at {ended:?}: {:?}",
                result.as_ref().map(|r| r.status)
            )
        });
        if let Err(error) = &result {
            violations.push(format!("the run ended with an error: {error}"));
        }
        check(
            &sim,
            &workload,
            &seen,
            &stored_at_start,
            &mut stats,
            &mut violations,
        )
        .await;
        violations.extend(sim.world.violations());
        {
            let shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
            violations.extend(shared.violations.iter().cloned());
            *stats.entry("full gates").or_default() += shared.full_gates;
        }
        let stored = sim.stored().await;
        let logs = format!(
            "{}\n{}\n{:?}",
            stored.coordinator.join("\n"),
            stored.resources.join("\n"),
            stored.executions
        );
        Outcome {
            violations,
            stats,
            logs,
        }
    })
}

// ── The oracles ───────────────────────────────────────────────────────────

/// The invocation each invocation was called from, and each execution's
/// invocation.
fn parents(state: &CoordinatorState) -> BTreeMap<InvocationId, InvocationId> {
    state
        .invocations
        .iter()
        .filter_map(|(id, invocation)| {
            let call = invocation.declaration.call.as_ref()?;
            let parent = state.executions.get(&call.parent)?.declaration.invocation;
            Some((*id, parent))
        })
        .collect()
}

fn is_descendant(
    parents: &BTreeMap<InvocationId, InvocationId>,
    mut invocation: InvocationId,
    ancestor: InvocationId,
) -> bool {
    while let Some(parent) = parents.get(&invocation) {
        if *parent == ancestor {
            return true;
        }
        invocation = *parent;
    }
    false
}

#[expect(
    clippy::too_many_lines,
    reason = "one pass over the stored run, oracle by oracle, reads best in one place"
)]
async fn check(
    sim: &SimHost,
    workload: &Workload,
    seen: &Mutex<Vec<(u32, u64, &'static str)>>,
    stored_at_start: &BTreeMap<u32, u64>,
    stats: &mut BTreeMap<&'static str, u64>,
    violations: &mut Vec<String>,
) {
    let logs = sim
        .store
        .open(&run_key(), Access::Read)
        .await
        .expect("the run is in the store");
    let records = read_coordinator_log(&*logs)
        .await
        .expect("the coordinator log decodes");
    let state = match CoordinatorState::replay(&records) {
        Ok(state) => state,
        Err(error) => {
            violations.push(format!("the coordinator log does not replay: {error}"));
            return;
        }
    };
    trace(|| {
        let lines: Vec<String> = records
            .iter()
            .map(|record| {
                let body = serde_json::to_string(&record.body).expect("a record encodes");
                let body: String = body.chars().take(220).collect();
                format!("    {} {body}", record.seq)
            })
            .collect();
        format!("coordinator log:\n{}", lines.join("\n"))
    });
    let parents = parents(&state);
    let seq_of = |wanted: &dyn Fn(&CoordinatorEvent) -> bool| {
        records
            .iter()
            .find(|record| wanted(&record.body))
            .map(|record| record.seq)
    };

    // The run's end.
    let finishes: Vec<RunStatus> = records
        .iter()
        .filter_map(|record| match &record.body {
            CoordinatorEvent::RunFinished { status } => Some(*status),
            _ => None,
        })
        .collect();
    let root_status = state.invocations[&InvocationId::ROOT]
        .result
        .as_ref()
        .map(|result| result.status);
    if finishes.len() != 1 || root_status.is_none_or(|status| finishes[0] != status) {
        violations.push(format!(
            "run.finished {finishes:?} for a root that finished {root_status:?}"
        ));
    }

    // Executions: every one finishes, replays, and ended as recorded.
    let mut engine_logs: BTreeMap<ExecutionId, EventLog> = BTreeMap::new();
    for (execution, declared) in &state.executions {
        let invocation = &state.invocations[&declared.declaration.invocation];
        let graph = &workload.graphs[&invocation.declaration.graph];
        let log = match read_execution_log(&*logs, *execution).await {
            Ok(decoded) => decoded.log,
            Err(error) => {
                violations.push(format!(
                    "execution {execution}'s log does not read: {error}"
                ));
                continue;
            }
        };
        let Some(exit) = &declared.exit else {
            violations.push(format!("execution {execution} never finished"));
            continue;
        };
        if let Err(mismatch) = engine::verify_replay(graph.clone(), &log) {
            violations.push(format!(
                "execution {execution}'s log does not replay: {mismatch}"
            ));
        }
        let replayed = engine::replay(graph.clone(), &log);
        if replayed.exit() != Some(exit) {
            violations.push(format!(
                "execution {execution} recorded {exit:?}, but its log ends {:?}",
                replayed.exit()
            ));
        }
        // A restart declares one successor, with the predecessor's counts.
        if matches!(exit, EngineExit::Restart { .. }) {
            *stats.entry("restarts").or_default() += 1;
            match state.successor_of(*execution) {
                None => violations.push(format!("execution {execution} restarted into nothing")),
                Some(successor) => {
                    let carried = &state.executions[&successor].declaration.start.prior_firings;
                    let counted: BTreeMap<NodeId, u32> = replayed
                        .prior_firings()
                        .into_iter()
                        .filter(|(node, _)| graph.node(*node).is_some())
                        .collect();
                    if *carried != counted {
                        violations.push(format!(
                            "execution {successor} carried {carried:?}, but its predecessor \
                             counted {counted:?}"
                        ));
                    }
                }
            }
        }
        // Every block comes from the breaker or the execution limit.
        for event in log.events() {
            match event {
                Event::AdmissionDecided {
                    decision: Admission::Block { reason },
                    ..
                } => violations.push(format!("an admission was blocked for `{reason}`")),
                Event::RoutingResolved { groups, .. } => {
                    for group in groups {
                        if let RouteDecision::Block { reason } = &group.decision {
                            *stats.entry("blocks").or_default() += 1;
                            let known = reason.contains("failure cycle detected")
                                || reason.contains("loop_restart blocked")
                                || reason.contains("maximum executions");
                            if !known {
                                violations.push(format!("a route was blocked for `{reason}`"));
                            }
                        }
                    }
                }
                Event::KillRequested { .. } => *stats.entry("kills").or_default() += 1,
                _ => {}
            }
        }
        // No execution is cancelled twice, or killed before it is
        // cancelled.
        let root_stops: Vec<bool> = log
            .events()
            .filter_map(|event| match event {
                Event::CancelRequested {
                    target: engine::CancelTarget::Scope(scope),
                } if *scope == CancelScopeId::ROOT => Some(false),
                Event::KillRequested { scope } if *scope == CancelScopeId::ROOT => Some(true),
                _ => None,
            })
            .collect();
        if root_stops.iter().filter(|kill| !**kill).count() > 1 || root_stops.first() == Some(&true)
        {
            violations.push(format!(
                "execution {execution} was stopped {root_stops:?} (true is a kill)"
            ));
        }
        engine_logs.insert(*execution, log);
    }

    // Invocations: all finish, after their descendants, within the limit.
    let total = state.invocations.len() as u64;
    *stats.entry("children").or_default() += total - 1;
    if let Some(limit) = workload.max_calls
        && total > u64::from(limit)
    {
        violations.push(format!("{total} invocations, above the limit of {limit}"));
    }
    for (id, invocation) in &state.invocations {
        if invocation.result.is_none() {
            violations.push(format!("invocation {id} never finished"));
            continue;
        }
        if invocation.declaration.sandbox != execution::SandboxBinding::Isolated {
            *stats.entry("inherited sandboxes").or_default() += 1;
        }
        let finished_at = seq_of(
            &|event| matches!(event, CoordinatorEvent::InvocationFinished { invocation, .. } if invocation == id),
        );
        for (other, other_state) in &state.invocations {
            if other_state.result.is_none() || !is_descendant(&parents, *other, *id) {
                continue;
            }
            let other_at = seq_of(
                &|event| matches!(event, CoordinatorEvent::InvocationFinished { invocation, .. } if invocation == other),
            );
            if other_at > finished_at {
                violations.push(format!(
                    "invocation {id} finished before its descendant {other}"
                ));
            }
        }
    }

    // Fork gates: no more live children than slots, started in order.
    let mut gates: BTreeMap<(ExecutionId, SmolStr), (u32, Vec<InvocationId>)> = BTreeMap::new();
    for (id, invocation) in &state.invocations {
        if let (Some(call), Some(admission)) = (
            &invocation.declaration.call,
            &invocation.declaration.admission,
        ) {
            gates
                .entry((call.parent, admission.gate.clone()))
                .or_insert((admission.max_parallel, Vec::new()))
                .1
                .push(*id);
        }
    }
    for ((parent, gate), (_, children)) in &gates {
        *stats.entry("gated children").or_default() += children.len() as u64;
        let started: Vec<InvocationId> = records
            .iter()
            .filter_map(|record| match &record.body {
                CoordinatorEvent::ExecutionDeclared {
                    invocation,
                    predecessor: None,
                    ..
                } if children.contains(invocation) => Some(*invocation),
                _ => None,
            })
            .collect();
        if started != *children {
            violations.push(format!(
                "gate {gate} of execution {parent} started {started:?}, declared {children:?}"
            ));
        }
    }

    // Cancellation: a cancel reaches every descendant at once. The records
    // that follow a cancel, up to the first record of another kind, cancel
    // every descendant that was declared, unfinished and uncancelled then.
    let mut cancelled_so_far: BTreeSet<InvocationId> = BTreeSet::new();
    let mut declared_so_far: BTreeSet<InvocationId> = BTreeSet::new();
    let mut finished_so_far: BTreeSet<InvocationId> = BTreeSet::new();
    for (at, record) in records.iter().enumerate() {
        match &record.body {
            CoordinatorEvent::InvocationDeclared { invocation, .. } => {
                declared_so_far.insert(*invocation);
            }
            CoordinatorEvent::InvocationFinished { invocation, .. } => {
                finished_so_far.insert(*invocation);
            }
            CoordinatorEvent::InvocationCancelRequested { invocation, .. } => {
                *stats
                    .entry(if *invocation == InvocationId::ROOT {
                        "root cancels"
                    } else {
                        "child cancels"
                    })
                    .or_default() += 1;
                let batch: BTreeSet<InvocationId> = records[at..]
                    .iter()
                    .map_while(|next| match &next.body {
                        CoordinatorEvent::InvocationCancelRequested { invocation, .. } => {
                            Some(*invocation)
                        }
                        _ => None,
                    })
                    .collect();
                for other in &declared_so_far {
                    if is_descendant(&parents, *other, *invocation)
                        && !finished_so_far.contains(other)
                        && !cancelled_so_far.contains(other)
                        && !batch.contains(other)
                    {
                        violations.push(format!(
                            "the cancel of invocation {invocation} (seq {}) did not reach its \
                             descendant {other}",
                            record.seq
                        ));
                    }
                }
                cancelled_so_far.insert(*invocation);
            }
            _ => {}
        }
    }

    // Results: what a caller received is the child's recorded result.
    for (execution, log) in &engine_logs {
        let declared = &state.executions[execution];
        let invocation = &state.invocations[&declared.declaration.invocation];
        let graph = &workload.graphs[&invocation.declaration.graph];
        let replayed = engine::replay(graph.clone(), log);
        for record in replayed.history() {
            if !record.name.starts_with("call-") {
                continue;
            }
            let Value::Array(received_results) = &record.outcome.output else {
                continue;
            };
            for (index, got) in received_results.iter().enumerate() {
                let slot = format!("c{index}");
                let child = state.invocations.iter().find(|(_, child)| {
                    child.declaration.call.as_ref().is_some_and(|call| {
                        call.parent == *execution
                            && call.firing == record.firing
                            && call.attempt == record.attempt
                            && call.slot == slot
                    })
                });
                let expected = child
                    .and_then(|(_, child)| child.result.as_ref())
                    .map(received);
                if expected.as_ref() != Some(got) {
                    violations.push(format!(
                        "execution {execution}'s {} received {got}, but the child recorded \
                         {expected:?}",
                        record.name
                    ));
                }
                *stats.entry("results received").or_default() += 1;
            }
        }
        for record in replayed.history() {
            if let ir::Status::Failure(info) = &record.outcome.status
                && info.message.contains("invocation limit")
            {
                *stats.entry("limit refusals").or_default() += 1;
            }
        }
    }

    // Observers: each lifetime's records in order, without gaps, from where
    // the stored log stood.
    {
        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
        for (lifetime, start) in stored_at_start {
            let seqs: Vec<u64> = seen
                .iter()
                .filter(|(at, ..)| at == lifetime)
                .map(|(_, seq, _)| *seq)
                .collect();
            let expected: Vec<u64> = (*start..*start + seqs.len() as u64).collect();
            if seqs != expected && !(*lifetime == 0 && seqs.first() == Some(&0)) {
                violations.push(format!(
                    "lifetime {lifetime} observed {seqs:?}, from {start} in the store"
                ));
            }
        }
    }

    // The leases: each ends settled, with its sandbox as its record says,
    // kept only as the run's retention keeps it, and released with a
    // `scope.released` before the run's end. A release that failed at the
    // run's end is the one exception: its intent stays, and its sandbox with
    // it.
    let mut latest: BTreeMap<u64, SandboxResourceRecord> = BTreeMap::new();
    let mut story = Vec::new();
    for line in logs
        .read(&LogId::Resources)
        .await
        .expect("the resource log reads")
    {
        let line: ResourceLogRecord = line.decode().expect("a resource record decodes");
        story.push(format!(
            "    {} lease {} {}",
            line.seq,
            line.body.lease.raw(),
            support::resource_kind(&line.body)
        ));
        latest.insert(line.body.lease.raw(), line.body);
    }
    let sandboxes = sim.world.sandboxes();
    let calls = sim.world.calls();
    trace(|| {
        let calls: Vec<String> = calls.iter().map(|call| format!("    {call:?}")).collect();
        format!(
            "resource log:\n{}\nprovider calls:\n{}",
            story.join("\n"),
            calls.join("\n")
        )
    });
    let last_call_failed = |sandbox: &WorldSandbox| {
        calls
            .iter()
            .rev()
            .find(|call| call.sandbox == sandbox.id)
            .is_some_and(|call| call.failed)
    };
    let released: BTreeSet<u64> = records
        .iter()
        .filter_map(|record| match &record.body {
            CoordinatorEvent::ScopeReleased { lease, .. } => Some(lease.raw()),
            _ => None,
        })
        .collect();
    // A release that reported a problem is retried and recorded again; one
    // that went through ends the lease, so a second clean one released it
    // twice.
    let mut clean: BTreeMap<u64, usize> = BTreeMap::new();
    for record in &records {
        if let CoordinatorEvent::ScopeReleased {
            lease, problems, ..
        } = &record.body
            && problems.is_empty()
        {
            *clean.entry(lease.raw()).or_default() += 1;
        }
    }
    for (lease, count) in clean {
        if count > 1 {
            violations.push(format!("lease {lease} was released cleanly {count} times"));
        }
    }
    for (lease, record) in &latest {
        let sandbox = record.resource_id.as_deref().and_then(|id| {
            sandboxes
                .iter()
                .rev()
                .find(|sandbox| sandbox.id == id && sandbox.lease() == Some(&lease.to_string()))
        });
        let reserved = record.state == LeaseState::Allocating && record.fingerprint.is_none();
        if let Some(intent) = record.pending {
            if sandbox.is_some_and(last_call_failed) {
                *stats.entry("releases left failed").or_default() += 1;
            } else {
                violations.push(format!("lease {lease} ended with a pending {intent:?}"));
            }
        } else {
            match record.state {
                LeaseState::Deleted => {
                    if let Some(sandbox) = sandbox.filter(|s| s.state != SandboxState::Deleted) {
                        violations.push(format!(
                            "lease {lease} is deleted, but its sandbox {} is {:?}",
                            sandbox.id, sandbox.state
                        ));
                    }
                }
                LeaseState::Stopped => {
                    // Someone outside the run may delete a kept sandbox: the
                    // run cannot know.
                    if sandbox.is_none_or(|sandbox| {
                        sandbox.state != SandboxState::Stopped && !sandbox.lost
                    }) {
                        violations.push(format!(
                            "lease {lease} is kept, but its sandbox is {:?}",
                            sandbox.map(|sandbox| sandbox.state)
                        ));
                    }
                    let status = state
                        .invocations
                        .get(&record.allocation.invocation)
                        .and_then(|invocation| invocation.result.as_ref())
                        .map(|result| result.status);
                    let keeps = match sim.leases.retention {
                        Retention::Always => true,
                        Retention::OnFailure => status != Some(RunStatus::Success),
                        Retention::Never => false,
                    };
                    if keeps {
                        *stats.entry("kept sandboxes").or_default() += 1;
                    } else {
                        violations.push(format!(
                            "lease {lease} kept its sandbox under {:?} after {status:?}",
                            sim.leases.retention
                        ));
                    }
                }
                _ if reserved => {}
                other => violations.push(format!("lease {lease} ended {other:?}")),
            }
        }
        if !reserved && !released.contains(lease) {
            violations.push(format!(
                "lease {lease} ended {:?} with no scope.released",
                record.state
            ));
        }
    }
    *stats.entry("scope releases").or_default() += released.len() as u64;
    // No sandbox of the run outlives it, unless its lease keeps it or its
    // last release failed.
    for sandbox in &sandboxes {
        if sandbox.state == SandboxState::Deleted {
            continue;
        }
        let kept = sandbox
            .lease()
            .and_then(|lease| latest.get(&lease.parse().ok()?))
            .is_some_and(|record| {
                record.state == LeaseState::Stopped
                    && record.resource_id.as_deref() == Some(sandbox.id.as_str())
                    && sandbox.state == SandboxState::Stopped
            });
        if !kept && !last_call_failed(sandbox) {
            violations.push(format!(
                "sandbox {} ({:?}, created in lifetime {}) outlived the run",
                sandbox.id, sandbox.state, sandbox.lifetime
            ));
        }
    }
    *stats.entry("sandboxes created").or_default() += sandboxes.len() as u64;
    for call in &calls {
        let key = match call.call {
            _ if call.failed => "failed provider calls",
            sim::Call::Start => "adopted or fenced sandboxes",
            sim::Call::Delete if call.unrecorded => "swept sandboxes",
            sim::Call::Delete => "deleted sandboxes",
            sim::Call::Stop => "stopped sandboxes",
            sim::Call::Create => continue,
        };
        *stats.entry(key).or_default() += 1;
    }
    *stats.entry("fenced processes").or_default() += sim.world.fenced() as u64;
    let now = Instant::now();
    for process in sim.world.processes() {
        if !process.running(now) {
            continue;
        }
        let excused = sandboxes.iter().any(|sandbox| {
            sandbox.id == process.key
                && sandbox.incarnation == process.generation
                && sandbox.state == SandboxState::Running
                && last_call_failed(sandbox)
        });
        if !excused {
            violations.push(format!(
                "firing {} attempt {} of {} (lifetime {}) outlived the run",
                process.firing, process.attempt, process.execution, process.lifetime
            ));
        }
    }
    if workload.breaker.is_some() {
        *stats.entry("breaker runs").or_default() += 1;
    }
    if workload.max_calls.is_some() {
        *stats.entry("limited runs").or_default() += 1;
    }
}

/// [`simulate_world`], with a panic anywhere in the run as a violation of
/// its seed, so one seed's panic names the seed and the rest still run.
fn simulate_or_panic(seed: u64) -> Outcome {
    panic::catch_unwind(AssertUnwindSafe(|| simulate_world(seed))).unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        Outcome {
            violations: vec![format!("panicked: {message}")],
            ..Outcome::default()
        }
    })
}

/// Every seed keeps every rule, through crashes, cancels, kills, restarts,
/// forks, gates and the breaker.
#[test]
fn seeded_runs_keep_the_execution_rules() {
    let outcomes: Vec<(u64, Outcome)> = seeds()
        .into_iter()
        .map(|seed| (seed, simulate_or_panic(seed)))
        .collect();
    let failed: Vec<String> = outcomes
        .iter()
        .filter(|(_, outcome)| !outcome.violations.is_empty())
        .map(|(seed, outcome)| {
            format!(
                "seed {seed} (replay with PETRI_DST_SEED={seed}): {:#?}",
                outcome.violations
            )
        })
        .collect();
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
            "children",
            "inherited sandboxes",
            "gated children",
            "full gates",
            "restarts",
            "results received",
            "root cancels",
            "child cancels",
            "kills",
            "crashes",
            "terminal replays",
            "breaker runs",
            "blocks",
            "limited runs",
            "limit refusals",
            "fenced processes",
            "sandboxes created",
            "scope releases",
            "kept sandboxes",
            "deleted sandboxes",
            "stopped sandboxes",
            "adopted or fenced sandboxes",
            "lost sandboxes",
            "failed provider calls",
            "crashes at provider calls",
            "crashes after resource records",
        ] {
            assert!(
                totals.get(key).copied().unwrap_or_default() > 0,
                "no seed reached {key}: {totals:?}"
            );
        }
    }
}

/// A seed's run, crashes included, stores the same logs twice.
#[test]
fn a_seeded_run_replays_byte_for_byte() {
    for seed in 0..32 {
        assert_eq!(
            simulate_world(seed),
            simulate_world(seed),
            "seed {seed} ran two ways"
        );
    }
}
