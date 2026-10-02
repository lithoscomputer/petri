//! Random flows: the generator the property tests and the Lean model check
//! share.
//!
//! A case is a small graph of `noop` steps plus the actions a host takes. A
//! forward arm from node `i` goes to a node after it; a back arm goes to `i`
//! or a node before it, and starts the next generation, so every cycle holds
//! a back edge (§8 invariant 1). Each node has a firing budget, and the host's
//! outcome for a node can change from one firing to the next, so a loop runs a
//! few times and then either exits or hits its budget. Each node also has a
//! retry policy, and the host scripts every attempt: success, failure, a
//! failure of class `flaky`, or a timeout. Each routing group picks its first
//! arm whose guard passes: `always`, `success()`, `failure()` or
//! `cancelled()` over the node's own outcome. Joins are `All`, `Any` or
//! `Quorum { n }`.
//!
//! About half the cases also stop work (§5). Nodes may belong to one of two
//! declared cancellation groups and may be marked `run_on_cancel`, and the
//! host cancels or kills the root or a group between its other steps. It
//! answers a stop signal as each node says: it honors it and reports
//! `Cancelled`, or it ignores it and reports the scripted outcome, like a step
//! that finished before the signal reached it. The host may also run a single
//! attempt and leave the firing waiting on its retry backoff, so a stop can
//! catch it there.
//!
//! Some generated graphs break a load-time rule on purpose: an `All` join over
//! two arms of one routing group, a `Quorum { n }` fed by fewer than `n`
//! groups (§8 invariant 10), or a loop head that does not join with `Any`
//! (invariant 8). The tests run them anyway.
//!
//! The case is also the wire format of the Lean model check: its JSON is
//! what `lean/PetriModel/Wire.lean` reads, and [`Observed`] is what the model
//! answers.

#![allow(
    dead_code,
    reason = "each test binary compiles the whole module, and no one test uses every helper"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use engine::{Command, EngineState, Event, RunError};
use ir::{
    Arm, Attempt, Backoff, Budget, CancelScopeId, Control, EdgeId, FailureInfo, FiringId, Graph,
    GraphBuilder, JoinPolicy, NodeId, Outcome, RetryOn, RetryPolicy, RunStatus, ScopeId, Status,
    UnderlyingFailure, Value,
};
use proptest::prelude::*;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

use crate::support::{Harness, NOOP};

const MAX_NODES: usize = 7;
const MAX_FIRINGS: u32 = 4;
const MAX_ATTEMPTS: u32 = 3;
const MAX_STOPS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum JoinSpec {
    All,
    Any,
    Quorum { n: u32 },
}

