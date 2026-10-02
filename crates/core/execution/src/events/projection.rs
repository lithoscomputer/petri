//! The derivation: one [`Projection`] per run turns each record and the
//! post-apply state into the record's own event and the view events
//! attached to it.

use std::collections::{BTreeMap, BTreeSet};

use driver::lifecycle::{BUDGET_PAUSED_KIND, BUDGET_RESUMED_KIND, BudgetNote, Note};
use driver::{BranchMap, BranchRef, BranchRole};
use engine::{
    CancelTarget, DecisionId, EngineState, Event, EventRecord, GroupDecision, RouteApplied,
    RouteDecision, SubgraphSplice,
};
use ir::placeholder::is_placeholder_item;
use ir::{
    Attempt, Control, FiringId, Generation, NodeId, Outcome, Status, StepEvent, Token, Value,
};
use smol_str::SmolStr;
use steps::{ANSWER_KEY, Answer, Question, QuestionExpired};

use super::{
    BranchResult, BudgetReading, BudgetState, CloneRef, Context, Derived, EventId, EventSource,
    ForkDisposition, ForkOccurrence, GroupTarget, NodeRef, ParentLink, Parsed, Record,
    RecordOrigin, RunEvent, Subject, ViewEvent, WaitState,
};
use crate::hooks::{HOOK_ACTIVITY_NOTE_KIND, HookActivity};
use crate::{
    CancelReason, CoordinatorEvent, CoordinatorRecord, ExecutionId, InvocationId,
    StoredEngineRecord,
};

/// Per-execution bookkeeping the derivation needs beyond the engine state.
#[derive(Default)]
struct ExecutionTrack {
    invocation: Option<InvocationId>,
    parent:     Option<ParentLink>,
    /// Firings seen live, so a new one is a visit start.
    firings:    BTreeSet<FiringId>,
    /// Firings whose attempt was dispatched at least once.
    started:    BTreeSet<FiringId>,
    /// Firings with a question out.
    asking:     BTreeSet<FiringId>,
    history:    usize,
    branches:   BranchMap,
    /// Fork firings whose `fork.started` was emitted.
    announced:  BTreeSet<FiringId>,
    /// Forks whose `fork.completed` is still to come, by the fork's firing.
    open:       BTreeMap<FiringId, OpenFork>,
}

/// A fork between its `fork.started` and its `fork.completed`. The
/// generation ties the branches and the join to this occurrence of the
/// fork: the engine fires one `(node, generation)` at most once, and the
/// tokens a fork routes to its branches and on to the join keep the fork
/// firing's generation.
struct OpenFork {
    occurrence: ForkOccurrence,
    branches:   Vec<BranchRef>,
}

impl OpenFork {
    fn covers(&self, fork: NodeId, generation: Generation) -> bool {
        self.occurrence.fork == fork && self.occurrence.generation == generation
    }
}

/// The view events one record produced, in order, each with the subject it
/// is about, before they are given ids.
#[derive(Default)]
struct Views(Vec<(Option<Subject>, ViewEvent)>);

impl Views {
    fn push(&mut self, subject: Option<Subject>, view: ViewEvent) {
        self.0.push((subject, view));
    }
}

/// What every event derived from one record shares: the log and `seq` that
/// identify the record, who appended it, where it sits in the run, and when
/// it was appended.
struct Envelope {
    source:      EventSource,
    seq:         u64,
    origin:      RecordOrigin,
    context:     Context,
    recorded_at: u64,
}

impl Envelope {
    /// The record's own event at index `0`, then its view events at `1` and
    /// up.
    fn events(
        self,
        subject: Option<Subject>,
        record: Record,
        derived: Option<Derived>,
        views: Views,
    ) -> Vec<RunEvent> {
        let id = |index: u32| EventId {
            source: self.source,
            seq: self.seq,
            index,
        };
        let mut out = Vec::with_capacity(1 + views.0.len());
        out.push(RunEvent {
            id: id(0),
            origin: self.origin,
            context: self.context.clone(),
            subject,
            observed_at: None,
            recorded_at: self.recorded_at,
            record: Some(record),
            derived,
        });
        out.extend(
            views
                .0
                .into_iter()
                .enumerate()
                .map(|(index, (subject, view))| RunEvent {
                    id: id(u32::try_from(index + 1).unwrap_or(u32::MAX)),
                    origin: RecordOrigin::Derived,
                    context: self.context.clone(),
                    subject,
                    observed_at: None,
                    recorded_at: self.recorded_at,
                    record: None,
                    derived: Some(Derived::View(view)),
                }),
        );
        out
    }
}

