//! The public event contract: one versioned stream a host projects a run
//! from, built only from durable records.
//!
//! # The rule
//!
//! A public event carries its record, unchanged, plus what Petri derived
//! beside it. Every [`RunEvent`] is either a record's own event or a view
//! event attached to one:
//!
//! - A record's own event is named after the record (`step.finished`,
//!   `execution.declared`): the `body.event` tag of the stored line. It carries
//!   the stored line itself under [`RunEvent::record`], exactly as the
//!   coordinator log or the execution's engine log holds it (`seq`, `origin`,
//!   `recorded_at`, `body`), serialized by the same types. Nothing is renamed,
//!   re-nested, dropped or lifted out of it. What Petri derived from the record
//!   and the post-apply state lives apart from it under [`RunEvent::derived`]:
//!   whether an attempt was final, the node a route resolved to, the answer a
//!   delivered control decoded to, Petri's reading of a step protocol it owns
//!   (`parsed`).
//! - A view event ([`ViewEvent`]: `visit.started`, `wait.state.changed`,
//!   `fork.completed` and the rest) has no record. It is derived from the state
//!   alone, marked [`RecordOrigin::Derived`], and follows the record whose
//!   apply produced it at `index` 1 and up.
//!
//! The same derivation runs live (as an
//! [`ExecutionObserver`](crate::ExecutionObserver)) and over a run's stored
//! logs ([`replay_run`]), so a host that lost its live subscription rebuilds
//! the same events, in the same order, with the same identities, from the
//! records alone. Nothing here reads a clock inside the state machine:
//! `recorded_at` is the time the record was appended to its log, read at that
//! boundary and persisted beside the record, so it is the same live and on
//! replay; `observed_at` is stamped by the projector when it sees a record live
//! and is absent on replay.
//!
//! # Identity and ordering
//!
//! [`EventId`] is `(log, seq, index)`: the log the record came from (the
//! coordinator log or one execution's engine log), the record's `seq` in
//! that log, and the ordinal of this event among the events one record
//! produced (`0` is the record's own event). Within one log the order is
//! total. Across logs the [`Context`] plus [`ParentLink`] tie an execution's
//! events to the invocation that declared it and to the parent firing that
//! called it.
//!
//! # Delivery
//!
//! [`EventProjector`] is the live path: the observer callback projects
//! synchronously and queues without waiting; a pump task hands each event to
//! the host's [`RunEventSink`] and awaits it, in order. The queue is bounded
//! ([`ProjectorOptions::capacity`], 1024 events by default), which is the
//! most the projector holds in memory: a slow sink delays delivery and never
//! slows the driver, and an event projected while the queue is full is not
//! queued. It is counted as `overflowed` in the [`ProjectionReceipt`], live
//! delivery goes on with the next event that finds room (so the sink sees
//! each log in order, with gaps), and the durable log keeps it. A sink
//! error stops the pump; every later event is counted as undelivered. A
//! `deliver` or `finish` that outlasts [`ProjectorOptions::stall_timeout`]
//! (30 seconds by default) is dropped and counts as a failure that names the
//! event, so [`EventProjector::shutdown`] completes within about one stall
//! budget plus the drain of the queue. None of this fails the run. A host
//! that needs completeness after an overflow, a failure or a stall calls
//! [`replay_run`] and deduplicates by [`EventId`]. A host that follows a
//! run through its store alone keeps a [`RunReplay`] and advances it: each
//! advance reads and folds only the records past the ones it consumed.
//!
//! # Crash recovery
//!
//! Read-only projection publishes what is stored. A crash can leave an
//! engine log short of the core records its last external record produced;
//! [`replay_run`] regenerates them to reach the right state but publishes no
//! event attached to a record that never reached the log. Resume writes those
//! records through the normal storage path, with normal recording times,
//! before its observers see them: the driver hands the regenerated suffix to
//! the log writer first and to every other observer after it, before it
//! dispatches pending work, so events for records the crash kept off disk
//! arrive with the same identities. Delivery is at-least-once, deduplicated
//! by [`EventId`]. Records before the loaded prefix are not re-delivered
//! live; [`replay_run`] covers them. A projector attached at resume is built
//! with [`EventProjector::primed`], which folds the stored prefix into its
//! state without delivering it, so the suffix derives the same events it
//! would have derived live.
//!
//! # Export
//!
//! Because a record's own event carries the stored line, a host that stores
//! `record` values stores the logs. [`verify_export`] proves it over a run
//! dir: the exported records equal the stored ones as JSON values, reload
//! through the log readers, and replay to the stored logs (a complete log
//! exactly, a crash prefix as a prefix). The standalone host runs it at the
//! end of every run under `verify_replay`.
//!
//! # Secrets
//!
//! Records are masked by the driver before they are appended, so every value
//! here is post-mask: a secret reference stays `{"$secret": ...}` and a
//! masked value stays `***`.