impl JoinSpec {
    fn policy(self) -> JoinPolicy {
        match self {
            Self::All => JoinPolicy::All,
            Self::Any => JoinPolicy::Any,
            Self::Quorum { n } => JoinPolicy::Quorum { n },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GuardSpec {
    Always,
    Success,
    Failure,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ArmSpec {
    pub to:    u32,
    pub guard: GuardSpec,
    /// A back arm: its token starts the next generation.
    pub back:  bool,
    /// The id the builder gives this arm's edge: arms are numbered from 0 in
    /// node, group and arm order.
    pub edge:  u32,
}

/// What the host reports for one attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutcomeSpec {
    Success,
    Failure,
    /// A failure of class `flaky`.
    Flaky,
    TimedOut,
    /// What the host reports when it honors a stop signal. Never scripted.
    Cancelled,
}

impl OutcomeSpec {
    pub(crate) fn outcome(self) -> Outcome {
        match self {
            Self::Success => Outcome::success(Value::Null),
            Self::Failure => Outcome::failure("scripted failure"),
            Self::Flaky => Outcome::new(
                Status::Failure(FailureInfo::new("scripted flake").with_class("flaky")),
                Value::Null,
            ),
            Self::TimedOut => Outcome::new(Status::TimedOut, Value::Null),
            Self::Cancelled => Outcome::cancelled(),
        }
    }
}

/// Which outcomes a node retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetryOnSpec {
    /// `RetryOn::default()`: the `Failure` and `TimedOut` statuses.
    Default,
    /// Only failures of class `flaky`.
    Flaky,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RetrySpec {
    pub max_attempts:   u32,
    pub retry_on:       RetryOnSpec,
    pub accept_partial: bool,
    pub initial_nanos:  u64,
    /// `Backoff.factor` as its `f64` bit pattern: JSON cannot carry NaN or the
    /// infinities.
    pub factor_bits:    u64,
    pub max_nanos:      u64,
}

impl RetrySpec {
    /// Whether this policy retries an outcome, restated from §3.1 and §4
    /// rather than read from `RetryPolicy::should_retry`, so a test that
    /// relies on it checks the core instead of agreeing with it: a success or
    /// a cancellation is never retried, the default retries a failure or a
    /// timeout, and the `flaky` policy retries a failure of that class only.
    pub(crate) fn retries(&self, outcome: OutcomeSpec) -> bool {
        match (self.retry_on, outcome) {
            (_, OutcomeSpec::Success | OutcomeSpec::Cancelled) => false,
            (RetryOnSpec::Default, _) => true,
            (RetryOnSpec::Flaky, outcome) => outcome == OutcomeSpec::Flaky,
        }
    }

    /// The status a firing records when its last attempt reports `outcome`
    /// after `attempts` attempts: an exhausted retryable failure becomes a
    /// partial success under `AcceptPartial`, keeping its failure, a timeout
    /// included (§3.1 rule 3). Restated from the spec, like [`Self::retries`].
    pub(crate) fn recorded(&self, attempts: u32, outcome: OutcomeSpec) -> Status {
        let status = outcome.outcome().status;
        if self.accept_partial && self.retries(outcome) && attempts >= self.max_attempts {
            Status::PartialSuccess {
                underlying: UnderlyingFailure::of(&status),
            }
        } else {
            status
        }
    }

    pub(crate) fn policy(&self) -> RetryPolicy {
        let retry_on = match self.retry_on {
            RetryOnSpec::Default => RetryOn::default(),
            RetryOnSpec::Flaky => RetryOn {
                statuses:        Vec::new(),
                failure_classes: vec!["flaky".into()],
            },
        };
        let policy = RetryPolicy::attempts(self.max_attempts)
            .with_retry_on(retry_on)
            .with_backoff(Backoff {
                initial: Duration::from_nanos(self.initial_nanos),
                factor:  f64::from_bits(self.factor_bits),
                max:     Duration::from_nanos(self.max_nanos),
                jitter:  false,
            });
        if self.accept_partial {
            policy.accepting_partial()
        } else {
            policy
        }
    }
}

/// How the host answers a stop signal to one of a node's firings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopAnswer {
    /// Report `Cancelled`.
    Honor,
    /// Report the scripted outcome, as a step that finished before the
    /// signal reached it.
    Ignore,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct NodeSpec {
    pub join:          JoinSpec,
    /// `Budget.max_firings`.
    pub max_firings:   u32,
    pub retry:         RetrySpec,
    /// What the host reports for each attempt of the node's first, second, …
    /// started firing. The last firing's list repeats, and within a list the
    /// last attempt repeats.
    pub outcomes:      Vec<Vec<OutcomeSpec>>,
    pub groups:        Vec<Vec<ArmSpec>>,
    /// The anchor of the node's cancellation group; an anchor names itself.
    pub group:         Option<u32>,
    pub run_on_cancel: bool,
    pub on_stop:       StopAnswer,
}

impl NodeSpec {
    /// What the host reports for attempt `attempt` (from 1) of firing
    /// `ordinal` (from 0).
    pub(crate) fn outcome(&self, ordinal: usize, attempt: u32) -> OutcomeSpec {
        let attempts = &self.outcomes[ordinal.min(self.outcomes.len() - 1)];
        attempts[(attempt.max(1) as usize - 1).min(attempts.len() - 1)]
    }

    /// The last attempt a firing's script reaches under this node's policy:
    /// the first outcome it does not retry, or the attempt limit.
    pub(crate) fn final_outcome(&self, ordinal: usize) -> OutcomeSpec {
        let mut attempt = 1;
        loop {
            let outcome = self.outcome(ordinal, attempt);
            if !self.retry.retries(outcome) || attempt >= self.retry.max_attempts {
                return outcome;
            }
            attempt += 1;
        }
    }
}

/// The cancellation scope a stop names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Target {
    Root,
    /// A declared cancellation group, by its anchor.
    Group(u32),
}

/// One host step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Finish the live firing at position `choice % live` in ascending
    /// `(node, generation)` order, running every attempt its script reaches.
    Finish(u32),
    /// Run only the chosen firing's next attempt. A retry leaves it waiting
    /// on its backoff until a later step picks it again.
    Attempt(u32),
    Cancel(Target),
    Kill(Target),
}

impl Action {
    fn map_entry<S: Serializer, T: Serialize>(
        serializer: S,
        key: &str,
        value: &T,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(key, value)?;
        map.end()
    }
}

/// A `finish` is its bare choice on the wire, as every host step was before
/// the host could do anything else; the others are one-key objects.
impl Serialize for Action {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Finish(choice) => serializer.serialize_u32(*choice),
            Self::Attempt(choice) => Self::map_entry(serializer, "attempt", choice),
            Self::Cancel(target) => Self::map_entry(serializer, "cancel", target),
            Self::Kill(target) => Self::map_entry(serializer, "kill", target),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct FlowCase {
    pub nodes:    Vec<NodeSpec>,
    /// The host's steps. Once they run out, the host finishes the first live
    /// firing until nothing runs.
    pub schedule: Vec<Action>,
}

impl FlowCase {
    /// Whether every step finishes a firing: no stop, and no single attempt.
    pub(crate) fn only_finishes(&self) -> bool {
        self.schedule
            .iter()
            .all(|action| matches!(action, Action::Finish(_)))
    }

    /// The same case with every firing cut to the last attempt its script
    /// reaches, and one attempt allowed. Retries are invisible outside the
    /// log (§4), so both run the same way when the host only finishes
    /// firings.
    pub(crate) fn finalized(&self) -> Self {
        let nodes = self
            .nodes
            .iter()
            .map(|node| NodeSpec {
                retry: RetrySpec {
                    max_attempts: 1,
                    ..node.retry.clone()
                },
                outcomes: (0..node.outcomes.len())
                    .map(|ordinal| vec![node.final_outcome(ordinal)])
                    .collect(),
                ..node.clone()
            })
            .collect();
        Self {
            nodes,
            schedule: self.schedule.clone(),
        }
    }