/// The stateless-by-record derivation, with the little state it needs across
/// records. One per run; fed both logs.
///
/// The fold is pure: it reads records and the post-apply engine state, does
/// no I/O, and hands back owned events. A record's own event carries the
/// record unchanged under `record`:
///
/// ```
/// use engine::{EngineState, Event, EventOrigin, EventRecord};
/// use execution::ExecutionId;
/// use execution::events::{Projection, Record};
/// use ir::{CancelScopeId, Graph};
///
/// let record = EventRecord {
///     seq:    0,
///     origin: EventOrigin::External,
///     event:  Event::cancel_scope(CancelScopeId::ROOT),
/// };
/// let state = EngineState::new(Graph::new());
/// let events = Projection::new().engine(ExecutionId::new(0), &record, 1_000, &state);
/// let Some(Record::Engine(stored)) = &events[0].record else {
///     panic!("a record's own event carries the record");
/// };
/// assert_eq!(stored.seq, 0);
/// assert_eq!(stored.recorded_at, 1_000);
/// assert_eq!(stored.body, record.event);
/// ```
#[derive(Default)]
pub struct Projection {
    executions:  BTreeMap<ExecutionId, ExecutionTrack>,
    invocations: BTreeMap<InvocationId, Option<ParentLink>>,
}

impl Projection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Derive the events of one coordinator record: the record's own event,
    /// then any view event attached to it.
    pub fn lifecycle(&mut self, record: &CoordinatorRecord) -> Vec<RunEvent> {
        let mut views = Views::default();
        let (invocation, execution, derived) = match &record.body {
            CoordinatorEvent::RunStarted { root, .. } => (Some(*root), None, None),
            CoordinatorEvent::GraphRegistered { .. }
            | CoordinatorEvent::RunPaused
            | CoordinatorEvent::RunUnpaused
            | CoordinatorEvent::RunFinished { .. } => (None, None, None),
            CoordinatorEvent::InvocationDeclared {
                invocation, call, ..
            } => {
                let link = call.as_ref().map(ParentLink::from);
                self.invocations.insert(*invocation, link);
                (Some(*invocation), None, None)
            }
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                ..
            } => {
                let track = self.executions.entry(*execution).or_default();
                track.invocation = Some(*invocation);
                track.parent = self.invocations.get(invocation).cloned().flatten();
                (Some(*invocation), Some(*execution), None)
            }
            CoordinatorEvent::ExecutionFinished { execution, .. } => (
                self.executions
                    .get(execution)
                    .and_then(|track| track.invocation),
                Some(*execution),
                None,
            ),
            CoordinatorEvent::InvocationFinished { invocation, result } => {
                (Some(*invocation), Some(result.final_execution), None)
            }
            // The lease's owner; the execution that acquired it is the
            // `scope.acquired` record's.
            CoordinatorEvent::ScopeReleased { invocation, .. } => (Some(*invocation), None, None),
            CoordinatorEvent::InvocationCancelRequested { invocation, reason } => {
                if let Some(CancelReason::StallTimeout {
                    stall_timeout_ms,
                    idle_ms,
                }) = reason
                {
                    views.push(None, ViewEvent::RunStalled {
                        stall_timeout_ms: *stall_timeout_ms,
                        idle_ms:          *idle_ms,
                    });
                }
                (Some(*invocation), None, None)
            }
            // A run-level note reads the way a firing's note does, with no
            // subject, since no firing owns it.
            CoordinatorEvent::RunNoteRecorded {
                execution,
                kind,
                payload,
            } => (
                None,
                *execution,
                Some(Derived::Parsed {
                    parsed: note_parsed(Note::new(kind.clone(), payload.clone())),
                }),
            ),
        };
        let context = Context {
            invocation,
            execution,
            parent: invocation
                .and_then(|invocation| self.invocations.get(&invocation).cloned())
                .flatten(),
        };
        Envelope {
            source: EventSource::Coordinator,
            seq: record.seq,
            origin: record.origin.into(),
            context,
            recorded_at: record.recorded_at,
        }
        .events(None, Record::Coordinator(record.clone()), derived, views)
    }

    /// Derive the events of one engine record, given its recording time
    /// (when the record reached its log) and the post-apply state: the
    /// record's own event, then the view events attached to it.
    pub fn engine(
        &mut self,
        execution: ExecutionId,
        record: &EventRecord,
        recorded_at: u64,
        state: &EngineState,
    ) -> Vec<RunEvent> {
        let track = self.executions.entry(execution).or_default();
        track.refresh_branches(state);
        let mut views = Views::default();
        let (subject, derived) = track.derive(execution, record, state, &mut views);
        // State-derived facts every record may carry: new firings (visits
        // starting), firings told to stop, new final records (visits
        // completing), and killed forks with no branch left running. The
        // order is the order of the view events.
        track.start_new_visits(execution, state, &mut views);
        track.mark_cancelling(record, state, &mut views);
        track.complete_visits(state, &mut views);
        track.close_killed_forks(state, &mut views);
        Envelope {
            source: EventSource::Execution { execution },
            seq: record.seq,
            origin: record.origin.into(),
            context: track.context(execution),
            recorded_at,
        }
        .events(
            subject,
            Record::Engine(StoredEngineRecord::new(record, recorded_at)),
            derived,
            views,
        )
    }
}