use std::time::Duration;

use driver::lifecycle::{BudgetNote, Note};
use driver::{BranchRef, BranchRole};
use engine::{Event, EventOrigin};
use ir::{
    Attempt, EdgeTransition, FiringId, Generation, Metrics, NodeId, Outcome, Status, StepEvent,
    Token, Value,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use steps::{Answer, Question, QuestionExpired};

use crate::hooks::HookActivity;
use crate::{
    CoordinatorEvent, CoordinatorRecord, ExecutionId, InvocationId, ParentCallKey,
    StoredEngineRecord,
};

pub mod export;
mod projection;
mod projector;
mod replay;

pub use export::{ExportError, verify_export, verify_export_run_dir};
pub use projection::Projection;
pub use projector::{
    CollectingSink, DEFAULT_QUEUE_CAPACITY, DEFAULT_STALL_TIMEOUT, EventProjector,
    ProjectionReceipt, ProjectorOptions, RunEventSink, SinkError,
};
pub use replay::{
    ReplayError, RunReplay, replay_execution, replay_run, replay_run_dir, replay_since,
};

/// The version of this contract. Bump when an existing field changes meaning
/// or a variant is removed; adding a variant or an optional field does not.
///
/// Version 2 made the stream lossless for replay: every event carried the
/// `origin` of its record and the first event of a record carried everything
/// the record did, under presentation names, so a reverse mapping could
/// rebuild the records.
///
/// Version 3 is one vocabulary for records and events: a public event is
/// named after its record (`step.finished`, `routing.resolved`) and carries
/// the stored record unchanged under `record`, with what Petri derived
/// beside it under `derived`. The presentation names of version 2
/// (`attempt_finished`, `routes_resolved`, `output_line` and the rest) are
/// gone, and so is the reverse mapping: export reads `record`.
///
/// Version 4 adds the scope records: `scope.acquired` and `scope.failed` in
/// an execution's log (where a scope's environment runs, or why it could not
/// be acquired) and `scope.released` in the coordinator log (a lease's
/// sandbox released by retention). The records are additive to the stream;
/// the version moves with the engine log (v11) and run format (6) that
/// carry them, so a host reading version 3 streams cannot mistake a run
/// with no scope records for one that had none to record. Within version 4,
/// `run.started` gained the optional `forked_from` (run format 7): a forked
/// run's stream names its source before any copied record (`FORK.md`).
///
/// Version 5 changes the shape of a partial success inside a `step.finished`
/// record: `underlying` is an `UnderlyingFailure` (`{"failure": {...}}` or
/// `"timed_out"`) instead of a bare `FailureInfo`. The version moves with the
/// engine log (v12) and run format (8) that carry it.
///
/// Version 6 records required finalization (run format 9). `run.finished`
/// is the overall result; a root invocation's result is workflow execution.
pub const EVENT_CONTRACT_VERSION: u32 = 6;

/// Which durable log an event was derived from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "log", rename_all = "snake_case")]
pub enum EventSource {
    Coordinator,
    Execution { execution: ExecutionId },
}

/// Who appended the record an event derives from, copied from the record;
/// or that the event has no record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordOrigin {
    /// The host fed the record: every coordinator record, and an engine
    /// record the driver applied (an execution's start, an admission, a
    /// step's start, progress and result, a routing decision, an elapsed
    /// retry, a host's cancel, kill or control). Replay consumes these.
    #[default]
    External,
    /// The core produced the record while draining: a routed token, an
    /// applied route, a splice, a cascading cancel. Replay regenerates these
    /// from the external records.
    Core,
    /// A view event: derived from the state alone, with no record of its
    /// own. Never stored; replay recomputes it.
    Derived,
}

