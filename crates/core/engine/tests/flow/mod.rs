//! Random acyclic flows: the generator the property tests and the Lean model
//! check share.
//!
//! A case is a small graph of `noop` steps plus the order a host finishes
//! them in. Node `i` routes only to nodes after it, so every run ends. Each
//! routing group picks its first arm whose guard passes: `always`,
//! `success()` or `failure()` over the node's own outcome. Joins are `All`,
//! `Any` or `Quorum { n }`.
//!
//! Some generated joins can never be satisfied, such as an `All` join over two
//! arms of one routing group. §8 invariant 10 rejects those graphs; the tests
//! run them anyway, to check that such a node never starts.
//!
//! The case is also the wire format of the Lean model check: its JSON is
//! what `lean/PetriModel/Wire.lean` reads, and [`Observed`] is what the model
//! answers.

#![allow(
    dead_code,
    reason = "each test binary compiles the whole module, and no one test uses every helper"
)]

use std::collections::{BTreeMap, BTreeSet};

use engine::{Command, Event};
use ir::{
    Arm, EdgeId, FiringId, Graph, GraphBuilder, JoinPolicy, NodeId, Outcome, RunStatus, ScopeId,
    Value,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

use crate::support::{Harness, NOOP};

const MAX_NODES: usize = 7;

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
    /// The id the builder gives this arm's edge: arms are numbered from 0 in
    /// node, group and arm order.
    pub edge:  u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct NodeSpec {
    pub join:   JoinSpec,
    /// The outcome the host reports when this node runs.
    pub fails:  bool,
    pub groups: Vec<Vec<ArmSpec>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct FlowCase {
    pub nodes:    Vec<NodeSpec>,
    /// Host choices: at step `k` the host finishes the live node at position
    /// `schedule[k] % live` in ascending node order, or the first one once
    /// the schedule runs out.
    pub schedule: Vec<u32>,
}

/// What a run looks like from outside the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Observed {
    /// Nodes started by the seed, then by each host step, each list sorted.
    pub steps:    Vec<Vec<u32>>,
    /// Nodes in the order the host finished them.
    pub finished: Vec<u32>,
    /// `(node, edge)` tokens still waiting at a join when the run ended.
    pub parked:   Vec<(u32, u32)>,
    /// `success` or `failed`; `unsettled` when the run did not finish.
    pub status:   String,
}

/// One `StartStep` the core issued.
pub(crate) struct Start {
    pub firing: FiringId,
    pub node:   NodeId,
    pub inputs: Vec<EdgeId>,
}

/// A finished run: what was observed, plus the harness for invariant checks.
pub(crate) struct Run {
    pub observed: Observed,
    pub starts:   Vec<Start>,
    pub harness:  Harness,
}

// ── Generation ────────────────────────────────────────────────────────────

/// A raw arm: a guard choice and a target offset, normalized by position.
type RawArm = (u8, u32);
type RawNode = (JoinSpec, bool, Vec<Vec<RawArm>>);

fn join_spec() -> impl Strategy<Value = JoinSpec> {
    prop_oneof![
        2 => Just(JoinSpec::All),
        1 => Just(JoinSpec::Any),
        1 => (0u32..=3).prop_map(|n| JoinSpec::Quorum { n }),
    ]
}

fn raw_node() -> impl Strategy<Value = RawNode> {
    let arm = || (0u8..10, any::<u32>());
    // Mostly one arm per group: an arm the group does not choose never
    // delivers, and leaves an `All` join downstream waiting forever.
    let group = prop_oneof![
        3 => prop::collection::vec(arm(), 1..=1),
        1 => prop::collection::vec(arm(), 2..=2),
    ];
    (
        join_spec(),
        prop::bool::weighted(0.15),
        prop::collection::vec(group, 1..=3),
    )
}

pub(crate) fn flow_case() -> impl Strategy<Value = FlowCase> {
    (2..=MAX_NODES)
        .prop_flat_map(|n| {
            (
                prop::collection::vec(raw_node(), n),
                prop::collection::vec(0u32..8, 0..=n),
            )
        })
        .prop_map(|(raw, schedule)| FlowCase::from_raw(&raw, schedule))
}

impl FlowCase {
    fn from_raw(raw: &[RawNode], schedule: Vec<u32>) -> Self {
        let count = u32::try_from(raw.len()).expect("a case holds at most MAX_NODES nodes");
        let mut next_edge = 0;
        let nodes = raw
            .iter()
            .zip(0..)
            .map(|((join, fails, groups), index)| {
                let later = count - index - 1;
                let groups = if later == 0 {
                    Vec::new()
                } else {
                    groups
                        .iter()
                        .map(|arms| {
                            let last = arms.len() - 1;
                            arms.iter()
                                .enumerate()
                                .map(|(position, (guard, offset))| {
                                    // Mostly unconditional, so joins see many tokens;
                                    // `Always` only ends a group (§8 invariant 2).
                                    let guard = match (guard, position == last) {
                                        (0..=6, true) => GuardSpec::Always,
                                        (0..=7, _) => GuardSpec::Success,
                                        _ => GuardSpec::Failure,
                                    };
                                    let edge = next_edge;
                                    next_edge += 1;
                                    ArmSpec {
                                        to: index + 1 + offset % later,
                                        guard,
                                        edge,
                                    }
                                })
                                .collect()
                        })
                        .collect()
                };
                NodeSpec {
                    join: *join,
                    fails: *fails,
                    groups,
                }
            })
            .collect();
        Self { nodes, schedule }
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
                            match arm.guard {
                                GuardSpec::Always => Arm::always(to),
                                GuardSpec::Success => Arm::when(to, success),
                                GuardSpec::Failure => Arm::when(to, failure),
                            }
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
    let mut live: BTreeMap<u32, FiringId> = BTreeMap::new();
    let mut steps = Vec::new();
    let mut finished = Vec::new();

    let first = drain_starts(&mut harness);
    steps.push(started_nodes(&first, &mut live));
    starts.extend(first);

    // Each node fires at most once in an acyclic graph, so a run that needs
    // more host steps than nodes has broken that rule; stop and report it.
    for step in 0..=case.nodes.len() {
        if live.is_empty() {
            break;
        }
        let choice = case.schedule.get(step).copied().unwrap_or(0) as usize % live.len();
        let (&node, &firing) = live.iter().nth(choice).expect("choice is below live.len()");
        live.remove(&node);
        let outcome = if case.nodes[node as usize].fails {
            Outcome::failure("scripted failure")
        } else {
            Outcome::success(Value::Null)
        };
        harness.finish(firing, outcome);
        finished.push(node);
        let batch = drain_starts(&mut harness);
        steps.push(started_nodes(&batch, &mut live));
        starts.extend(batch);
    }

    let parked = harness
        .state
        .pending_tokens()
        .map(|((node, _), token)| (node.raw(), token.edge.raw()))
        .collect::<BTreeSet<_>>()
        .into_iter()
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
            status: status.to_owned(),
        },
        starts,
        harness,
    }
}

/// Take the `StartStep` commands issued so far, keeping everything else.
fn drain_starts(harness: &mut Harness) -> Vec<Start> {
    let mut starts = Vec::new();
    harness.commands.retain(|command| {
        let Command::StartStep(resolved) = command else {
            return true;
        };
        starts.push(Start {
            firing: resolved.id(),
            node:   resolved.node(),
            inputs: resolved.inputs().iter().map(|token| token.edge).collect(),
        });
        false
    });
    starts
}

fn started_nodes(starts: &[Start], live: &mut BTreeMap<u32, FiringId>) -> Vec<u32> {
    let mut nodes: Vec<u32> = starts.iter().map(|start| start.node.raw()).collect();
    for start in starts {
        live.insert(start.node.raw(), start.firing);
    }
    nodes.sort_unstable();
    nodes
}