    /// Whether a stop of `target` reaches `node`: the root covers every
    /// node, a group its members.
    pub(crate) fn covers(&self, target: Target, node: u32) -> bool {
        match target {
            Target::Root => true,
            Target::Group(anchor) => self.nodes[node as usize].group == Some(anchor),
        }
    }
}

/// What a run looks like from outside the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Observed {
    /// `(node, generation)` firings started by the seed, then by each host
    /// step, each list sorted.
    pub steps:           Vec<Vec<(u32, u32)>>,
    /// `(node, generation)` firings in the order the host finished them, or a
    /// stop settled them.
    pub finished:        Vec<(u32, u32)>,
    /// `(node, generation, edge)` tokens still waiting at a join when the run
    /// ended.
    pub parked:          Vec<(u32, u32, u32)>,
    /// Nodes whose budget refused a firing, in the order it happened.
    pub budget_exceeded: Vec<u32>,
    /// `success`, `failed` or `cancelled`; `unsettled` when the run did not
    /// finish.
    pub status:          String,
    /// `(node, generation, attempts, status)` per finished firing, in the
    /// order the host finished them; the status is the record's tag.
    #[serde(default)]
    pub attempts:        Vec<(u32, u32, u32, String)>,
    /// `(node, generation, next attempt, base delay in nanoseconds)` per
    /// scheduled retry, in order.
    #[serde(default)]
    pub retries:         Vec<(u32, u32, u32, u64)>,
    /// `(node, generation, cancel | kill)` per stop signal the core sent a
    /// firing, in order.
    #[serde(default)]
    pub controls:        Vec<(u32, u32, String)>,
    /// `(node, generation)` keys that completed `Cancelled` without running,
    /// in the order they recorded.
    #[serde(default)]
    pub completed:       Vec<(u32, u32)>,
}

impl Observed {
    /// What routing and the run context see: everything but the attempt
    /// counts and the retries.
    pub(crate) fn routing(&self) -> Self {
        Self {
            attempts: self
                .attempts
                .iter()
                .map(|(node, generation, _, status)| (*node, *generation, 1, status.clone()))
                .collect(),
            retries: Vec::new(),
            ..self.clone()
        }
    }
}

/// One `StartStep` the core issued.
pub(crate) struct Start {
    pub firing:     FiringId,
    pub node:       NodeId,
    pub generation: u32,
    /// Which of the node's started firings this is, from 0.
    pub ordinal:    usize,
    pub attempt:    u32,
    /// What the host reports for this attempt: the scripted outcome, or
    /// `Cancelled` when it honors a stop signal.
    pub outcome:    OutcomeSpec,
    pub inputs:     Vec<EdgeId>,
    /// The firing each input token came from.
    pub sources:    Vec<FiringId>,
    /// The host step that started it, as an index into `Observed::steps`.
    pub step:       usize,
    /// The host step that reported it.
    pub reported:   Option<usize>,
}

/// A stop signal's tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Cancel,
    Kill,
}

impl Stop {
    fn tag(self) -> &'static str {
        match self {
            Self::Cancel => "cancel",
            Self::Kill => "kill",
        }
    }
}

/// A stop the host sent, with what it saw just before.
pub(crate) struct StopAt {
    /// The host step, as an index into `Observed::steps`.
    pub step:      usize,
    pub stop:      Stop,
    pub target:    Target,
    pub live:      BTreeSet<FiringId>,
    /// The live firings waiting on a retry backoff.
    pub waiting:   BTreeSet<FiringId>,
    /// The firings an earlier stop already signalled.
    pub signalled: BTreeSet<FiringId>,
}

/// A stop signal the core sent a firing.
pub(crate) struct ControlAt {
    pub step:   usize,
    pub firing: FiringId,
    pub stop:   Stop,
}

/// A finished run: what was observed, plus what the host saw and the harness
/// for invariant checks.
pub(crate) struct Run {
    pub observed: Observed,
    pub starts:   Vec<Start>,
    pub stops:    Vec<StopAt>,
    pub controls: Vec<ControlAt>,
    /// Firings a stop settled while they waited on a retry backoff, with the
    /// stop's step.
    pub settled:  BTreeMap<FiringId, usize>,
    /// Each budget error's node, with the host step that raised it.
    pub refusals: Vec<(usize, NodeId)>,
    pub harness:  Harness,
}

// ── Generation ────────────────────────────────────────────────────────────

/// A raw arm: a guard choice, a target offset, a back-arm roll and a
/// `cancelled()` roll, normalized by position.
type RawArm = (u8, u32, u8, bool);
/// A raw arm with its target resolved: the guard roll, the target, whether
/// it is a back arm, and whether it routes on `cancelled()`.
type PlacedArm = (u8, u32, bool, bool);
/// A raw node's stop settings: a group roll, `run_on_cancel`, and whether the
/// host honors a stop signal.
type RawStopping = (u8, bool, bool);
/// A raw node: its join, whether a loop head keeps that join (and breaks
/// invariant 8), its budget, its outcomes, its groups and its stop settings.
type RawNode = (
    JoinSpec,
    bool,
    u32,
    RetrySpec,
    Vec<Vec<OutcomeSpec>>,
    Vec<Vec<RawArm>>,
    RawStopping,
);
/// A raw host step: a choice, and whether it runs a single attempt.
type RawStep = (u32, bool);
/// A raw stop: where it goes in the schedule, which tier (a cancel, a kill,
/// or a cancel the host escalates to a kill), a target roll, and the choice
/// of a single attempt the host runs just before it, if any.
type RawStop = (u32, u8, u32, Option<u32>);

