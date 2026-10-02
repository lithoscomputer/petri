//! Random flows: the generator the property tests and the Lean model check
//! share.
//!
//! A case is a small graph of `noop` steps plus the order a host finishes
//! them in. A forward arm from node `i` goes to a node after it; a back arm
//! goes to `i` or a node before it, and starts the next generation, so every
//! cycle holds a back edge (§8 invariant 1). Each node has a firing budget,
//! and the host's outcome for a node can change from one firing to the next,
//! so a loop runs a few times and then either exits or hits its budget. Each
//! routing group picks its first arm whose guard passes: `always`,
//! `success()` or `failure()` over the node's own outcome. Joins are `All`,
//! `Any` or `Quorum { n }`.
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

use engine::{Command, Event, RunError};
use ir::{
    Arm, Budget, EdgeId, FiringId, Graph, GraphBuilder, JoinPolicy, NodeId, Outcome, RunStatus,
    ScopeId, Value,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

use crate::support::{Harness, NOOP};

const MAX_NODES: usize = 7;
const MAX_FIRINGS: u32 = 4;

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct NodeSpec {
    pub join:        JoinSpec,
    /// `Budget.max_firings`.
    pub max_firings: u32,
    /// Whether the host fails the node's first, second, … firing; the last
    /// entry repeats.
    pub outcomes:    Vec<bool>,
    pub groups:      Vec<Vec<ArmSpec>>,
}

impl NodeSpec {
    /// Whether the host fails this node's firing number `ordinal`, from 0.
    pub(crate) fn fails(&self, ordinal: usize) -> bool {
        self.outcomes[ordinal.min(self.outcomes.len() - 1)]
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct FlowCase {
    pub nodes:    Vec<NodeSpec>,
    /// Host choices: at step `k` the host finishes the live firing at position
    /// `schedule[k] % live` in ascending `(node, generation)` order, or the
    /// first one once the schedule runs out.
    pub schedule: Vec<u32>,
}

/// What a run looks like from outside the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Observed {
    /// `(node, generation)` firings started by the seed, then by each host
    /// step, each list sorted.
    pub steps:           Vec<Vec<(u32, u32)>>,
    /// `(node, generation)` firings in the order the host finished them.
    pub finished:        Vec<(u32, u32)>,
    /// `(node, generation, edge)` tokens still waiting at a join when the run
    /// ended.
    pub parked:          Vec<(u32, u32, u32)>,
    /// Nodes whose budget refused a firing, in the order it happened.
    pub budget_exceeded: Vec<u32>,
    /// `success` or `failed`; `unsettled` when the run did not finish.
    pub status:          String,
}

/// One `StartStep` the core issued.
pub(crate) struct Start {
    pub firing:     FiringId,
    pub node:       NodeId,
    pub generation: u32,
    /// Which of the node's firings this is, from 0.
    pub ordinal:    usize,
    /// Whether the host fails it.
    pub fails:      bool,
    pub inputs:     Vec<EdgeId>,
}

/// A finished run: what was observed, plus the harness for invariant checks.
pub(crate) struct Run {
    pub observed: Observed,
    pub starts:   Vec<Start>,
    pub harness:  Harness,
}

// ── Generation ────────────────────────────────────────────────────────────

/// A raw arm: a guard choice, a target offset and a back-arm roll, normalized
/// by position.
type RawArm = (u8, u32, u8);
/// A raw node: its join, whether a loop head keeps that join (and breaks
/// invariant 8), its budget, its outcomes and its groups.
type RawNode = (JoinSpec, bool, u32, Vec<bool>, Vec<Vec<RawArm>>);

fn join_spec() -> impl Strategy<Value = JoinSpec> {
    prop_oneof![
        2 => Just(JoinSpec::All),
        1 => Just(JoinSpec::Any),
        1 => (0u32..=3).prop_map(|n| JoinSpec::Quorum { n }),
    ]
}

fn raw_node() -> impl Strategy<Value = RawNode> {
    let arm = || (0u8..10, any::<u32>(), 0u8..10);
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
        prop::collection::vec(prop::bool::weighted(0.25), 1..=3),
        prop::collection::vec(group, 1..=3),
    )
}

pub(crate) fn flow_case() -> impl Strategy<Value = FlowCase> {
    (2..=MAX_NODES)
        .prop_flat_map(|n| {
            (
                prop::collection::vec(raw_node(), n),
                prop::collection::vec(0u32..8, 0..=2 * n),
            )
        })
        .prop_map(|(raw, schedule)| FlowCase::from_raw(&raw, schedule))
}

impl FlowCase {
    fn from_raw(raw: &[RawNode], schedule: Vec<u32>) -> Self {
        let count = u32::try_from(raw.len()).expect("a case holds at most MAX_NODES nodes");
        // Targets first: an arm with nowhere to go is dropped, and guards
        // depend on an arm's final position in its group.
        let targets: Vec<Vec<Vec<(u8, u32, bool)>>> = raw
            .iter()
            .zip(0..)
            .map(|((.., groups), index)| {
                let later = count - index - 1;
                groups
                    .iter()
                    .map(|arms| {
                        arms.iter()
                            .filter_map(|&(guard, offset, back_roll)| {
                                // About one arm in five loops back; the last node,
                                // which has no forward target, loops back less often
                                // and otherwise ends the flow.
                                if back_roll < 2 || (later == 0 && back_roll < 4) {
                                    Some((guard, offset % (index + 1), true))
                                } else if later > 0 {
                                    Some((guard, index + 1 + offset % later, false))
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
            .filter(|(.., back)| *back)
            .map(|(_, to, _)| *to)
            .collect();

        let mut next_edge = 0;
        let nodes = raw
            .iter()
            .zip(targets)
            .zip(0..)
            .map(
                |(((join, keep_join, max_firings, outcomes, _), groups), index)| {
                    let groups = groups
                        .into_iter()
                        .map(|arms| {
                            let last = arms.len() - 1;
                            arms.into_iter()
                                .enumerate()
                                .map(|(position, (guard, to, back))| {
                                    // Forward arms are mostly unconditional, so joins see
                                    // many tokens; back arms are mostly guarded, so a loop
                                    // exits on an outcome more often than on its budget.
                                    // `Always` only ends a group (§8 invariant 2).
                                    let guard = match (guard, position == last, back) {
                                        (0..=6, true, false) | (0..=1, true, true) => {
                                            GuardSpec::Always
                                        }
                                        (0..=7, _, false) | (0..=5, _, true) => GuardSpec::Success,
                                        _ => GuardSpec::Failure,
                                    };
                                    let edge = next_edge;
                                    next_edge += 1;
                                    ArmSpec {
                                        to,
                                        guard,
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
                        outcomes: outcomes.clone(),
                        groups,
                    }
                },
            )
            .collect();
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
        for (node, id) in self.nodes.iter().zip(&ids) {
            b.set_join(*id, node.join.policy());
            b.set_budget(*id, Budget::looped(node.max_firings));
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
    let mut harness = Harness::new(case.graph());
    harness.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });

    let mut starts = Vec::new();
    let mut live: BTreeMap<(u32, u32), (FiringId, bool)> = BTreeMap::new();
    let mut ordinals: BTreeMap<u32, usize> = BTreeMap::new();
    let mut steps = Vec::new();
    let mut finished = Vec::new();

    let first = drain_starts(&mut harness, case, &mut ordinals);
    steps.push(started_keys(&first, &mut live));
    starts.extend(first);

    // Every host step finishes one firing, and a node fires at most its budget,
    // so a run that needs more steps than the budgets add up to has broken
    // that rule; stop and report it.
    let limit: u32 = case.nodes.iter().map(|node| node.max_firings).sum();
    for step in 0..=limit as usize {
        if live.is_empty() {
            break;
        }
        let choice = case.schedule.get(step).copied().unwrap_or(0) as usize % live.len();
        let (&key, &(firing, fails)) = live.iter().nth(choice).expect("choice is below live.len()");
        live.remove(&key);
        let outcome = if fails {
            Outcome::failure("scripted failure")
        } else {
            Outcome::success(Value::Null)
        };
        harness.finish(firing, outcome);
        finished.push(key);
        let batch = drain_starts(&mut harness, case, &mut ordinals);
        steps.push(started_keys(&batch, &mut live));
        starts.extend(batch);
    }

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
            steps,
            finished,
            parked,
            budget_exceeded,
            status: status.to_owned(),
        },
        starts,
        harness,
    }
}

/// Take the `StartStep` commands issued so far, keeping everything else, and
/// decide each firing's scripted outcome.
fn drain_starts(
    harness: &mut Harness,
    case: &FlowCase,
    ordinals: &mut BTreeMap<u32, usize>,
) -> Vec<Start> {
    let mut starts = Vec::new();
    harness.commands.retain(|command| {
        let Command::StartStep(resolved) = command else {
            return true;
        };
        let node = resolved.node();
        let ordinal = ordinals.entry(node.raw()).or_default();
        starts.push(Start {
            firing: resolved.id(),
            node,
            generation: resolved.generation().raw(),
            ordinal: *ordinal,
            fails: case.nodes[node.index()].fails(*ordinal),
            inputs: resolved.inputs().iter().map(|token| token.edge).collect(),
        });
        *ordinal += 1;
        false
    });
    starts
}

fn started_keys(
    starts: &[Start],
    live: &mut BTreeMap<(u32, u32), (FiringId, bool)>,
) -> Vec<(u32, u32)> {
    let mut keys: Vec<(u32, u32)> = starts
        .iter()
        .map(|start| (start.node.raw(), start.generation))
        .collect();
    for start in starts {
        live.insert(
            (start.node.raw(), start.generation),
            (start.firing, start.fails),
        );
    }
    keys.sort_unstable();
    keys
}