impl From<EventOrigin> for RecordOrigin {
    fn from(origin: EventOrigin) -> Self {
        match origin {
            EventOrigin::External => Self::External,
            EventOrigin::Core => Self::Core,
        }
    }
}

/// A stable identity for deduplication: the log, the record's `seq` in it,
/// and which of the events derived from that record this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventId {
    #[serde(flatten)]
    pub source: EventSource,
    /// The record's position in its log.
    pub seq:    u64,
    /// `0` is the record's own event; the view events attached to the record
    /// follow at `1` and up.
    pub index:  u32,
}

/// The firing that called a nested invocation: the driver's type, so the
/// hook context and the event context name a parent the same way.
pub use driver::ParentLink;

impl From<&ParentCallKey> for ParentLink {
    fn from(key: &ParentCallKey) -> Self {
        Self {
            execution: key.parent,
            firing:    key.firing,
            attempt:   key.attempt,
            slot:      key.slot.clone(),
        }
    }
}

/// Where an event sits in the run: its invocation and execution, and the
/// parent call for an event of a nested invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation: Option<InvocationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution:  Option<ExecutionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent:     Option<ParentLink>,
}

/// A node, with the frontend's metadata so a host can tell a logical stage
/// from a synthetic lowering node without knowing the frontend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRef {
    pub id:   NodeId,
    /// The instance name (`build`, or `build#2` for an expansion clone).
    pub name: SmolStr,
    /// The step kind the node runs.
    pub kind: SmolStr,
    /// `Node::meta` verbatim. A frontend marks a node it invented with
    /// `"synthetic": true` and names its role under `"kind"`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

/// What an event is about: one firing of one node, or the node alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subject {
    pub node:       NodeRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firing:     Option<FiringId>,
    /// Which firing of the node this is within its execution, 1-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visit:      Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt:    Option<Attempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<Generation>,
    pub branch:     BranchRole,
}

/// The stored line a record's own event carries: a coordinator log line or
/// an engine log line, by the same types that write the logs. The log it
/// came from is the event's [`EventId::source`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Record {
    Coordinator(CoordinatorRecord),
    Engine(StoredEngineRecord),
}

impl Record {
    /// The engine event, for an engine record.
    pub fn engine(&self) -> Option<&Event> {
        match self {
            Self::Engine(record) => Some(&record.body),
            Self::Coordinator(_) => None,
        }
    }

    /// The coordinator event, for a coordinator record.
    pub fn coordinator(&self) -> Option<&CoordinatorEvent> {
        match self {
            Self::Coordinator(record) => Some(&record.body),
            Self::Engine(_) => None,
        }
    }
}

/// One event of the public stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub id:          EventId,
    /// Copied from the record: who appended it. `derived` for a view event,
    /// which has no record.
    #[serde(default)]
    pub origin:      RecordOrigin,
    #[serde(default)]
    pub context:     Context,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject:     Option<Subject>,
    /// Milliseconds since the Unix epoch when the projector saw the record
    /// live. Absent on replay: a replayed event is not observed again, and
    /// replay time is never passed off as execution time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<u64>,
    /// Milliseconds since the Unix epoch when the record this event derives
    /// from was appended to its log: the driver's clock for an engine record,
    /// the coordinator store's for a coordinator record, read at the append
    /// and persisted beside the record. The same live and on replay; the
    /// time an event happened, as opposed to when it was seen. On a record's
    /// own event it repeats `record.recorded_at`; a view event carries the
    /// time of the record it is attached to.
    pub recorded_at: u64,
    /// The stored line, unchanged, on a record's own event (`index` 0).
    /// Absent on a view event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record:      Option<Record>,
    /// What Petri derived beside the record, or the view event itself.
    /// Absent when a record's event derives nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived:     Option<Derived>,
}

impl RunEvent {
    /// The engine event of an engine record's own event.
    pub fn engine(&self) -> Option<&Event> {
        self.record.as_ref().and_then(Record::engine)
    }

    /// The coordinator event of a coordinator record's own event.
    pub fn coordinator(&self) -> Option<&CoordinatorEvent> {
        self.record.as_ref().and_then(Record::coordinator)
    }