impl ExecutionTrack {
    /// Rebuild the branch map once the graph outgrew it (a splice added
    /// clones).
    fn refresh_branches(&mut self, state: &EngineState) {
        if !self.branches.covers(state.graph()) {
            self.branches = BranchMap::of(state.graph()).with_expansions(state);
        }
    }

    fn context(&self, execution: ExecutionId) -> Context {
        Context {
            invocation: self.invocation,
            execution:  Some(execution),
            parent:     self.parent.clone(),
        }
    }

    /// The record's own derivation: its subject and what it derived, with
    /// the view events its apply produced.
    fn derive(
        &mut self,
        execution: ExecutionId,
        record: &EventRecord,
        state: &EngineState,
        views: &mut Views,
    ) -> (Option<Subject>, Option<Derived>) {
        match &record.event {
            // A scope is not a node: its records carry their scope and have
            // no subject.
            Event::ExecutionStarted { .. }
            | Event::KillRequested { .. }
            | Event::ScopeAcquired { .. }
            | Event::ScopeFailed { .. }
            | Event::CancelRequested {
                target: CancelTarget::Scope(_),
            } => (None, None),
            Event::CancelRequested {
                target: CancelTarget::Group(node),
            } => (self.node_subject(state, *node), None),
            Event::TokenEmitted { token } => (self.subject_of(state, token.from), None),
            Event::StepStarted { firing, .. } => self.on_step_started(state, *firing, views),
            Event::StepProgressRecorded { firing, ev } => {
                self.on_step_progress(state, *firing, ev, views)
            }
            Event::StepFinished {
                firing,
                attempt,
                outcome,
            } => self.on_step_finished(state, *firing, *attempt, outcome, views),
            Event::AdmissionDecided { decision_id, .. } => match decision_id {
                DecisionId::AttemptStart { firing, .. } => (self.subject_of(state, *firing), None),
                DecisionId::ExecutionStart | DecisionId::Route { .. } => (None, None),
            },
            Event::RoutingResolved {
                decision_id,
                groups,
            } => match decision_id {
                DecisionId::Route { firing, .. } => (
                    self.subject_of(state, *firing),
                    Some(Derived::RoutingResolved {
                        groups: groups
                            .iter()
                            .map(|group| group_target(state, group))
                            .collect(),
                    }),
                ),
                DecisionId::ExecutionStart | DecisionId::AttemptStart { .. } => (None, None),
            },
            Event::RouteApplied { applied } => {
                self.on_route_applied(execution, state, applied, views)
            }
            Event::RetryElapsed { firing, .. } => (self.subject_of(state, *firing), None),
            Event::NodeExpanded { node, splice } => {
                self.on_node_expanded(execution, state, *node, splice, views)
            }
            Event::ControlRequested { firing, ctl } => {
                self.on_control_requested(state, *firing, ctl, views)
            }
        }
    }

    /// The attempt was dispatched: the firing runs.
    fn on_step_started(
        &mut self,
        state: &EngineState,
        firing: FiringId,
        views: &mut Views,
    ) -> (Option<Subject>, Option<Derived>) {
        self.started.insert(firing);
        let subject = self.subject_of(state, firing);
        views.push(subject.clone(), ViewEvent::WaitStateChanged {
            state: WaitState::Running,
        });
        (subject, None)
    }

    /// A progress payload: a question puts the firing on hold, and the
    /// step's own expiry of it lifts the hold.
    fn on_step_progress(
        &mut self,
        state: &EngineState,
        firing: FiringId,
        ev: &StepEvent,
        views: &mut Views,
    ) -> (Option<Subject>, Option<Derived>) {
        let subject = self.subject_of(state, firing);
        let parsed = parse_progress(ev);
        match &parsed {
            Some(Parsed::Question { .. }) => {
                self.asking.insert(firing);
                views.push(subject.clone(), ViewEvent::WaitStateChanged {
                    state: WaitState::AwaitingAnswer,
                });
            }
            Some(Parsed::QuestionExpired { .. }) => {
                // The step ended its own wait: the question is no longer
                // out, as after an answer.
                if self.asking.remove(&firing) {
                    views.push(subject.clone(), ViewEvent::WaitStateChanged {
                        state: WaitState::Running,
                    });
                }
            }
            Some(Parsed::Note { .. }) | None => {}
        }
        (subject, parsed.map(|parsed| Derived::Parsed { parsed }))
    }