fn join_spec() -> impl Strategy<Value = JoinSpec> {
    prop_oneof![
        2 => Just(JoinSpec::All),
        1 => Just(JoinSpec::Any),
        1 => (0u32..=3).prop_map(|n| JoinSpec::Quorum { n }),
    ]
}

fn outcome_spec() -> impl Strategy<Value = OutcomeSpec> {
    prop_oneof![
        5 => Just(OutcomeSpec::Success),
        3 => Just(OutcomeSpec::Failure),
        2 => Just(OutcomeSpec::Flaky),
        1 => Just(OutcomeSpec::TimedOut),
    ]
}

/// Retry policies, with backoffs that exercise the delay arithmetic: a zero,
/// a huge value, a factor that is NaN, negative or infinite.
fn retry_spec() -> impl Strategy<Value = RetrySpec> {
    let nanos = || prop_oneof![0u64..=5_000_000_000, any::<u64>()];
    let factor = prop_oneof![
        prop::sample::select(vec![1.0, 1.5, 2.0, 0.0, -1.0, f64::NAN, f64::INFINITY]),
        0.0f64..10.0,
        any::<f64>(),
    ];
    (
        1..=MAX_ATTEMPTS,
        prop::bool::weighted(0.7),
        prop::bool::weighted(0.3),
        nanos(),
        factor,
        nanos(),
    )
        .prop_map(
            |(max_attempts, default_on, accept_partial, initial_nanos, factor, max_nanos)| {
                RetrySpec {
                    max_attempts,
                    retry_on: if default_on {
                        RetryOnSpec::Default
                    } else {
                        RetryOnSpec::Flaky
                    },
                    accept_partial,
                    initial_nanos,
                    factor_bits: factor.to_bits(),
                    max_nanos,
                }
            },
        )
}

fn raw_node() -> impl Strategy<Value = RawNode> {
    let arm = || (0u8..10, any::<u32>(), 0u8..10, prop::bool::weighted(0.15));
    // Mostly one arm per group: an arm the group does not choose never
    // delivers, and leaves an `All` join downstream waiting forever.
    let group = prop_oneof![
        3 => prop::collection::vec(arm(), 1..=1),
        1 => prop::collection::vec(arm(), 2..=2),
    ];
    (
        join_spec(),
        prop::bool::weighted(0.1),
        1..=MAX_FIRINGS,
        retry_spec(),
        prop::collection::vec(prop::collection::vec(outcome_spec(), 1..=3), 1..=3),
        prop::collection::vec(group, 1..=3),
        (
            0u8..10,
            prop::bool::weighted(0.5),
            prop::bool::weighted(0.6),
        ),
    )
}

/// A case, with stops about half the time.
pub(crate) fn flow_case() -> impl Strategy<Value = FlowCase> {
    prop_oneof![flow_case_without_stops(), flow_case_with_stops()]
}

/// A case whose host only finishes firings: no groups, no `run_on_cancel`,
/// no `cancelled()` guard, no stop and no single attempt.
pub(crate) fn flow_case_without_stops() -> impl Strategy<Value = FlowCase> {
    (2..=MAX_NODES)
        .prop_flat_map(|n| {
            (
                prop::collection::vec(raw_node(), n),
                prop::collection::vec(0u32..8, 0..=2 * n),
            )
        })
        .prop_map(|(raw, choices)| {
            let steps: Vec<RawStep> = choices.into_iter().map(|choice| (choice, false)).collect();
            FlowCase::from_raw(&raw, &steps, &[], false)
        })
}

/// A case with one to three stops, and a host that sometimes runs a single
/// attempt.
fn flow_case_with_stops() -> impl Strategy<Value = FlowCase> {
    (2..=MAX_NODES)
        .prop_flat_map(|n| {
            (
                prop::collection::vec(raw_node(), n),
                prop::collection::vec((0u32..8, prop::bool::weighted(0.25)), 0..=2 * n),
                prop::collection::vec(
                    (
                        any::<u32>(),
                        0u8..10,
                        any::<u32>(),
                        prop::option::weighted(0.7, 0u32..8),
                    ),
                    1..=MAX_STOPS,
                ),
            )
        })
        .prop_map(|(raw, steps, stops)| FlowCase::from_raw(&raw, &steps, &stops, true))
}

/// `always()` only ends a group (§8 invariant 2). Forward arms are mostly
/// unconditional, so joins see many tokens; back arms are mostly guarded, so
/// a loop exits on an outcome more often than on its budget. A case with
/// stops also routes on `cancelled()`.
fn guard_spec(roll: u8, last: bool, back: bool, cancelled: bool) -> GuardSpec {
    match (roll, last, back) {
        _ if cancelled => GuardSpec::Cancelled,
        (0..=6, true, false) | (0..=1, true, true) => GuardSpec::Always,
        (0..=7, _, false) | (0..=5, _, true) => GuardSpec::Success,
        _ => GuardSpec::Failure,
    }
}