    /// The view event, for an event that is one.
    pub fn view(&self) -> Option<&ViewEvent> {
        match &self.derived {
            Some(Derived::View(view)) => Some(view),
            _ => None,
        }
    }

    /// Petri's reading of a step protocol it owns, on a
    /// `step.progress.recorded` or `run.note.recorded` event.
    pub fn parsed(&self) -> Option<&Parsed> {
        match &self.derived {
            Some(Derived::Parsed { parsed }) => Some(parsed),
            _ => None,
        }
    }

    /// The step-defined payload of a `step.progress.recorded` event, as
    /// recorded: a backend's own event, a step's report, a Petri protocol.
    pub fn custom(&self) -> Option<&Value> {
        match self.engine() {
            Some(Event::StepProgressRecorded {
                ev: StepEvent::Custom(value),
                ..
            }) => Some(value),
            _ => None,
        }
    }

    /// The note a `step.progress.recorded` or `run.note.recorded` event
    /// carries, when it is one.
    pub fn note(&self) -> Option<&Note> {
        match self.parsed() {
            Some(Parsed::Note { note, .. }) => Some(note),
            _ => None,
        }
    }
}

/// What Petri derived beside a record, or the view event an event is. Each
/// record kind that derives anything has its own shape; the view events
/// are tagged by `event`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Derived {
    /// A view event, on an event with no record.
    View(ViewEvent),
    /// `step.finished`: `final` is whether the engine recorded the attempt
    /// as the firing's outcome (a non-final attempt is followed by a
    /// retry); `exhausted` is whether the retry policy allowed no further
    /// attempt while the status was retryable.
    StepFinished {
        #[serde(rename = "final")]
        is_final:  bool,
        exhausted: bool,
    },
    /// `routing.resolved`: the node each group's decision resolved to.
    RoutingResolved { groups: Vec<GroupTarget> },
    /// `route.applied`: the node the edge leads to, the edge's transition
    /// and whether it is a `back` edge. Absent for a route that applied
    /// nothing.
    RouteApplied {
        target:     NodeRef,
        transition: EdgeTransition,
        back:       bool,
    },
    /// `node.expanded`: each clone's entry node.
    NodeExpanded { clones: Vec<CloneRef> },
    /// `control.requested`: `deliverable` is whether the firing could
    /// receive the control (a late answer is recorded but not deliverable);
    /// `answer` is the decoding of a delivered value that reads as one.
    ControlRequested {
        deliverable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer:      Option<Answer>,
    },
    /// `step.progress.recorded` and `run.note.recorded`: Petri's reading of
    /// a step protocol it owns. Absent when the payload is a log line, an
    /// artifact, or a payload Petri does not own (a backend's own event is
    /// forwarded as recorded, `kind` naming the backend).
    Parsed { parsed: Parsed },
}

/// One routing group's resolved target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupTarget {
    pub group:  u32,
    /// The node an `emit` decision leads to. Absent for `none` and
    /// `block`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<NodeRef>,
}

/// One expansion clone's entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloneRef {
    pub index: u32,
    pub entry: NodeRef,
}

/// Petri's reading of a step protocol it owns, from a `$question`,
/// `$question_expired` or `$note` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Parsed {
    /// The step asked the host a question.
    Question { question: Question },
    /// The step's own answer deadline passed with no answer, as the step
    /// reported it. The attempt's outcome follows as `step.finished`;
    /// expiry is never inferred from that outcome.
    QuestionExpired { expired: QuestionExpired },
    /// A host extension recorded a fact (`driver::lifecycle::Note`). Kinds
    /// the driver writes: `result_prepared`, `transition`, `budget_paused`,
    /// `budget_resumed`. Kinds the hook adapter writes: `hook` (a hook
    /// service report) and `hook.activity`. Two kinds get a reading beside
    /// the note: `hook_activity` for a hook's own agent event, `budget` for
    /// an attempt budget's pause or resume.
    Note {
        note:          Note,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hook_activity: Option<HookActivity>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        budget:        Option<BudgetReading>,
    },
}