    /// An attempt returned: final, or followed by a retry.
    fn on_step_finished(
        &mut self,
        state: &EngineState,
        firing: FiringId,
        attempt: Attempt,
        outcome: &Outcome,
        views: &mut Views,
    ) -> (Option<Subject>, Option<Derived>) {
        let is_final = state
            .history()
            .iter()
            .rev()
            .any(|entry| entry.firing == firing && entry.attempt == attempt);
        let node = state
            .firing_node(firing)
            .and_then(|id| state.graph().node(id));
        let exhausted = node.is_some_and(|node| {
            node.retry.should_retry(&outcome.status) && !node.retry.has_attempt_after(attempt)
        });
        self.asking.remove(&firing);
        let subject = self.subject_of(state, firing);
        if !is_final && let Some(node) = node {
            views.push(subject.clone(), ViewEvent::RetryScheduled {
                next_attempt: attempt.next(),
                base_delay:   node.retry.base_delay(attempt),
            });
            views.push(subject.clone(), ViewEvent::WaitStateChanged {
                state: WaitState::AwaitingRetry,
            });
        }
        (
            subject,
            Some(Derived::StepFinished {
                is_final,
                exhausted,
            }),
        )
    }

    /// A route applied: the first edge of a fork's routing announces the
    /// fork.
    fn on_route_applied(
        &mut self,
        execution: ExecutionId,
        state: &EngineState,
        applied: &RouteApplied,
        views: &mut Views,
    ) -> (Option<Subject>, Option<Derived>) {
        let firing = applied.firing();
        let subject = self.subject_of(state, firing);
        if let (Some(subject), RouteApplied::Edge { .. }) = (&subject, applied)
            && let BranchRole::Fork { branches } = subject.branch
        {
            // The fork's routing applies group by group; the first applied
            // route announces the fork once.
            let fork = subject.node.id;
            let branches = (0..branches)
                .map(|index| BranchRef { fork, index })
                .collect();
            self.announce_fork(execution, state, firing, branches, views);
        }
        (subject, applied_target(state, applied))
    }

    /// A template expanded: an expansion is a fork whose clones are the
    /// branches, in item order, and the node that fanned out into the
    /// template (the branch map's fork for it) announces them once.
    fn on_node_expanded(
        &mut self,
        execution: ExecutionId,
        state: &EngineState,
        node: NodeId,
        splice: &SubgraphSplice,
        views: &mut Views,
    ) -> (Option<Subject>, Option<Derived>) {
        let derived = Derived::NodeExpanded {
            clones: splice
                .clones
                .iter()
                .map(|clone| CloneRef {
                    index: clone.index,
                    entry: clone
                        .nodes
                        .iter()
                        .find(|n| n.id == clone.entry)
                        .map_or_else(
                            || NodeRef {
                                id:   clone.entry,
                                name: SmolStr::new(""),
                                kind: SmolStr::new(""),
                                meta: Value::Null,
                            },
                            node_ref,
                        ),
                })
                .collect(),
        };
        // The records of one engine turn are derived against the state after
        // the whole turn, so the fork's own `route.applied` may already see
        // its role and announce it; the guard is shared, so whichever record
        // comes first announces and the other stays quiet. The join derives
        // `branch.completed` and `fork.completed` from the same roles the
        // static path uses.
        if let Some(fork) = self.branches.expansion_fork(node) {
            // The fork's own firing: the record of the fork node in the
            // expansion's generation, which the template's token carried
            // from it.
            let firing = state
                .history()
                .iter()
                .rev()
                .find(|record| record.node == fork && record.generation == splice.generation)
                .map(|record| record.firing);
            // The placeholder clone an empty list expands to is no branch:
            // the fork starts and closes with none.
            let mut branches: Vec<BranchRef> = splice
                .clones
                .iter()
                .filter(|clone| !is_placeholder_item(&clone.item))
                .map(|clone| BranchRef {
                    fork,
                    index: clone.index,
                })
                .collect();
            branches.sort_by_key(|branch| branch.index);
            if let Some(firing) = firing {
                self.announce_fork(execution, state, firing, branches, views);
            }
        }
        (self.node_subject(state, node), Some(derived))
    }