impl FlowCase {
    fn from_raw(raw: &[RawNode], steps: &[RawStep], stops: &[RawStop], stopping: bool) -> Self {
        let count = u32::try_from(raw.len()).expect("a case holds at most MAX_NODES nodes");
        // Targets first: an arm with nowhere to go is dropped, and guards
        // depend on an arm's final position in its group.
        let targets: Vec<Vec<Vec<PlacedArm>>> = raw
            .iter()
            .zip(0..)
            .map(|((.., groups, _), index)| {
                let later = count - index - 1;
                groups
                    .iter()
                    .map(|arms| {
                        arms.iter()
                            .filter_map(|&(guard, offset, back_roll, cancelled)| {
                                let cancelled = stopping && cancelled;
                                // About one arm in five loops back; the last node,
                                // which has no forward target, loops back less often
                                // and otherwise ends the flow.
                                if back_roll < 2 || (later == 0 && back_roll < 4) {
                                    Some((guard, offset % (index + 1), true, cancelled))
                                } else if later > 0 {
                                    Some((guard, index + 1 + offset % later, false, cancelled))
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                    .filter(|arms| !arms.is_empty())
                    .collect()
            })
            .collect();
        let loop_heads: BTreeSet<u32> = targets
            .iter()
            .flatten()
            .flatten()
            .filter(|(_, _, back, _)| *back)
            .map(|(_, to, ..)| *to)
            .collect();

        // Two cancellation groups at most, each anchored at its first member.
        // Members are mostly early nodes, which an early stop finds live, and
        // later nodes mostly stay outside, where cancelled work flows on.
        let group_of: Vec<Option<usize>> = raw
            .iter()
            .zip(0u8..)
            .map(|((.., (roll, _, _)), index)| {
                (stopping && *roll < 6u8.saturating_sub(index).max(1))
                    .then_some(usize::from(roll % 2))
            })
            .collect();
        let anchor_of =
            |group: usize| (0..count).find(|index| group_of[*index as usize] == Some(group));
        let anchors: Vec<u32> = (0..2).filter_map(anchor_of).collect();

        let mut next_edge = 0;
        let nodes = raw
            .iter()
            .zip(targets)
            .zip(0..)
            .map(|((raw, groups), index)| {
                let (join, keep_join, max_firings, retry, outcomes, _, stopping_roll) = raw;
                let (_, run_on_cancel, honor) = *stopping_roll;
                let groups = groups
                    .into_iter()
                    .map(|arms| {
                        let last = arms.len() - 1;
                        arms.into_iter()
                            .enumerate()
                            .map(|(position, (guard, to, back, cancelled))| {
                                let edge = next_edge;
                                next_edge += 1;
                                ArmSpec {
                                    to,
                                    guard: guard_spec(guard, position == last, back, cancelled),
                                    back,
                                    edge,
                                }
                            })
                            .collect()
                    })
                    .collect();
                // A loop head joins with `Any` (invariant 8), except for the few
                // that keep their join so validation has something to reject.
                let join = if loop_heads.contains(&index) && !keep_join {
                    JoinSpec::Any
                } else {
                    *join
                };
                NodeSpec {
                    join,
                    max_firings: *max_firings,
                    retry: retry.clone(),
                    outcomes: outcomes.clone(),
                    groups,
                    group: group_of[index as usize].and_then(anchor_of),
                    run_on_cancel: stopping && run_on_cancel,
                    on_stop: if honor {
                        StopAnswer::Honor
                    } else {
                        StopAnswer::Ignore
                    },
                }
            })
            .collect();

        let mut schedule: Vec<Action> = steps
            .iter()
            .map(|&(choice, attempt)| {
                if attempt {
                    Action::Attempt(choice)
                } else {
                    Action::Finish(choice)
                }
            })
            .collect();
        // Stops land early in the run, while most of its work is still live.
        // A single attempt just before a stop may leave a firing waiting on
        // its backoff for the stop to settle. A cancel the host escalates to a
        // kill a few steps later, as the driver does when its cleanup grace
        // runs out (§10), lets some cancelled work route on and reaches the
        // rest still cancelling.
        for &(position, tier, roll, attempt) in stops {
            let target = match roll as usize % (anchors.len() + 1) {
                0 => Target::Root,
                group => Target::Group(anchors[group - 1]),
            };
            let at = (position as usize % (raw.len() / 2 + 2)).min(schedule.len());
            let mut actions: Vec<Action> = attempt.map(Action::Attempt).into_iter().collect();
            actions.push(if (5..=6).contains(&tier) {
                Action::Kill(target)
            } else {
                Action::Cancel(target)
            });
            let kill_at =
                (at + actions.len() + usize::from(tier % 3)).min(schedule.len() + actions.len());
            schedule.splice(at..at, actions);
            if tier >= 7 {
                schedule.insert(kill_at, Action::Kill(target));
            }
        }
        Self { nodes, schedule }
    }

    /// Nodes no forward arm reaches: the entries, seeded in node order. A loop
    /// head may be one (§8, "entry-node seeding checks consider forward edges
    /// only").
    pub(crate) fn entries(&self) -> Vec<u32> {
        let reached: BTreeSet<u32> = self
            .nodes
            .iter()
            .flat_map(|node| node.groups.iter().flatten())
            .filter(|arm| !arm.back)
            .map(|arm| arm.to)
            .collect();
        (0..)
            .take(self.nodes.len())
            .filter(|index| !reached.contains(index))
            .collect()
    }

    /// The graph this case describes, built the way a frontend would.
    pub(crate) fn graph(&self) -> Graph {
        let mut b = GraphBuilder::new();
        let scope = ScopeId::new(0);
        let ids: Vec<NodeId> = (0..self.nodes.len())
            .map(|i| b.add_step(&format!("n{i}"), scope, NOOP))
            .collect();
        let success = b.exprs().call("success", Vec::new());
        let failure = b.exprs().call("failure", Vec::new());
        let cancelled = b.exprs().call("cancelled", Vec::new());
        for (node, id) in self.nodes.iter().zip(&ids) {
            b.set_join(*id, node.join.policy());
            b.set_budget(*id, Budget::looped(node.max_firings));
            let built = b.node_mut(*id);
            built.retry = node.retry.policy();
            built.run_on_cancel = node.run_on_cancel;
            built.cancel_group = node.group.map(|anchor| ids[anchor as usize]);
            if node.groups.is_empty() {
                continue;
            }
            let groups = node
                .groups
                .iter()
                .map(|arms| {
                    arms.iter()
                        .map(|arm| {
                            let to = ids[arm.to as usize];
                            let built = match arm.guard {
                                GuardSpec::Always => Arm::always(to),
                                GuardSpec::Success => Arm::when(to, success),
                                GuardSpec::Failure => Arm::when(to, failure),
                                GuardSpec::Cancelled => Arm::when(to, cancelled),
                            };
                            if arm.back { built.with_back() } else { built }
                        })
                        .collect()
                })
                .collect();
            let built = b.fan_out_groups(*id, groups);
            let expected: Vec<Vec<EdgeId>> = node
                .groups
                .iter()
                .map(|arms| arms.iter().map(|arm| EdgeId::new(arm.edge)).collect())
                .collect();
            assert_eq!(built, expected, "the builder numbers arms in case order");
        }
        for entry in self.entries() {
            b.mark_entry(ids[entry as usize]);
        }
        b.build()
    }
}

// ── Running ───────────────────────────────────────────────────────────────

/// Run a case through the real core with a host that follows the schedule.
pub(crate) fn run(case: &FlowCase) -> Run {
    run_with(case, Harness::new(case.graph()))
}

/// [`run`], reporting every `apply` to `observer`: the new state and the
/// commands it produced.
pub(crate) fn run_observed(
    case: &FlowCase,
    observer: impl FnMut(&EngineState, &[Command]) + 'static,
) -> Run {
    run_with(case, Harness::new(case.graph()).observe(observer))
}

fn run_with(case: &FlowCase, harness: Harness) -> Run {
    let mut host = Host::new(case, harness);
    // Every step after the schedule finishes one firing, and a node fires at
    // most its budget, so a run that needs more steps has broken that rule;
    // stop and report it.
    let limit = case.schedule.len()
        + case
            .nodes
            .iter()
            .map(|node| node.max_firings as usize)
            .sum::<usize>();
    for index in 0..=limit {
        if host.live.is_empty() {
            break;
        }
        let action = case
            .schedule
            .get(index)
            .copied()
            .unwrap_or(Action::Finish(0));
        let step = host.steps.len();
        let errors = host.harness.state.errors().len();
        match action {
            Action::Finish(choice) => host.finish(step, choice, true),
            Action::Attempt(choice) => host.finish(step, choice, false),
            Action::Cancel(target) => host.stop(step, Stop::Cancel, target),
            Action::Kill(target) => host.stop(step, Stop::Kill, target),
        }
        host.take_controls(step);
        host.take_refusals(step, errors);
    }
    host.into_run()
}

/// The scripted host, and everything it saw.
struct Host<'a> {
    case:      &'a FlowCase,
    harness:   Harness,
    starts:    Vec<Start>,
    /// Live firings by key.
    live:      BTreeMap<(u32, u32), FiringId>,
    /// Live firings waiting on a retry backoff, with the attempt the driver
    /// feeds back next.
    waiting:   BTreeMap<FiringId, Attempt>,
    /// Firings the core sent a stop signal.
    signalled: BTreeSet<FiringId>,
    ordinals:  Ordinals,
    steps:     Vec<Vec<(u32, u32)>>,
    finished:  Vec<(u32, u32)>,
    attempts:  Vec<(u32, u32, u32, String)>,
    stops:     Vec<StopAt>,
    controls:  Vec<ControlAt>,
    settled:   BTreeMap<FiringId, usize>,
    refusals:  Vec<(usize, NodeId)>,
}

impl<'a> Host<'a> {
    fn new(case: &'a FlowCase, mut harness: Harness) -> Self {
        harness.feed(Event::ExecutionStarted {
            start: engine::EngineStart::default(),
        });
        let mut host = Self {
            case,
            harness,
            starts: Vec::new(),
            live: BTreeMap::new(),
            waiting: BTreeMap::new(),
            signalled: BTreeSet::new(),
            ordinals: Ordinals::default(),
            steps: Vec::new(),
            finished: Vec::new(),
            attempts: Vec::new(),
            stops: Vec::new(),
            controls: Vec::new(),
            settled: BTreeMap::new(),
            refusals: Vec::new(),
        };
        let seeded = host.drain_starts(0);
        host.started(seeded);
        host
    }

    /// Finish the chosen firing, or run its next attempt only. A firing
    /// waiting on its backoff first gets the `RetryElapsed` the driver's
    /// sleeper feeds.
    fn finish(&mut self, step: usize, choice: u32, every_attempt: bool) {
        let index = choice as usize % self.live.len();
        let (&key, &firing) = self
            .live
            .iter()
            .nth(index)
            .expect("the index is below live.len()");
        if let Some(next_attempt) = self.waiting.remove(&firing) {
            self.resume(step, firing, next_attempt);
        }
        loop {
            self.report(step, firing);
            let retry = self.take_retry(firing);
            let batch = self.drain_starts(step);
            let Some(next_attempt) = retry else {
                self.live.remove(&key);
                self.finished.push(key);
                self.record_attempts(key, firing);
                self.started(batch);
                return;
            };
            assert!(batch.is_empty(), "a non-final attempt routes nothing");
            if !every_attempt {
                self.waiting.insert(firing, next_attempt);
                self.steps.push(Vec::new());
                return;
            }
            self.resume(step, firing, next_attempt);
        }
    }

    /// Feed a firing's `RetryElapsed` and take the attempt it starts.
    fn resume(&mut self, step: usize, firing: FiringId, next_attempt: Attempt) {
        self.harness.feed(Event::RetryElapsed {
            firing,
            next_attempt,
        });
        let batch = self.drain_starts(step);
        assert!(
            batch.len() == 1 && batch[0].firing == firing,
            "a retry starts its own firing's next attempt and nothing else"
        );
        self.starts.extend(batch);
    }

    /// Report the firing's running attempt: the scripted outcome, or
    /// `Cancelled` when the host honors a stop signal.
    fn report(&mut self, step: usize, firing: FiringId) {
        let start = self
            .starts
            .iter_mut()
            .rev()
            .find(|start| start.firing == firing)
            .expect("a live firing has started");
        if self.signalled.contains(&firing)
            && self.case.nodes[start.node.index()].on_stop == StopAnswer::Honor
        {
            start.outcome = OutcomeSpec::Cancelled;
        }
        start.reported = Some(step);
        let outcome = start.outcome.outcome();
        self.harness.finish(firing, outcome);
    }

    /// Take the retry the core scheduled for this firing, if any.
    fn take_retry(&mut self, firing: FiringId) -> Option<Attempt> {
        let position = self.harness.commands.iter().position(|command| {
            matches!(command, Command::ScheduleRetry { firing: scheduled, .. } if *scheduled == firing)
        })?;
        match self.harness.commands.remove(position) {
            Command::ScheduleRetry {
                next_attempt,
                base_delay,
                ..
            } => {
                self.harness
                    .scheduled_retries
                    .push((firing, next_attempt, base_delay));
                Some(next_attempt)
            }
            _ => None,
        }
    }

    /// Cancel or kill a scope. A firing waiting on its backoff has no work in
    /// flight, so the stop settles it at once (§5); the driver's sleeper
    /// cannot be recalled, and its `RetryElapsed` still arrives, unless the
    /// settle ended the run: the driver stops feeding the core at the finish.
    fn stop(&mut self, step: usize, stop: Stop, target: Target) {
        self.stops.push(StopAt {
            step,
            stop,
            target,
            live: self.live.values().copied().collect(),
            waiting: self.waiting.keys().copied().collect(),
            signalled: self.signalled.clone(),
        });
        let event = match (stop, target) {
            (Stop::Cancel, Target::Root) => Event::cancel_scope(CancelScopeId::ROOT),
            (Stop::Cancel, Target::Group(anchor)) => Event::cancel_group(NodeId::new(anchor)),
            (Stop::Kill, target) => Event::KillRequested {
                scope: self.scope_of(target),
            },
        };
        self.harness.feed(event);
        let settled: Vec<((u32, u32), FiringId)> = self
            .live
            .iter()
            .filter(|(_, firing)| {
                self.waiting.contains_key(*firing) && self.harness.state.firing(**firing).is_none()
            })
            .map(|(key, firing)| (*key, *firing))
            .collect();
        for (key, firing) in settled {
            let next_attempt = self
                .waiting
                .remove(&firing)
                .expect("only waiting firings settle");
            self.live.remove(&key);
            self.finished.push(key);
            self.settled.insert(firing, step);
            self.record_attempts(key, firing);
            if self.harness.status.is_none() {
                self.harness.feed(Event::RetryElapsed {
                    firing,
                    next_attempt,
                });
            }
        }
        let batch = self.drain_starts(step);
        self.started(batch);
    }

    /// The cancel scope a kill names: the root, or the group's scope, a child
    /// of the root.
    fn scope_of(&self, target: Target) -> CancelScopeId {
        let Target::Group(anchor) = target else {
            return CancelScopeId::ROOT;
        };
        let state = &self.harness.state;
        state
            .cancel_scope(CancelScopeId::ROOT)
            .into_iter()
            .flat_map(|root| root.children.iter().copied())
            .find(|child| {
                state
                    .cancel_scope(*child)
                    .is_some_and(|scope| scope.nodes.contains(&NodeId::new(anchor)))
            })
            .expect("every group has its own scope under the root")
    }

    /// Take the stop signals the core sent.
    fn take_controls(&mut self, step: usize) {
        let mut delivered = Vec::new();
        self.harness.commands.retain(|command| match command {
            Command::DeliverControl { firing, ctl } => {
                delivered.push((*firing, ctl.clone()));
                false
            }
            _ => true,
        });
        for (firing, ctl) in delivered {
            let stop = match ctl {
                Control::Cancel => Stop::Cancel,
                Control::Kill => Stop::Kill,
                other => panic!("the core sent {other:?}, which no generated step asks for"),
            };
            self.controls.push(ControlAt { step, firing, stop });
            self.signalled.insert(firing);
        }
    }

    /// Note the budget errors a step raised, from the error at `from` on.
    fn take_refusals(&mut self, step: usize, from: usize) {
        for error in &self.harness.state.errors()[from..] {
            if let RunError::BudgetExceeded { node, .. } = error {
                self.refusals.push((step, *node));
            }
        }
    }

    /// The attempts entry for a firing that just finished or settled.
    fn record_attempts(&mut self, key: (u32, u32), firing: FiringId) {
        let attempt = self
            .starts
            .iter()
            .rev()
            .find(|start| start.firing == firing)
            .map_or(1, |start| start.attempt);
        let status = self
            .harness
            .state
            .history()
            .iter()
            .rev()
            .find(|record| record.firing == firing)
            .map_or_else(
                || "unrecorded".to_owned(),
                |record| record_tag(&record.outcome.status),
            );
        self.attempts.push((key.0, key.1, attempt, status));
    }

    /// Take the `StartStep` commands issued so far, keeping everything else,
    /// and look up each attempt's scripted outcome.
    fn drain_starts(&mut self, step: usize) -> Vec<Start> {
        let mut starts = Vec::new();
        let case = self.case;
        let ordinals = &mut self.ordinals;
        self.harness.commands.retain(|command| {
            let Command::StartStep(resolved) = command else {
                return true;
            };
            let node = resolved.node();
            let attempt = resolved.attempt().raw();
            let ordinal = *ordinals.of_firing.entry(resolved.id()).or_insert_with(|| {
                let next = ordinals.per_node.entry(node.raw()).or_default();
                *next += 1;
                *next - 1
            });
            starts.push(Start {
                firing: resolved.id(),
                node,
                generation: resolved.generation().raw(),
                ordinal,
                attempt,
                outcome: case.nodes[node.index()].outcome(ordinal, attempt),
                inputs: resolved.inputs().iter().map(|token| token.edge).collect(),
                sources: resolved.inputs().iter().map(|token| token.from).collect(),
                step,
                reported: None,
            });
            false
        });
        starts
    }

    /// Record a step's new firings as live.
    fn started(&mut self, batch: Vec<Start>) {
        let mut keys: Vec<(u32, u32)> = batch
            .iter()
            .map(|start| (start.node.raw(), start.generation))
            .collect();
        for start in &batch {
            self.live
                .insert((start.node.raw(), start.generation), start.firing);
        }
        keys.sort_unstable();
        self.steps.push(keys);
        self.starts.extend(batch);
    }

    fn into_run(self) -> Run {
        let harness = self.harness;
        let key_of: BTreeMap<FiringId, (u32, u32)> = self
            .starts
            .iter()
            .map(|start| (start.firing, (start.node.raw(), start.generation)))
            .collect();
        let retries = harness
            .scheduled_retries
            .iter()
            .map(|(firing, next, delay)| {
                let (node, generation) = key_of[firing];
                let nanos = u64::try_from(delay.as_nanos())
                    .expect("a base delay is built from u64 nanoseconds");
                (node, generation, next.raw(), nanos)
            })
            .collect();
        let controls = self
            .controls
            .iter()
            .map(|control| {
                let (node, generation) = key_of[&control.firing];
                (node, generation, control.stop.tag().to_owned())
            })
            .collect();
        let started: BTreeSet<FiringId> = self.starts.iter().map(|start| start.firing).collect();
        let completed = harness
            .state
            .history()
            .iter()
            .filter(|record| !started.contains(&record.firing))
            .map(|record| (record.node.raw(), record.generation.raw()))
            .collect();
        let parked = harness
            .state
            .pending_tokens()
            .map(|((node, generation), token)| (node.raw(), generation.raw(), token.edge.raw()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let budget_exceeded = harness
            .state
            .errors()
            .iter()
            .filter_map(|error| match error {
                RunError::BudgetExceeded { node, .. } => Some(node.raw()),
                _ => None,
            })
            .collect();
        let status = match harness.status {
            Some(RunStatus::Success) => "success",
            Some(RunStatus::Failed) => "failed",
            Some(RunStatus::Cancelled) => "cancelled",
            None => "unsettled",
        };
        Run {
            observed: Observed {
                steps: self.steps,
                finished: self.finished,
                parked,
                budget_exceeded,
                status: status.to_owned(),
                attempts: self.attempts,
                retries,
                controls,
                completed,
            },
            starts: self.starts,
            stops: self.stops,
            controls: self.controls,
            settled: self.settled,
            refusals: self.refusals,
            harness,
        }
    }
}

/// A record's status tag, naming a partial success's underlying failure
/// (`partial_success/timed_out`), as the Lean model's `Status.tag` does.
fn record_tag(status: &Status) -> String {
    match status {
        Status::PartialSuccess {
            underlying: Some(underlying),
        } => format!("partial_success/{}", underlying.status().tag()),
        status => status.tag().to_owned(),
    }
}

/// Which of its node's started firings each firing is.
#[derive(Default)]
struct Ordinals {
    per_node:  BTreeMap<u32, usize>,
    of_firing: BTreeMap<FiringId, usize>,
}