/// An attempt budget's pause or resume: an executor-enforced budget stopped
/// counting because the attempt asked a question, or counts again because
/// its last pending question was answered. `remaining_ms` is the active-work
/// time left.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetReading {
    pub state: BudgetState,
    #[serde(flatten)]
    pub note:  BudgetNote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetState {
    Paused,
    Resumed,
}

/// Where a firing stands between records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitState {
    /// The host is deciding whether the attempt may start.
    AwaitingAdmission,
    Running,
    /// A question is out and no answer has arrived.
    AwaitingAnswer,
    /// Between attempts, waiting out the backoff.
    AwaitingRetry,
    /// Told to stop; the outcome is on its way.
    Cancelling,
}

/// One branch's result: the record of the last node that ran on the branch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BranchResult {
    pub branch:  BranchRef,
    /// The last node on the branch: the one whose token reached the join, or
    /// the branch's last record when the fork was cancelled or killed.
    pub node:    NodeRef,
    pub firing:  FiringId,
    pub status:  Status,
    /// The token payload the branch handed to the join. Absent when the join
    /// never fired (a cancelled or killed fork).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub payload: Value,
}

/// One occurrence of a fork: the firing of the fork node that opened it, in
/// its execution. Every event about the fork's branches carries it, so a
/// host keys a repeated visit of one fork, a fork inside a branch, or two
/// branches with the same target on this reference and never on the most
/// recent fork it saw.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkOccurrence {
    pub execution:  ExecutionId,
    pub fork:       NodeId,
    pub firing:     FiringId,
    /// Which firing of the fork node this is within the execution, 1-based.
    pub visit:      u32,
    pub generation: Generation,
}

/// How a fork's branches were closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkDisposition {
    /// Every branch's token reached the join and the join fired.
    #[default]
    Joined,
    /// The join completed without running: its scope was cancelled, or every
    /// branch reached it cancelled. The results are the branches' last
    /// records.
    Cancelled,
    /// The fork's scope was killed and the join never fired. The results are
    /// the branches' last records; a branch that never recorded one is
    /// absent.
    Killed,
}

/// The view events: the projection's incremental reading of the state,
/// never stored. Each is attached to the record whose apply produced it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum ViewEvent {
    /// A node's join was satisfied and a firing exists, awaiting admission.
    #[serde(rename = "visit.started")]
    VisitStarted { inputs: Vec<Token> },
    /// A firing's final record exists: the node completed this visit.
    /// `executed` is false for a completion the engine synthesized (a false
    /// precondition, a cancelled scope, a blocked admission).
    #[serde(rename = "visit.completed")]
    VisitCompleted {
        outcome:  Outcome,
        executed: bool,
        attempts: u32,
    },
    /// A non-final attempt returned and the next one is waiting out the
    /// backoff.
    #[serde(rename = "retry.scheduled")]
    RetryScheduled {
        next_attempt: Attempt,
        base_delay:   Duration,
    },
    /// A firing's wait state changed.
    #[serde(rename = "wait.state.changed")]
    WaitStateChanged { state: WaitState },
    /// The routes of a fork node applied: its branches are starting.
    #[serde(rename = "fork.started")]
    ForkStarted {
        occurrence: ForkOccurrence,
        branches:   Vec<BranchRef>,
    },
    /// A branch reached its end: its final token reached the join, or the
    /// fork was cancelled or killed and this is the branch's last record.
    #[serde(rename = "branch.completed")]
    BranchCompleted {
        occurrence: ForkOccurrence,
        result:     BranchResult,
    },
    /// The fork's branches are all accounted for, in branch order:
    /// `disposition` says whether the join fired or the fork was stopped.
    #[serde(rename = "fork.completed")]
    ForkCompleted {
        occurrence:  ForkOccurrence,
        fork:        NodeRef,
        results:     Vec<BranchResult>,
        #[serde(default)]
        disposition: ForkDisposition,
    },
    /// The stall watchdog cancelled the run: no execution activity for the
    /// budget. Attached to the `invocation.cancel.requested` record that
    /// carries the reason.
    #[serde(rename = "run.stalled")]
    RunStalled {
        stall_timeout_ms: u64,
        idle_ms:          u64,
    },
}

/// Metrics helpers a projection consumer commonly wants.
pub fn duration_of(metrics: &Metrics) -> Option<Duration> {
    metrics.duration_ms.map(Duration::from_millis)
}