    /// A control reached the firing: an answer to its question lifts the
    /// hold.
    fn on_control_requested(
        &mut self,
        state: &EngineState,
        firing: FiringId,
        ctl: &Control,
        views: &mut Views,
    ) -> (Option<Subject>, Option<Derived>) {
        let deliverable = state
            .firing(firing)
            .is_some_and(|f| !f.cancelling && !f.awaiting_retry)
            && !state.is_awaiting_admission(firing);
        let answer = match ctl {
            Control::Deliver(value) if value.get(ANSWER_KEY).is_some() => Answer::from_value(value),
            _ => None,
        };
        let answered = answer.is_some() && deliverable && self.asking.remove(&firing);
        let subject = self.subject_of(state, firing);
        if answered {
            views.push(subject.clone(), ViewEvent::WaitStateChanged {
                state: WaitState::Running,
            });
        }
        (
            subject,
            Some(Derived::ControlRequested {
                deliverable,
                answer,
            }),
        )
    }

    /// Announce the fork `firing` opened, once: remember it as open and emit
    /// `fork.started`. The fork's first applied route and the expansion of
    /// its template can each announce it; the guard is shared.
    fn announce_fork(
        &mut self,
        execution: ExecutionId,
        state: &EngineState,
        firing: FiringId,
        branches: Vec<BranchRef>,
        views: &mut Views,
    ) {
        if !self.announced.insert(firing) {
            return;
        }
        let Some(subject) = self.subject_of(state, firing) else {
            return;
        };
        let Some(occurrence) = occurrence_of(execution, &subject) else {
            return;
        };
        self.open_fork(occurrence.clone(), &branches);
        views.push(Some(subject), ViewEvent::ForkStarted {
            occurrence,
            branches,
        });
    }

    /// Visits that started: every firing seen for the first time. A join's
    /// first sight closes the fork it joins.
    fn start_new_visits(&mut self, execution: ExecutionId, state: &EngineState, views: &mut Views) {
        let live: Vec<FiringId> = state.live_firings().map(|f| f.id).collect();
        for firing in live {
            if !self.firings.insert(firing) {
                continue;
            }
            let inputs = state
                .firing(firing)
                .map(|f| f.inputs.clone())
                .unwrap_or_default();
            let subject = self.subject_of(state, firing);
            if let Some(subject) = &subject
                && let BranchRole::Join { fork } = subject.branch
            {
                // The join fired: every branch is in, and its inputs are
                // the branches' final tokens.
                let results = self.branch_results(state, fork, &inputs);
                let occurrence = self
                    .take_open(fork, subject.generation)
                    .map(|open| open.occurrence)
                    .or_else(|| {
                        self.recover_occurrence(state, execution, fork, subject.generation)
                    });
                if let Some(occurrence) = occurrence {
                    self.close_fork(
                        state,
                        subject,
                        occurrence,
                        results,
                        ForkDisposition::Joined,
                        views,
                    );
                }
            }
            views.push(subject.clone(), ViewEvent::VisitStarted { inputs });
            views.push(subject, ViewEvent::WaitStateChanged {
                state: WaitState::AwaitingAdmission,
            });
        }
    }

    /// Firings a cancel or a kill told to stop.
    fn mark_cancelling(&self, record: &EventRecord, state: &EngineState, views: &mut Views) {
        if !matches!(
            &record.event,
            Event::CancelRequested { .. } | Event::KillRequested { .. }
        ) {
            return;
        }
        let cancelling: Vec<FiringId> = state
            .live_firings()
            .filter(|f| f.cancelling)
            .map(|f| f.id)
            .collect();
        for firing in cancelling {
            views.push(
                self.subject_of(state, firing),
                ViewEvent::WaitStateChanged {
                    state: WaitState::Cancelling,
                },
            );
        }
    }

    /// Visits that completed: the history entries this record added. A join
    /// that completed without ever being live was synthesized (the fork's
    /// scope was cancelled, or every branch reached it cancelled); its
    /// record closes the fork from the branches' own final records.
    fn complete_visits(&mut self, state: &EngineState, views: &mut Views) {
        let history = state.history();
        if history.len() <= self.history {
            return;
        }
        for entry in &history[self.history..] {
            let executed = self.started.contains(&entry.firing);
            let attempts = entry.attempt.raw();
            let subject = state.graph().node(entry.node).map(|node| {
                self.subject(
                    state,
                    node,
                    Some((entry.firing, entry.attempt, entry.generation)),
                )
            });
            if let Some(subject) = &subject
                && let BranchRole::Join { fork } = subject.branch
                && !self.firings.contains(&entry.firing)
                && let Some(open) = self.take_open(fork, Some(entry.generation))
            {
                let results = self.member_results(state, &open);
                let disposition = if matches!(entry.outcome.status, Status::Cancelled) {
                    ForkDisposition::Cancelled
                } else {
                    ForkDisposition::Joined
                };
                self.close_fork(state, subject, open.occurrence, results, disposition, views);
            }
            views.push(subject, ViewEvent::VisitCompleted {
                outcome: entry.outcome.clone(),
                executed,
                attempts,
            });
        }
        self.history = history.len();
    }

