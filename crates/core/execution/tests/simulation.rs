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
//! after a chosen coordinator record or resource record, at a provider
//! call, before its effect or after it, or at a store fault: one append to
//! the coordinator, resource or an engine log fails, before its records are
//! stored or after (a lost reply), and sometimes the store stays down for
//! the rest of the lifetime. A store fault ends the lifetime with the run's
//! error, and the host resumes; a run whose creation it cut short is started
//! again under its key. Some crashes leave a zombie: the store releases the
//! lifetime's lease, as an operator or a liveness check would, the next
//! lifetime takes the run, and the zombie runs on for a while beside it. Its
//! writes are refused; what it does without a write (processes, hooks,
//! provider calls it began) is the documented hazard of releasing a live
//! owner's lease, counted rather than flagged. Then it runs the real
//! coordinator
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
//! - the run ends, well within a bound, with a result, not an error (a store
//!   fault's error ends only its lifetime);
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
//! - observers see each lifetime's coordinator records in order, without gaps;
//! - the store keeps its contract: every log reads back gapless, and no
//!   coordinator record comes from two lifetimes (a zombie's writes are
//!   refused).
//!
//! `PETRI_DST_SEEDS` sets how many seeds run (64 by default), and a tenth as
//! many, at least 128, run twice to compare their logs; `PETRI_DST_SEED`
//! runs one, to replay a failure, and `PETRI_DST_TRACE=1` prints what it did.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::future::{self, Future};
use std::num::NonZeroU32;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use driver::ExecutionReport;
use engine::{
    Admission, DecisionId, EngineExit, EngineState, Event, EventLog, EventRecord, RouteDecision,
};
use execution::controls::ControlService;
use execution::host::{self, HostError, HostRun};
use execution::watchdog::{StallWatchdog, WatchdogTask};
use execution::{
    CoordinatorEvent, CoordinatorHandle, CoordinatorRecord, CoordinatorState, Delivery,
    ExecutionId, ExecutionObserver, GraphDigest, InterviewDispatcher, InterviewReceipt,
    InvocationId, LeaseState, LogId, ResourceLogRecord, RunStore as _, SandboxResourceRecord,
    read_coordinator_log, read_execution_log,
};
use executor::Retention;
use executor_sandbox::LostSandbox;
use ir::{
    Arm, Backoff, Budget, CancelScopeId, EdgeTransition, Graph, GraphBuilder, JoinPolicy, NodeId,
    RetryPolicy, RunStatus, ScopeId, StepRef, validate,
};
use serde_json::{Value, json};
use smol_str::SmolStr;
use steps::{Answer, Question, QuestionExpired};
use store::Access;
use support::{
    Call, FaultLog, INVOKE, Leases, SIMULATED_EPOCH_MS, SimHost, SimInterviewer, StoreFault,
    digest_of, received, run_key, run_paused,
};
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
    // Some steps ask the host first; most of those give up waiting after a
    // while.
    let asks = dice.chance(12);
    let ask_ms = dice.chance(70).then(|| 30 + dice.roll(90));
    StepRef::new(
        SANDBOXED,
        json!({
            "work_ms": work_ms,
            "exits": exits,
            "honor_term": !dice.chance(30),
            "wedged": wedged,
            "lines": dice.roll(3),
            "asks": asks,
            "ask_ms": ask_ms,
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
    /// The root's stall budget, which the host's watchdog enforces.
    stall:     Option<Duration>,
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
    let stall = dice
        .chance(30)
        .then(|| Duration::from_millis(60 + dice.roll(140)));
    root.graph.policy.stall_timeout = stall;
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
        stall,
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
    /// Pause at the first time, unpause at the second.
    pause:        Option<(Duration, Duration)>,
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
    let pause = dice.chance(35).then(|| {
        let at = Duration::from_millis(dice.roll(150));
        (at, at + Duration::from_millis(20 + dice.roll(200)))
    });
    Stops {
        cancel,
        kill,
        cancel_child,
        pause,
    }
}

/// The coordinator record kinds a crash can follow. `root.finished` is the
/// root's `invocation.finished`, before `run.finished`.
const RECORD_KINDS: [&str; 9] = [
    "invocation.declared",
    "execution.declared",
    "execution.finished",
    "invocation.finished",
    "invocation.cancel.requested",
    "root.finished",
    "scope.released",
    "run.paused",
    "run.unpaused",
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
    /// A store fault: one append fails, and the host resumes when the run
    /// ends with the error.
    Store(StoreFault),
}

fn pick<T: Copy>(dice: &mut Dice, choices: &[T]) -> T {
    choices[usize::try_from(dice.roll(choices.len() as u64)).expect("an index")]
}

fn crashes(dice: &mut Dice) -> Vec<Crash> {
    let count = [0, 0, 1, 1, 2, 3][usize::try_from(dice.roll(6)).expect("below 6")];
    (0..count)
        .map(|_| {
            let nth = usize::try_from(dice.roll(3)).expect("small");
            match dice.roll(12) {
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
                8 | 9 => Crash::Resource {
                    kind: pick(dice, &RESOURCE_KINDS),
                    nth,
                },
                _ => Crash::Store(store_fault(dice)),
            }
        })
        .collect()
}

/// A store fault: one append of a log's kind fails, before its records are
/// stored or after (a lost reply), and sometimes the store stays down.
fn store_fault(dice: &mut Dice) -> StoreFault {
    StoreFault {
        log:        pick(dice, &[
            FaultLog::Coordinator,
            FaultLog::Resources,
            FaultLog::Execution,
        ]),
        nth:        usize::try_from(dice.roll(4)).expect("small"),
        lost_reply: dice.chance(50),
        down:       dice.chance(30),
    }
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
        CoordinatorEvent::RunPaused => "run.paused",
        CoordinatorEvent::RunUnpaused => "run.unpaused",
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

/// What the host does at a planned time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Act {
    CancelRoot,
    CancelChild(u64),
    Pause,
    Unpause,
}

/// The host: its stops and its pause, against whichever coordinator and
/// control service are running.
fn host(
    stops: Stops,
    current: Arc<Mutex<Option<CoordinatorHandle>>>,
    controls: Arc<Mutex<Option<ControlService>>>,
    epoch: Instant,
) -> JoinHandle<()> {
    let handle = move || {
        current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    };
    let service = move || {
        controls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    };
    tokio::spawn(async move {
        let mut plan: Vec<(Duration, Act)> = Vec::new();
        plan.extend(stops.cancel.map(|at| (at, Act::CancelRoot)));
        plan.extend(stops.kill.map(|at| (at, Act::CancelRoot)));
        plan.extend(
            stops
                .cancel_child
                .map(|(at, id)| (at, Act::CancelChild(id))),
        );
        plan.extend(stops.pause.map(|(at, _)| (at, Act::Pause)));
        plan.extend(stops.pause.map(|(_, until)| (until, Act::Unpause)));
        plan.sort_unstable();
        for (at, act) in plan {
            time::sleep_until(epoch + at).await;
            if matches!(act, Act::Pause | Act::Unpause) {
                // A host whose run is down between lifetimes waits for the
                // next one's service.
                let service = loop {
                    if let Some(service) = service() {
                        break service;
                    }
                    time::sleep(Duration::from_millis(1)).await;
                };
                if act == Act::Pause {
                    service.pause();
                } else {
                    service.unpause().await;
                }
                continue;
            }
            // A host whose coordinator is down asks the next one.
            let handle = loop {
                if let Some(handle) = handle() {
                    break handle;
                }
                time::sleep(Duration::from_millis(1)).await;
            };
            match act {
                Act::CancelChild(id) => handle.cancel(InvocationId::new(id)),
                _ => handle.cancel_root(),
            }
        }
        // A lifetime that resumed paused after the pause ended is unpaused
        // again, as a person would.
        if stops.pause.is_some() {
            loop {
                time::sleep(Duration::from_millis(5)).await;
                if let Some(service) = service()
                    && service.is_paused()
                {
                    service.unpause().await;
                }
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

/// A lifetime its successor took the run from, still running: its run, when
/// it is finally gone, and its watchdog.
struct Zombie<'a> {
    run:      HostRunFuture<'a>,
    until:    Instant,
    lifetime: u32,
    watchdog: Option<WatchdogTask>,
}

/// Resolve when the zombie's run ends by itself or its time is up; never
/// without one.
async fn lingering(zombie: &mut Option<Zombie<'_>>) {
    match zombie {
        Some(zombie) => {
            tokio::select! {
                biased;
                _ = &mut zombie.run => {}
                () = time::sleep_until(zombie.until) => {}
            }
        }
        None => future::pending().await,
    }
}

/// The zombie is finally gone: its calls never return from here on.
fn lay_to_rest(sim: &SimHost, zombie: Zombie<'_>, stats: &mut BTreeMap<&'static str, u64>) {
    let Zombie {
        run,
        until,
        lifetime,
        watchdog,
    } = zombie;
    sim.world.lay(lifetime);
    if Instant::now() < until {
        *stats
            .entry("zombies that stopped by themselves")
            .or_default() += 1;
    }
    trace(|| format!("lifetime {lifetime} laid to rest"));
    drop(watchdog);
    drop(run);
}

/// What the store holds at a crash, for the next lifetime.
#[derive(Default)]
struct Progress {
    /// The attempts whose finish the stored logs hold, by execution: what
    /// the world checks the resumed lifetime against.
    finished:   BTreeSet<(SmolStr, u64, u32)>,
    /// The firings still stopping in them.
    stopping:   BTreeSet<(SmolStr, u64)>,
    /// The root's result was stored, so the resume replays it.
    root_done:  bool,
    /// The last recorded control is a pause.
    paused:     bool,
    /// The run's end was stored: there is nothing to resume.
    run_status: Option<RunStatus>,
}

async fn stored_progress(sim: &SimHost, workload: &Workload) -> Option<Progress> {
    let logs = sim.store.open(&run_key(), Access::Read).await.ok()?;
    let records = read_coordinator_log(&*logs)
        .await
        .expect("the coordinator log decodes");
    // A crash before `run.started` was stored leaves nothing to resume from.
    if records.is_empty() {
        return None;
    }
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
    Some(Progress {
        finished,
        stopping,
        root_done,
        paused: state.paused,
        run_status: state.run_status,
    })
}

fn simulate_world(seed: u64) -> Outcome {
    let mut dice = Dice(seed);
    let workload = workload(&mut dice);
    let stops = stops(&mut dice);
    let mut crashes = crashes(&mut dice);
    // A pause is worth crashing in: the resumed run must start held.
    if stops.pause.is_some() && dice.chance(50) {
        crashes.insert(0, Crash::After {
            kind: "run.paused",
            nth:  0,
        });
    }
    // A store fault is worth planning first: the first lifetime reaches it.
    if dice.chance(25) {
        crashes.insert(0, Crash::Store(store_fault(&mut dice)));
    }
    // Some stores are slow: each append waits up to this long, so records
    // queue behind the writer.
    let slow = dice.chance(30).then(|| 1 + dice.roll(20));
    // Some crashes leave a zombie: the lifetime runs on for a while after
    // its successor took the run.
    let lingers: Vec<Option<Duration>> = crashes
        .iter()
        .map(|crash| {
            (!matches!(crash, Crash::Store(_)) && dice.chance(30))
                .then(|| Duration::from_millis(5 + dice.roll(96)))
        })
        .collect();
    let (leases, lose) = leases(&mut dice);
    trace(|| {
        format!(
            "seed {seed}: {} graphs, breaker {:?}, limit {:?}, stops {stops:?}, crashes \
             {crashes:?}, lingers {lingers:?}, slow {slow:?}, leases {leases:?}, lose {lose}%",
            workload.graphs.len(),
            workload.breaker,
            workload.max_calls
        )
    });
    run_paused(async move {
        let mut sim = SimHost::new("execution-simulation", seed, FAULTS);
        sim.leases = leases;
        if let Some(most) = slow {
            sim.watched.slow(seed, most);
        }
        let epoch = sim.epoch;
        let current = Arc::new(Mutex::new(None));
        let controls_now = Arc::new(Mutex::new(None));
        let host = host(
            stops,
            Arc::clone(&current),
            Arc::clone(&controls_now),
            epoch,
        );
        let seen = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut stats: BTreeMap<&'static str, u64> = BTreeMap::new();
        if slow.is_some() {
            *stats.entry("slow stores").or_default() += 1;
        }
        let mut violations = Vec::new();
        let mut plan = crashes.iter().copied().zip(lingers.iter().copied());
        let mut zombie: Option<Zombie<'_>> = None;
        let mut lifetime = 0_u32;
        let mut stored_at_start = BTreeMap::new();
        let mut started_at = BTreeMap::new();
        // The interview receipt as the CLI keeps it on disk: written at each
        // outcome, continued by the next lifetime.
        let receipt: Arc<Mutex<Option<InterviewReceipt>>> = Arc::new(Mutex::new(None));
        let result = loop {
            let planned = plan.next();
            let crash = planned.map(|(crash, _)| crash);
            let linger = planned.and_then(|(_, linger)| linger);
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
                match crash {
                    Some(Crash::Store(fault)) => Some(fault),
                    _ => None,
                },
            );
            // The host services a CLI process installs, one set per
            // lifetime.
            let controls = ControlService::new();
            *controls_now.lock().unwrap_or_else(PoisonError::into_inner) = Some(controls.clone());
            let watchdog = workload.stall.map(StallWatchdog::new);
            let watchdog_task: Arc<Mutex<Option<WatchdogTask>>> = Arc::new(Mutex::new(None));
            let alive = Arc::new(AtomicBool::new(true));
            let earlier = receipt
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let dispatcher =
                InterviewDispatcher::continuing(Arc::new(SimInterviewer { seed }), earlier);
            {
                let receipt = Arc::clone(&receipt);
                let alive = Arc::clone(&alive);
                dispatcher.publish_to(Arc::new(move |published: &InterviewReceipt| {
                    // A dead process writes nothing.
                    if alive.load(Ordering::Acquire) {
                        *receipt.lock().unwrap_or_else(PoisonError::into_inner) =
                            Some(published.clone());
                    }
                }));
            }
            let runtime = sim.runtime().hooks(controls.hooks(Some(
                sim.hooks.for_lifetime(Arc::clone(&sim.world), lifetime),
            )));
            let mut observers: Vec<Arc<dyn ExecutionObserver>> = vec![
                watch,
                Arc::new(controls.clone()),
                Arc::new(dispatcher.clone()),
            ];
            if let Some(watchdog) = &watchdog {
                observers.push(Arc::new(watchdog.clone()));
            }
            let hand = {
                let current = Arc::clone(&current);
                let controls = controls.clone();
                let watchdog = watchdog.clone();
                let watchdog_task = Arc::clone(&watchdog_task);
                let dispatcher = dispatcher.clone();
                move |handle: CoordinatorHandle, secrets| {
                    *current.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle.clone());
                    controls.wire(handle.clone());
                    dispatcher.wire(handle.clone(), secrets);
                    if let Some(watchdog) = &watchdog {
                        *watchdog_task.lock().unwrap_or_else(PoisonError::into_inner) =
                            Some(watchdog.start(handle));
                    }
                }
            };
            started_at.insert(
                lifetime,
                SIMULATED_EPOCH_MS + u64::try_from(epoch.elapsed().as_millis()).unwrap_or(0),
            );
            let stored = if lifetime == 0 {
                0
            } else {
                sim.stored_or_empty().await.coordinator.len() as u64
            };
            stored_at_start.insert(lifetime, stored);
            // A crash before the run was stored leaves nothing to resume: the
            // host starts it again.
            let created = lifetime > 0 && sim.store.open(&run_key(), Access::Read).await.is_ok();
            // The run owns its runtime, so a zombie outlives this pass of
            // the loop. A run whose creation was cut short is refused as
            // never started: the host starts it again under the same key.
            let mut run: HostRunFuture<'_> = {
                let workload = &workload;
                Box::pin(async move {
                    let start = |observers: Vec<Arc<dyn ExecutionObserver>>, hand| {
                        let mut run = HostRun::new(workload.root.clone())
                            .with_children(workload.children.clone());
                        for observer in observers {
                            run = run.observe(observer);
                        }
                        host::run_configured(&runtime, run, hand)
                    };
                    if created {
                        let resumed = host::resume_configured(
                            &runtime,
                            Vec::new(),
                            observers.clone(),
                            hand.clone(),
                        )
                        .await;
                        match resumed {
                            Err(HostError::NotStarted) => start(observers, hand).await,
                            resumed => resumed,
                        }
                    } else {
                        start(observers, hand).await
                    }
                })
            };
            let crash_now = async {
                match crash {
                    Some(Crash::At(at)) => time::sleep_until(epoch + at).await,
                    Some(Crash::After { .. } | Crash::Call(_) | Crash::Resource { .. }) => {
                        notify.notified().await;
                    }
                    Some(Crash::Store(_)) | None => future::pending().await,
                }
            };
            tokio::pin!(crash_now);
            let ended: Option<Result<RunStatus, String>> = loop {
                tokio::select! {
                    biased;
                    () = &mut crash_now => break None,
                    result = &mut run => break Some(
                        result
                            .map(|report| report.status)
                            .map_err(|error| error.to_string()),
                    ),
                    () = lingering(&mut zombie) => {
                        if let Some(ended) = zombie.take() {
                            lay_to_rest(&sim, ended, &mut stats);
                        }
                    }
                    () = time::sleep_until(epoch + DEADLINE) => {
                        break Some(Err(format!("the run did not end within {DEADLINE:?}")));
                    }
                }
            };
            // A store fault ends a lifetime with the run's error: the host
            // process exits, and the next one resumes the run.
            let faulted = sim.watched.fault_fired();
            if faulted {
                *stats.entry("store faults").or_default() += 1;
                if let Some(Crash::Store(fault)) = crash {
                    if fault.lost_reply {
                        *stats.entry("lost replies").or_default() += 1;
                    }
                    if fault.down {
                        *stats.entry("stores down").or_default() += 1;
                    }
                }
            }
            let fault_ended = faulted && matches!(ended, Some(Err(_)));
            if let Some(result) = ended.clone().filter(|_| !fault_ended) {
                let task = watchdog_task
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(task) = task {
                    task.stop().await;
                }
                if watchdog.as_ref().and_then(StallWatchdog::tripped).is_some() {
                    *stats.entry("stall cancels").or_default() += 1;
                }
                let finished = dispatcher.shutdown().await;
                *receipt.lock().unwrap_or_else(PoisonError::into_inner) = Some(finished);
                if let Some(ended) = zombie.take() {
                    lay_to_rest(&sim, ended, &mut stats);
                }
                break result;
            }
            // The crash, or the fault's end: the lifetime is dead before its
            // drivers drop, so no release of theirs reaches the world.
            if fault_ended {
                *stats.entry("lifetimes a store fault ended").or_default() += 1;
                trace(|| format!("store fault ended the lifetime: {ended:?}"));
            } else {
                *stats.entry("crashes").or_default() += 1;
            }
            match crash {
                Some(Crash::After { kind, .. }) => *stats.entry(kind).or_default() += 1,
                Some(Crash::Call(_)) => *stats.entry("crashes at provider calls").or_default() += 1,
                Some(Crash::Resource { .. }) => {
                    *stats.entry("crashes after resource records").or_default() += 1;
                }
                Some(Crash::Store(_) | Crash::At(_)) | None => {}
            }
            let progress = stored_progress(&sim, &workload).await.unwrap_or_default();
            if progress.root_done {
                *stats.entry("terminal replays").or_default() += 1;
            }
            if progress.paused {
                *stats.entry("crashes while paused").or_default() += 1;
            }
            let run_status = progress.run_status;
            sim.world
                .begin_lifetime(progress.finished, progress.stopping);
            // The process is gone, and its lease with it, as a file lock ends
            // with its process: a write it still had in flight is refused.
            sim.store.release(&run_key());
            *current.lock().unwrap_or_else(PoisonError::into_inner) = None;
            *controls_now.lock().unwrap_or_else(PoisonError::into_inner) = None;
            alive.store(false, Ordering::Release);
            let task = watchdog_task
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            // The process lost its lease but not its life: the next lifetime
            // takes the run, as an operator's release or a liveness check
            // lets it, and this one runs on for a while. Otherwise the process
            // died, its watchdog with it.
            if let Some(linger) = linger.filter(|_| !fault_ended) {
                if let Some(earlier) = zombie.take() {
                    lay_to_rest(&sim, earlier, &mut stats);
                }
                sim.world.haunt(lifetime);
                *stats.entry("zombie lifetimes").or_default() += 1;
                zombie = Some(Zombie {
                    run,
                    until: Instant::now() + linger,
                    lifetime,
                    watchdog: task,
                });
            } else {
                drop(task);
                drop(run);
            }
            if lose > 0 {
                *stats.entry("lost sandboxes").or_default() +=
                    sim.world.lose_sandboxes(lose) as u64;
            }
            trace(|| format!("crash at {:?} ({crash:?})", epoch.elapsed()));
            // The crash came after the run's end was stored: the run is over,
            // and a host reads its result instead of resuming it.
            if let Some(status) = run_status {
                *stats.entry("crashes after the run's end").or_default() += 1;
                break Ok(status);
            }
            lifetime += 1;
        };
        if let Some(ended) = zombie.take() {
            lay_to_rest(&sim, ended, &mut stats);
        }
        host.abort();
        let (zombie_calls, zombie_starts) = sim.world.zombie_actions();
        *stats.entry("zombie provider calls").or_default() += zombie_calls as u64;
        *stats.entry("zombie process starts").or_default() += zombie_starts as u64;
        let ended = epoch.elapsed();
        trace(|| format!("ended at {ended:?}: {:?}", result.as_ref()));
        if let Err(error) = &result {
            violations.push(format!("the run ended with an error: {error}"));
        }
        let receipt = receipt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        check(
            &sim,
            &workload,
            &seen,
            &stored_at_start,
            &started_at,
            receipt.as_ref(),
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
    started_at: &BTreeMap<u32, u64>,
    receipt: Option<&InterviewReceipt>,
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
            CoordinatorEvent::RunFinished { status, .. } => Some(*status),
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

    // The store's contract: every log reads back gapless, each record's seq
    // its position, and no coordinator seq was observed from two lifetimes,
    // as a write a zombie landed after its takeover would be.
    {
        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
        let mut writers: BTreeMap<u64, u32> = BTreeMap::new();
        for (lifetime, seq, _) in seen.iter() {
            if let Some(earlier) = writers.insert(*seq, *lifetime)
                && earlier != *lifetime
            {
                violations.push(format!(
                    "coordinator seq {seq} was observed from lifetimes {earlier} and {lifetime}"
                ));
            }
        }
    }
    let mut stored_logs = vec![LogId::Coordinator, LogId::Resources];
    stored_logs.extend(
        state
            .executions
            .keys()
            .map(|execution| LogId::Execution(*execution)),
    );
    for log in stored_logs {
        match logs.read(&log).await {
            Ok(stored) => {
                if let Some((at, record)) = stored
                    .iter()
                    .enumerate()
                    .find(|(at, record)| record.seq != *at as u64)
                {
                    violations.push(format!(
                        "the {log} log holds seq {} at position {at}",
                        record.seq
                    ));
                }
            }
            Err(error) => violations.push(format!("the {log} log does not read: {error}")),
        }
    }

    // Pause: no attempt is admitted while the run is durably paused, from a
    // `run.paused` to its `run.unpaused`, or to the end. The hold comes
    // before the record, so an admission in the pause's own millisecond is
    // one the hold had already let through.
    let mut windows: Vec<(u64, u64)> = Vec::new();
    let mut open: Option<u64> = None;
    for record in &records {
        match record.body {
            CoordinatorEvent::RunPaused if open.is_none() => open = Some(record.recorded_at),
            CoordinatorEvent::RunUnpaused => {
                if let Some(from) = open.take() {
                    windows.push((from, record.recorded_at));
                }
            }
            _ => {}
        }
    }
    windows.extend(open.map(|from| (from, u64::MAX)));
    *stats.entry("pauses").or_default() += windows.len() as u64;
    let paused_at = |at: u64| windows.iter().any(|(from, to)| at > *from && at < *to);
    let mut activity: Vec<u64> = records.iter().map(|record| record.recorded_at).collect();
    activity.extend(started_at.values().copied());
    // Questions: each answer names a question its firing asked, reaches the
    // log no later than the firing's end (the same instant is a race the
    // driver refuses; a later one is an answer the dispatcher sent while the
    // firing lived, which a slow store held up, and the receipt says it was
    // not live), no question is answered more often than it was asked, and
    // the times a question waited on the host park the watchdog.
    let not_live: BTreeSet<(ExecutionId, String)> = receipt
        .map(|receipt| {
            receipt
                .questions
                .iter()
                .filter(|question| question.delivery == Delivery::NotLive)
                .map(|question| (question.execution, question.question.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut waiting: Vec<(u64, u64)> = Vec::new();
    for execution in state.executions.keys() {
        let Ok(decoded) = read_execution_log(&*logs, *execution).await else {
            continue;
        };
        // No firing fails for the store: a failed write ends the lifetime
        // instead.
        for record in decoded.log.records() {
            let message = match &record.event {
                Event::ScopeFailed { error, causes, .. } => {
                    Some(format!("{error}: {}", causes.join(": ")))
                }
                Event::StepFinished { outcome, .. } => outcome
                    .status
                    .failure_info()
                    .map(|failure| failure.message.clone()),
                _ => None,
            };
            if let Some(message) = message
                && (message.contains("simulated store failure")
                    || message.contains("an earlier write to the run's store failed"))
            {
                violations.push(format!(
                    "execution {execution} recorded a store failure as a firing's: {message}"
                ));
            }
        }
        let mut open: BTreeMap<(u64, String), u64> = BTreeMap::new();
        let mut asks: BTreeMap<(u64, String), usize> = BTreeMap::new();
        let mut answers: BTreeMap<(u64, String), usize> = BTreeMap::new();
        let mut ended: BTreeMap<u64, u64> = BTreeMap::new();
        for (record, at) in decoded.log.records().iter().zip(&decoded.recorded_at) {
            match &record.event {
                Event::StepStarted { firing, .. } => {
                    ended.remove(&firing.raw());
                }
                Event::StepProgressRecorded { firing, ev } => {
                    if let Some(question) = Question::from_event(ev) {
                        let key = (firing.raw(), question.id);
                        *asks.entry(key.clone()).or_default() += 1;
                        open.entry(key).or_insert(*at);
                        *stats.entry("questions").or_default() += 1;
                    } else if let Some(expired) = QuestionExpired::from_event(ev) {
                        *stats.entry("expired questions").or_default() += 1;
                        if let Some(from) = open.remove(&(firing.raw(), expired.question)) {
                            waiting.push((from, *at));
                        }
                    }
                }
                Event::ControlRequested {
                    firing,
                    ctl: ir::Control::Deliver(value),
                } => {
                    if let Some(id) = Answer::from_value(value).and_then(|answer| answer.question) {
                        let key = (firing.raw(), id);
                        *stats.entry("answers").or_default() += 1;
                        if !asks.contains_key(&key) {
                            violations.push(format!(
                                "execution {execution} answered {key:?}, which it never asked"
                            ));
                        }
                        if let Some(end) = ended.get(&key.0)
                            && end < at
                            && !not_live.contains(&(*execution, key.1.clone()))
                        {
                            violations.push(format!(
                                "execution {execution} answered {key:?} at {at}, after the \
                                 firing ended at {end}"
                            ));
                        }
                        *answers.entry(key.clone()).or_default() += 1;
                        if let Some(from) = open.remove(&key) {
                            waiting.push((from, *at));
                        }
                    }
                }
                Event::StepFinished { firing, .. } => {
                    ended.insert(firing.raw(), *at);
                    let asked: Vec<_> = open
                        .keys()
                        .filter(|(asker, _)| *asker == firing.raw())
                        .cloned()
                        .collect();
                    for key in asked {
                        if let Some(from) = open.remove(&key) {
                            waiting.push((from, *at));
                        }
                    }
                }
                _ => {}
            }
        }
        for (key, count) in &answers {
            let asked = asks.get(key).copied().unwrap_or_default();
            if *count > asked {
                violations.push(format!(
                    "execution {execution} answered {key:?} {count} times, asked {asked} times"
                ));
            }
        }
        waiting.extend(open.values().map(|from| (*from, u64::MAX)));
        for (record, at) in decoded.log.records().iter().zip(&decoded.recorded_at) {
            activity.push(*at);
            if let Event::AdmissionDecided {
                decision_id: DecisionId::AttemptStart { firing, attempt },
                decision: Admission::Admit,
                ..
            } = &record.event
                && paused_at(*at)
            {
                violations.push(format!(
                    "execution {execution} admitted firing {firing} attempt {attempt} at {at}, \
                     while the run was paused ({windows:?})"
                ));
            }
        }
    }
    // A question waits on the host only while its lifetime lives: a resumed
    // run that does not ask it again has nothing waiting.
    for window in &mut waiting {
        if let Some(next) = started_at
            .values()
            .copied()
            .filter(|start| *start > window.0)
            .min()
        {
            window.1 = window.1.min(next);
        }
    }
    // The watchdog: a stall cancel only after a whole budget with no
    // record, no lifetime start and no unpause, and never while paused.
    if let Some(budget) = workload.stall {
        let budget = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX);
        for record in &records {
            let CoordinatorEvent::InvocationCancelRequested {
                reason: Some(execution::CancelReason::StallTimeout { .. }),
                ..
            } = &record.body
            else {
                continue;
            };
            let at = record.recorded_at;
            let last = activity
                .iter()
                .copied()
                .filter(|t| *t < at)
                .max()
                .unwrap_or(0);
            if at - last < budget {
                violations.push(format!(
                    "the watchdog cancelled at {at}, {} ms after the last activity (budget \
                     {budget})",
                    at - last
                ));
            }
            if paused_at(at) {
                violations.push(format!("the watchdog cancelled at {at}, while paused"));
            }
            if waiting.iter().any(|(from, to)| at > *from && at < *to) {
                violations.push(format!(
                    "the watchdog cancelled at {at}, while a question waited on the host"
                ));
            }
        }
    }

    // The receipt: every question is filed under the invocation that asked
    // it, at that invocation's path, through every resume.
    if let Some(receipt) = receipt {
        let mut paths: BTreeMap<InvocationId, String> = BTreeMap::new();
        paths.insert(InvocationId::ROOT, "/".to_owned());
        for (invocation, declared) in &state.invocations {
            if let Some(call) = &declared.declaration.call {
                let parent = state
                    .executions
                    .get(&call.parent)
                    .and_then(|parent| paths.get(&parent.declaration.invocation))
                    .cloned()
                    .unwrap_or_else(|| "/".to_owned());
                let path = if parent == "/" {
                    format!("/{}", call.slot)
                } else {
                    format!("{parent}/{}", call.slot)
                };
                paths.insert(*invocation, path);
            }
        }
        for question in &receipt.questions {
            let invocation = state
                .executions
                .get(&question.execution)
                .map(|declared| declared.declaration.invocation);
            if invocation != Some(question.invocation)
                || paths.get(&question.invocation) != Some(&question.invocation_path)
            {
                violations.push(format!(
                    "the receipt files `{}` of execution {} under invocation {} at {}; it \
                     belongs to {invocation:?} at {:?}",
                    question.question,
                    question.execution,
                    question.invocation,
                    question.invocation_path,
                    invocation.and_then(|invocation| paths.get(&invocation))
                ));
            }
            if question.lifetime < receipt.lifetime {
                *stats
                    .entry("receipt questions kept across a resume")
                    .or_default() += 1;
            }
        }
    }

    // Run notes, recorded at least once: every run-level hook call's note is
    // in the log, a note is recorded no more often than its point ran, and it
    // names the execution that ran it.
    let mut called: BTreeMap<(String, u64, Option<u64>), usize> = BTreeMap::new();
    for (kind, execution, scope) in sim.hooks.calls() {
        *called
            .entry((kind.to_owned(), execution, scope))
            .or_default() += 1;
    }
    let mut noted: BTreeMap<(String, u64, Option<u64>), usize> = BTreeMap::new();
    for record in &records {
        if let CoordinatorEvent::RunNoteRecorded {
            execution,
            kind,
            payload,
        } = &record.body
        {
            let key = (
                kind.to_string(),
                payload["execution"].as_u64().unwrap_or(u64::MAX),
                payload["scope"].as_u64(),
            );
            if execution.map(ExecutionId::raw) != Some(key.1) {
                violations.push(format!("the note {key:?} names execution {execution:?}"));
            }
            *noted.entry(key).or_default() += 1;
        }
    }
    *stats.entry("zombie hook calls").or_default() += sim.hooks.zombie_calls().len() as u64;
    for (key, count) in &noted {
        *stats.entry("run notes").or_default() += *count as u64;
        if *count > 1 {
            *stats.entry("repeated run notes").or_default() += 1;
        }
        if *count > called.get(key).copied().unwrap_or_default() {
            violations.push(format!(
                "the note {key:?} is recorded {count} times, its point ran fewer"
            ));
        }
    }
    for key in called.keys() {
        if !noted.contains_key(key) {
            violations.push(format!(
                "the {key:?} hook ran, and its note was never recorded"
            ));
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
            "pauses",
            "crashes while paused",
            "stall cancels",
            "questions",
            "answers",
            "expired questions",
            "receipt questions kept across a resume",
            "run notes",
            "repeated run notes",
            "store faults",
            "lifetimes a store fault ended",
            "lost replies",
            "stores down",
            "zombie lifetimes",
            "slow stores",
        ] {
            assert!(
                totals.get(key).copied().unwrap_or_default() > 0,
                "no seed reached {key}: {totals:?}"
            );
        }
    }
}

/// A seed's run, crashes included, stores the same logs twice. Enough seeds
/// that some hold several attempts at a pause, whose wake order must not
/// vary.
#[test]
fn a_seeded_run_replays_byte_for_byte() {
    let count = env::var("PETRI_DST_SEEDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(128, |count| (count / 10).max(128));
    for seed in 0..count {
        assert_eq!(
            simulate_world(seed),
            simulate_world(seed),
            "seed {seed} ran two ways"
        );
    }
}