    /// A killed fork's join never fires: its tokens were dropped. Once no
    /// branch of the fork has a live firing left, the fork is closed from
    /// the branches' final records.
    fn close_killed_forks(&mut self, state: &EngineState, views: &mut Views) {
        let killed: Vec<FiringId> = self
            .open
            .iter()
            .filter(|(_, open)| {
                state.is_node_killed(open.occurrence.fork)
                    && !state.live_firings().any(|firing| {
                        firing.generation == open.occurrence.generation
                            && matches!(
                                self.branches.role(firing.node),
                                BranchRole::Member(branch) if branch.fork == open.occurrence.fork
                            )
                    })
            })
            .map(|(firing, _)| *firing)
            .collect();
        for firing in killed {
            let Some(open) = self.open.remove(&firing) else {
                continue;
            };
            let Some(subject) = self.subject_of(state, firing) else {
                continue;
            };
            let results = self.member_results(state, &open);
            self.close_fork(
                state,
                &subject,
                open.occurrence,
                results,
                ForkDisposition::Killed,
                views,
            );
        }
    }

    /// Remember an announced fork until its join closes it.
    fn open_fork(&mut self, occurrence: ForkOccurrence, branches: &[BranchRef]) {
        self.open.insert(occurrence.firing, OpenFork {
            occurrence,
            branches: branches.to_vec(),
        });
    }

    /// Take the open fork a join of `fork` in `generation` closes, if any.
    fn take_open(&mut self, fork: NodeId, generation: Option<Generation>) -> Option<OpenFork> {
        let generation = generation?;
        let firing = self
            .open
            .iter()
            .find(|(_, open)| open.covers(fork, generation))
            .map(|(firing, _)| *firing)?;
        self.open.remove(&firing)
    }

    /// Emit `branch.completed` per result, each on its branch's last firing,
    /// then `fork.completed` on `subject` (the join's firing when there is
    /// one, else the fork's own).
    fn close_fork(
        &self,
        state: &EngineState,
        subject: &Subject,
        occurrence: ForkOccurrence,
        results: Vec<BranchResult>,
        disposition: ForkDisposition,
        views: &mut Views,
    ) {
        for result in &results {
            views.push(
                self.subject_of(state, result.firing),
                ViewEvent::BranchCompleted {
                    occurrence: occurrence.clone(),
                    result:     result.clone(),
                },
            );
        }
        if let Some(fork_node) = state.graph().node(occurrence.fork).map(node_ref) {
            views.push(Some(subject.clone()), ViewEvent::ForkCompleted {
                occurrence,
                fork: fork_node,
                results,
                disposition,
            });
        }
    }

    /// The subject for `node`: one of its firings when there is one, else
    /// the node alone.
    fn subject(
        &self,
        state: &EngineState,
        node: &ir::Node,
        firing: Option<(FiringId, Attempt, Generation)>,
    ) -> Subject {
        let (firing, attempt, generation) = match firing {
            Some((firing, attempt, generation)) => (Some(firing), Some(attempt), Some(generation)),
            None => (None, None, None),
        };
        Subject {
            node: node_ref(node),
            firing,
            visit: Some(state.firing_count(node.id)),
            attempt,
            generation,
            branch: self.branches.role(node.id),
        }
    }

    fn node_subject(&self, state: &EngineState, node: NodeId) -> Option<Subject> {
        state
            .graph()
            .node(node)
            .map(|node| self.subject(state, node, None))
    }

    /// The subject for a firing, live or retired.
    fn subject_of(&self, state: &EngineState, firing: FiringId) -> Option<Subject> {
        let (node, attempt, generation) = if let Some(live) = state.firing(firing) {
            (live.node, live.attempt, live.generation)
        } else {
            let record = state.history().iter().rev().find(|r| r.firing == firing)?;
            (record.node, record.attempt, record.generation)
        };
        let node = state.graph().node(node)?;
        Some(self.subject(state, node, Some((firing, attempt, generation))))
    }

    /// The branch results a join's inputs carry, in branch order.
    fn branch_results(
        &self,
        state: &EngineState,
        fork: NodeId,
        inputs: &[Token],
    ) -> Vec<BranchResult> {
        let mut results: Vec<BranchResult> = inputs
            .iter()
            .filter_map(|token| {
                let record = state
                    .history()
                    .iter()
                    .rev()
                    .find(|r| r.firing == token.from)?;
                let node = state.graph().node(record.node)?;
                let branch = match self.branches.role(record.node) {
                    BranchRole::Member(branch) if branch.fork == fork => branch,
                    BranchRole::Fork { .. } if record.node == fork => BranchRef { fork, index: 0 },
                    _ => return None,
                };
                Some(BranchResult {
                    branch,
                    node: node_ref(node),
                    firing: token.from,
                    status: record.outcome.status.clone(),
                    payload: token.payload.clone(),
                })
            })
            .collect();
        results.sort_by_key(|result| result.branch.index);
        results
    }

    /// The occurrence of `fork` in `generation` from the fork's own record,
    /// for a join whose fork this projection never saw announced (it
    /// attached after the fork fired without being primed).
    fn recover_occurrence(
        &self,
        state: &EngineState,
        execution: ExecutionId,
        fork: NodeId,
        generation: Option<Generation>,
    ) -> Option<ForkOccurrence> {
        let record = state.history().iter().rev().find(|record| {
            record.node == fork
                && generation.is_none_or(|generation| record.generation == generation)
        })?;
        let subject = self.subject_of(state, record.firing)?;
        occurrence_of(execution, &subject)
    }

    /// The branches' final records for a fork that was cancelled or killed:
    /// the latest record of a member of each branch in the fork's
    /// generation, in branch order. A branch with no record yet is absent.
    /// The token payloads are gone with the join that never ran, so none is
    /// carried.
    fn member_results(&self, state: &EngineState, open: &OpenFork) -> Vec<BranchResult> {
        let mut results: BTreeMap<u32, BranchResult> = BTreeMap::new();
        for record in state.history().iter().rev() {
            if record.generation != open.occurrence.generation {
                continue;
            }
            let BranchRole::Member(branch) = self.branches.role(record.node) else {
                continue;
            };
            if !open.branches.contains(&branch) || results.contains_key(&branch.index) {
                continue;
            }
            let Some(node) = state.graph().node(record.node) else {
                continue;
            };
            results.insert(branch.index, BranchResult {
                branch,
                node: node_ref(node),
                firing: record.firing,
                status: record.outcome.status.clone(),
                payload: Value::Null,
            });
        }
        results.into_values().collect()
    }
}

fn node_ref(node: &ir::Node) -> NodeRef {
    NodeRef {
        id:   node.id,
        name: node.name.clone(),
        kind: SmolStr::new(node.step.kind.as_str()),
        meta: node.meta.clone(),
    }
}

/// The node one routing group's decision leads to.
fn group_target(state: &EngineState, group: &GroupDecision) -> GroupTarget {
    let target = match &group.decision {
        RouteDecision::Emit(edge) => state
            .graph()
            .edge(*edge)
            .and_then(|edge| state.graph().node(edge.to))
            .map(node_ref),
        RouteDecision::None | RouteDecision::Block { .. } => None,
    };
    GroupTarget {
        group: group.group,
        target,
    }
}

/// What an applied route derives: the node its edge leads to, the edge's
/// transition and `back`. Nothing for a route that applied nothing or one
/// whose target the graph no longer names.
fn applied_target(state: &EngineState, applied: &RouteApplied) -> Option<Derived> {
    match applied {
        RouteApplied::Edge { edge, .. } => {
            let arm = state.graph().edge(*edge)?;
            let target = state.graph().node(arm.to).map(node_ref)?;
            Some(Derived::RouteApplied {
                target,
                transition: arm.transition,
                back: arm.back,
            })
        }
        RouteApplied::None { .. } => None,
    }
}

/// The occurrence a fork firing's subject names.
fn occurrence_of(execution: ExecutionId, subject: &Subject) -> Option<ForkOccurrence> {
    Some(ForkOccurrence {
        execution,
        fork: subject.node.id,
        firing: subject.firing?,
        visit: subject.visit?,
        generation: subject.generation?,
    })
}

/// Petri's reading of a progress payload, when it is a protocol Petri owns.
fn parse_progress(ev: &StepEvent) -> Option<Parsed> {
    if let Some(question) = Question::from_event(ev) {
        return Some(Parsed::Question { question });
    }
    if let Some(expired) = QuestionExpired::from_event(ev) {
        return Some(Parsed::QuestionExpired { expired });
    }
    Note::from_step_event(ev).map(note_parsed)
}

/// A note, with the reading two kinds get beside it: a hook's own agent
/// event, and an attempt budget's pause or resume. A note whose payload
/// does not read as its kind says stays a bare note.
fn note_parsed(note: Note) -> Parsed {
    let budget = |state: BudgetState| {
        serde_json::from_value::<BudgetNote>(note.payload.clone())
            .ok()
            .map(|note| BudgetReading { state, note })
    };
    let (hook_activity, budget) = match note.kind.as_str() {
        HOOK_ACTIVITY_NOTE_KIND => (
            serde_json::from_value::<HookActivity>(note.payload.clone()).ok(),
            None,
        ),
        BUDGET_PAUSED_KIND => (None, budget(BudgetState::Paused)),
        BUDGET_RESUMED_KIND => (None, budget(BudgetState::Resumed)),
        _ => (None, None),
    };
    Parsed::Note {
        note,
        hook_activity,
        budget,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn budget_notes_and_hook_activity_read_beside_the_note_and_other_notes_stay_bare() {
        let paused = Note::new(
            BUDGET_PAUSED_KIND,
            json!({ "attempt": 1, "remaining_ms": 4000, "pending_questions": 1 }),
        );
        let Parsed::Note { budget, .. } = note_parsed(paused.clone()) else {
            panic!("a note");
        };
        assert_eq!(
            budget,
            Some(BudgetReading {
                state: BudgetState::Paused,
                note:  BudgetNote {
                    attempt:           Attempt::new(1),
                    remaining_ms:      4000,
                    pending_questions: 1,
                },
            })
        );
        let resumed = Note::new(
            BUDGET_RESUMED_KIND,
            json!({ "attempt": 1, "remaining_ms": 4000, "pending_questions": 0 }),
        );
        let Parsed::Note { budget, .. } = note_parsed(resumed) else {
            panic!("a note");
        };
        assert_eq!(
            budget.map(|budget| budget.state),
            Some(BudgetState::Resumed)
        );
        let hook = Note::new("hook", json!({ "point": "before_attempt" }));
        assert_eq!(note_parsed(hook.clone()), Parsed::Note {
            note:          hook,
            hook_activity: None,
            budget:        None,
        });
        let malformed = Note::new(BUDGET_PAUSED_KIND, json!("not a budget"));
        assert!(matches!(note_parsed(malformed), Parsed::Note {
            budget: None,
            ..
        }));
    }

    /// The step protocols Petri owns parse; a backend's own event, a log
    /// line and an artifact do not: they are forwarded as recorded.
    #[test]
    fn only_petri_s_own_protocols_parse() {
        let backend = StepEvent::Custom(json!({
            "kind": "pebble",
            "event": { "session_id": "ses_1", "seq": 3, "event": { "TurnStarted": {} } },
        }));
        assert!(parse_progress(&backend).is_none());
        assert!(
            parse_progress(&StepEvent::Log {
                stream: ir::LogStream::Stdout,
                line:   "x".into(),
            })
            .is_none()
        );
        let note = StepEvent::Custom(json!({ "$note": { "kind": "transition", "payload": 1 } }));
        assert!(matches!(parse_progress(&note), Some(Parsed::Note { .. })));
    }

    /// A record's own event carries the stored line verbatim, the derived
    /// values apart from it, and the envelope repeats the record's identity.
    #[test]
    fn a_record_event_carries_its_line_and_derived_values_apart() {
        let record = CoordinatorRecord::external(3, 1_789, CoordinatorEvent::RunNoteRecorded {
            execution: Some(ExecutionId::new(1)),
            kind:      "hook".into(),
            payload:   json!({ "point": "run_finished" }),
        });
        let events = Projection::new().lifecycle(&record);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.origin, RecordOrigin::External);
        assert_eq!(event.recorded_at, 1_789);
        assert_eq!(event.context.execution, Some(ExecutionId::new(1)));
        let json = serde_json::to_value(event).expect("encodes");
        assert_eq!(
            json["id"],
            json!({ "log": "coordinator", "seq": 3, "index": 0 })
        );
        assert_eq!(
            json["record"],
            serde_json::to_value(&record).expect("encodes"),
            "the record is the stored line"
        );
        assert_eq!(json["record"]["body"]["event"], json!("run.note.recorded"));
        assert_eq!(json["derived"]["parsed"]["kind"], json!("note"));
        let back: RunEvent = serde_json::from_value(json).expect("decodes");
        assert_eq!(&back, event);
    }
}
