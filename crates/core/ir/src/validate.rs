//! Load-time validation: the invariants of §7, plus the structural checks the
//! rest of the system assumes (ids in range, node index == `NodeId`, ...).
//!
//! Every check runs, so one call reports every problem rather than the first.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use smol_str::SmolStr;

use crate::expr::Expr;
use crate::flow::FailureClass;
use crate::graph::{
    Completion, EdgeTransition, ExpandTarget, Expansion, ExprOrValue, Graph, GraphBody, Guard,
    JoinPolicy, ResultProjection, SelectionPolicy,
};
use crate::ids::{EdgeId, ExprId, Live, NodeId, ScopeId, StepKindId};
use crate::step::StepKinds;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError<S = Live> {
    // ── Structure ──────────────────────────────────────────────────────────
    #[error("node at index {index} declares id {declared}; node ids must equal their index")]
    NodeIdMismatch {
        index:    usize,
        declared: NodeId<S>,
    },
    #[error("scope at index {index} declares id {declared}; scope ids must equal their index")]
    ScopeIdMismatch {
        index:    usize,
        declared: ScopeId<S>,
    },
    #[error("node {node} refers to unknown scope {scope}")]
    UnknownScope { node: NodeId<S>, scope: ScopeId<S> },
    #[error("node {node} names invalid cancellation group anchor {anchor}")]
    InvalidCancelGroup {
        node:   NodeId<S>,
        anchor: NodeId<S>,
    },
    #[error("cancellation group {anchor} crosses the expansion boundary at node {node}")]
    CancelGroupCrossesExpansion {
        node:   NodeId<S>,
        anchor: NodeId<S>,
    },
    #[error("edge {edge} on node {from} points at unknown node {to}")]
    UnknownTarget {
        from: NodeId<S>,
        edge: EdgeId<S>,
        to:   NodeId<S>,
    },
    #[error("node {node} uses step kind `{kind}` which is not registered")]
    UnknownStepKind { node: NodeId<S>, kind: StepKindId },
    #[error("node {node} has an invalid step config ({class}): {message}")]
    BadStepConfig {
        node:    NodeId<S>,
        /// The step's own failure class — the same one a firing-time rejection
        /// of this config carries (`bad_config`, or a `check_raw` class).
        class:   FailureClass,
        message: String,
    },
    #[error("the graph has no entry nodes")]
    NoEntry,
    #[error("entry list refers to unknown node {0}")]
    UnknownEntry(NodeId<S>),
    #[error("entry node {0} is listed twice")]
    DuplicateEntry(NodeId<S>),
    #[error(
        "entry node {0} has an incoming forward edge; entry nodes are seeded, and only \
         a back edge may point at one"
    )]
    EntryHasIncoming(NodeId<S>),
    #[error("the completion policy names unknown node {0}")]
    CompletionUnknownNode(NodeId<S>),
    #[error("the result projection names unknown node {0}")]
    ResultUnknownNode(NodeId<S>),

    // ── Invariant 1 ────────────────────────────────────────────────────────
    #[error("cycle through nodes {} contains no back edge", fmt_ids(.0))]
    CycleWithoutBackEdge(Vec<NodeId<S>>),

    // ── Invariant 2 ────────────────────────────────────────────────────────
    #[error("node {node}: Guard::Always on arm {arm} is not the final arm of its group")]
    AlwaysNotLast { node: NodeId<S>, arm: usize },

    // ── Invariant 3 ────────────────────────────────────────────────────────
    #[error("node {node}: select group {group} has no arms")]
    EmptyGroup { node: NodeId<S>, group: usize },
    #[error("node {node}: restart edges require exactly one routing group")]
    RestartWithMultipleGroups { node: NodeId<S> },
    #[error("node {node}: tier {tier} names edge {edge}, which is not an arm of group {group}")]
    UnknownTierEdge {
        node:  NodeId<S>,
        group: usize,
        tier:  usize,
        edge:  EdgeId<S>,
    },

    // ── Invariant 4 ────────────────────────────────────────────────────────
    #[error("node {0}: Budget.max_firings must be >= 1")]
    ZeroBudget(NodeId<S>),
    #[error("node {0} is reachable through a back edge, so it needs a finite firing budget")]
    UnboundedLoopBudget(NodeId<S>),

    // ── Invariant 5 ────────────────────────────────────────────────────────
    #[error("edge id {0} is used more than once")]
    DuplicateEdgeId(EdgeId<S>),
    #[error("edge id {0} is reserved for seed tokens")]
    ReservedEdgeId(EdgeId<S>),

    // ── Invariant 6 ────────────────────────────────────────────────────────
    #[error("{site} refers to expression {expr}, which is not in the table")]
    UnknownExpr { site: SmolStr, expr: ExprId<S> },

    // ── Invariant 8 ────────────────────────────────────────────────────────
    #[error(
        "node {0} has an incoming back edge, so it is a loop head: use `JoinPolicy::Any`. \
         A forward edge into the head carries only generation 0 and a back edge only \
         generations 1 and up, so no generation ever holds a token on both and any other \
         policy is unsatisfiable forever"
    )]
    LoopHeadMustJoinAny(NodeId<S>),

    // ── Invariant 7 ────────────────────────────────────────────────────────
    #[error("node {node}: expansion subgraph entry {entry} does not reach exit {exit}")]
    ExitUnreachable {
        node:  NodeId<S>,
        entry: NodeId<S>,
        exit:  NodeId<S>,
    },
    #[error(
        "node {node}: expansion subgraph exit {exit} does not postdominate entry {entry}; \
         node {offender} can complete the region without reaching the exit"
    )]
    ExitNotPostdominator {
        node:     NodeId<S>,
        entry:    NodeId<S>,
        exit:     NodeId<S>,
        offender: NodeId<S>,
    },
    #[error(
        "node {node}: edge {edge} crosses the expansion subgraph boundary; \
         only edges into the entry and out of the exit may cross"
    )]
    BoundaryCrossing { node: NodeId<S>, edge: EdgeId<S> },

    // ── Invariant 10 ───────────────────────────────────────────────────────
    #[error(
        "node {node} joins with `JoinPolicy::All`, but its incoming edges {first} and \
         {second} are arms of one routing group on node {from}. A group emits at most \
         one token, so the two edges never both deliver and node {node} can never run"
    )]
    AllJoinExclusiveArms {
        node:   NodeId<S>,
        from:   NodeId<S>,
        first:  EdgeId<S>,
        second: EdgeId<S>,
    },
    #[error(
        "node {node} joins with `JoinPolicy::Quorum {{ n: {n} }}`, but only {fan_in} routing \
         group(s) can send it a token. A group emits at most one token, so the quorum is \
         never reached and node {node} can never run"
    )]
    QuorumExceedsFanIn {
        node:   NodeId<S>,
        n:      u32,
        fan_in: usize,
    },
}

/// Node ids joined for a message: a cycle is a list, and `Vec` has no
/// `Display`.
fn fmt_ids<S>(ids: &[NodeId<S>]) -> String {
    let mut out = String::new();
    for (index, id) in ids.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&id.raw().to_string());
    }
    out
}

/// The structural location a validation error describes. Callers can format it
/// for their own container or map it to source spans without matching every
/// [`ValidationError`] variant again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidationLocation<S = Live> {
    Graph,
    Node(NodeId<S>),
    Edge(EdgeId<S>),
    Scope(ScopeId<S>),
    Entry(NodeId<S>),
    Site(SmolStr),
}

impl<S> ValidationError<S> {
    /// The diagnostic code a frontend reports this error under.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NodeIdMismatch { .. } => "validate.node_id_mismatch",
            Self::ScopeIdMismatch { .. } => "validate.scope_id_mismatch",
            Self::UnknownScope { .. } => "validate.unknown_scope",
            Self::InvalidCancelGroup { .. } => "validate.invalid_cancel_group",
            Self::CancelGroupCrossesExpansion { .. } => "validate.cancel_group_expansion",
            Self::UnknownTarget { .. } => "validate.unknown_target",
            Self::UnknownStepKind { .. } => "step.unknown_kind",
            // Deliberately not the step's own `class`: the code is a
            // machine-matched contract, and its stability beats specificity.
            // The class is in the message instead.
            Self::BadStepConfig { .. } => "step.bad_config",
            Self::NoEntry => "validate.no_entry",
            Self::UnknownEntry(_) => "validate.unknown_entry",
            Self::DuplicateEntry(_) => "validate.duplicate_entry",
            Self::CompletionUnknownNode(_) => "validate.completion_unknown_node",
            Self::ResultUnknownNode(_) => "validate.result_unknown_node",
            Self::CycleWithoutBackEdge(_) => "validate.cycle_without_back_edge",
            Self::AlwaysNotLast { .. } => "validate.always_not_last",
            Self::EmptyGroup { .. } => "validate.empty_group",
            Self::RestartWithMultipleGroups { .. } => "validate.restart_multiple_groups",
            Self::UnknownTierEdge { .. } => "validate.unknown_tier_edge",
            Self::ZeroBudget(_) => "validate.zero_budget",
            Self::UnboundedLoopBudget(_) => "validate.unbounded_loop_budget",
            Self::LoopHeadMustJoinAny(_) => "validate.loop_head_must_join_any",
            Self::DuplicateEdgeId(_) | Self::ReservedEdgeId(_) => "validate.edge_id",
            Self::UnknownExpr { .. } => "validate.unknown_expr",
            Self::ExitUnreachable { .. }
            | Self::ExitNotPostdominator { .. }
            | Self::BoundaryCrossing { .. } => "validate.expansion_region",
            Self::EntryHasIncoming(_) => "validate.entry_has_incoming",
            Self::AllJoinExclusiveArms { .. } => "validate.all_join_exclusive_arms",
            Self::QuorumExceedsFanIn { .. } => "validate.quorum_exceeds_fan_in",
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "the index names a slot in the node or scope table, and those ids are \
                  `u32`, so no index that table can hold exceeds `u32::MAX`"
    )]
    pub fn location(&self) -> ValidationLocation<S> {
        match self {
            Self::NodeIdMismatch { index, .. } => {
                ValidationLocation::Node(NodeId::new(*index as u32))
            }
            Self::ScopeIdMismatch { index, .. } => {
                ValidationLocation::Scope(ScopeId::new(*index as u32))
            }
            Self::UnknownScope { node, .. }
            | Self::InvalidCancelGroup { node, .. }
            | Self::CancelGroupCrossesExpansion { node, .. }
            | Self::UnknownStepKind { node, .. }
            | Self::BadStepConfig { node, .. }
            | Self::AlwaysNotLast { node, .. }
            | Self::EmptyGroup { node, .. }
            | Self::RestartWithMultipleGroups { node }
            | Self::UnknownTierEdge { node, .. }
            | Self::ExitUnreachable { node, .. }
            | Self::ExitNotPostdominator { node, .. }
            | Self::BoundaryCrossing { node, .. }
            | Self::ZeroBudget(node)
            | Self::UnboundedLoopBudget(node)
            | Self::LoopHeadMustJoinAny(node)
            | Self::AllJoinExclusiveArms { node, .. }
            | Self::QuorumExceedsFanIn { node, .. } => ValidationLocation::Node(*node),
            Self::UnknownTarget { edge, .. }
            | Self::DuplicateEdgeId(edge)
            | Self::ReservedEdgeId(edge) => ValidationLocation::Edge(*edge),
            Self::NoEntry | Self::CompletionUnknownNode(_) | Self::ResultUnknownNode(_) => {
                ValidationLocation::Graph
            }
            Self::UnknownEntry(node)
            | Self::DuplicateEntry(node)
            | Self::EntryHasIncoming(node) => ValidationLocation::Entry(*node),
            Self::CycleWithoutBackEdge(nodes) => nodes
                .first()
                .copied()
                .map_or(ValidationLocation::Graph, ValidationLocation::Node),
            Self::UnknownExpr { site, .. } => ValidationLocation::Site(site.clone()),
        }
    }

    /// The node a source frontend should use as the primary diagnostic span.
    /// Errors without a meaningful node return `None` and use the document
    /// span.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the index names a slot in the node table, and node ids are `u32`, so no \
                  index that table can hold exceeds `u32::MAX`"
    )]
    pub fn primary_node(&self) -> Option<NodeId<S>> {
        match self {
            Self::NodeIdMismatch { index, .. } => Some(NodeId::new(*index as u32)),
            Self::UnknownScope { node, .. }
            | Self::InvalidCancelGroup { node, .. }
            | Self::CancelGroupCrossesExpansion { node, .. }
            | Self::UnknownStepKind { node, .. }
            | Self::BadStepConfig { node, .. }
            | Self::AlwaysNotLast { node, .. }
            | Self::EmptyGroup { node, .. }
            | Self::RestartWithMultipleGroups { node }
            | Self::UnknownTierEdge { node, .. }
            | Self::ExitUnreachable { node, .. }
            | Self::ExitNotPostdominator { node, .. }
            | Self::BoundaryCrossing { node, .. }
            | Self::UnknownEntry(node)
            | Self::DuplicateEntry(node)
            | Self::EntryHasIncoming(node)
            | Self::ZeroBudget(node)
            | Self::UnboundedLoopBudget(node)
            | Self::LoopHeadMustJoinAny(node)
            | Self::AllJoinExclusiveArms { node, .. }
            | Self::QuorumExceedsFanIn { node, .. } => Some(*node),
            Self::UnknownTarget { from, .. } => Some(*from),
            Self::CycleWithoutBackEdge(nodes) => nodes.first().copied(),
            Self::ScopeIdMismatch { .. }
            | Self::NoEntry
            | Self::CompletionUnknownNode(_)
            | Self::ResultUnknownNode(_)
            | Self::DuplicateEdgeId(_)
            | Self::ReservedEdgeId(_)
            | Self::UnknownExpr { .. } => None,
        }
    }

    /// A hint to attach beneath the message, if the error has one.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::LoopHeadMustJoinAny(_) => Some(
                "put a dedicated join node in front of the loop head, and let the back edge target the loop head",
            ),
            Self::CycleWithoutBackEdge(_) => {
                Some("mark the edge that closes the cycle as a back edge")
            }
            Self::UnboundedLoopBudget(_) => {
                Some("give every node in the loop a finite `Budget.max_firings`")
            }
            Self::AlwaysNotLast { .. } => {
                Some("move the `Guard::Always` arm to the end of its select group")
            }
            Self::AllJoinExclusiveArms { .. } => Some(
                "join with `Any` to run on whichever arm the group picks, or send each arm to a node of its own",
            ),
            Self::QuorumExceedsFanIn { .. } => Some(
                "arms of one routing group count once toward a quorum; lower `n`, or route more groups into the node",
            ),
            _ => None,
        }
    }
}

/// Something worth flagging that is still a legal graph.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ValidationWarning<S = Live> {
    #[error(
        "scope {scope} can be re-entered at node {at}: a path leaves the scope through \
         node {via} and comes back. A scope is released once nothing in it can run, and \
         release is irreversible, so re-entry acquires a fresh runtime and workspace and \
         anything the earlier firings left behind is gone. Either restructure so the \
         returning path joins on an edge from inside the scope, or accept \
         fresh-environment semantics at node {at}. What remains after suppression is still \
         a static over-approximation: it cannot tell whether a given run reaches the \
         releasing state."
    )]
    ScopeReentry {
        scope: ScopeId<S>,
        /// The node inside the scope the path comes back to.
        at:    NodeId<S>,
        /// The node outside the scope the path travels through.
        via:   NodeId<S>,
    },
    #[error(
        "node {node} sets `run_on_cancel` on an expansion node; a cancelled scope \
         never splices, so the flag is ignored (v1). Flag the template nodes inside \
         the region instead — clones inherit it"
    )]
    RunOnCancelExpansion { node: NodeId<S> },
    #[error(
        "node {node} joins an edge from node {inside}, which a loop reaches, with an edge from \
         node {outside}, which no loop reaches. A token keeps the generation its loop gave it, \
         and a join matches tokens of one generation, so once the loop has iterated the two \
         never meet: node {node} waits forever, and the run can still report success"
    )]
    JoinAcrossGenerations {
        node:    NodeId<S>,
        inside:  NodeId<S>,
        outside: NodeId<S>,
    },
}

impl<S> ValidationWarning<S> {
    /// The diagnostic code a frontend reports this warning under.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ScopeReentry { .. } => "lint.scope_reentry",
            Self::RunOnCancelExpansion { .. } => "lint.run_on_cancel_expansion",
            Self::JoinAcrossGenerations { .. } => "lint.join_across_generations",
        }
    }

    /// The node a frontend anchors the diagnostic's span to.
    pub fn primary_node(&self) -> NodeId<S> {
        match self {
            Self::ScopeReentry { at, .. } => *at,
            Self::RunOnCancelExpansion { node } | Self::JoinAcrossGenerations { node, .. } => *node,
        }
    }

    /// A hint to attach beneath the message, if the warning has one.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::ScopeReentry { .. } => Some(
                "this lint is a static over-approximation: it fires whenever a path can leave a scope and \
                 return, and cannot tell whether a given run reaches the releasing state",
            ),
            Self::RunOnCancelExpansion { .. } => None,
            Self::JoinAcrossGenerations { .. } => Some(
                "the join fires only when the loop exits in its first iteration; nothing yet \
                 waits for a loop and a path outside it together (engine-spec.md §14)",
            ),
        }
    }
}

/// Everything one validation pass found. Errors block a load; warnings do not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationReport<S = Live> {
    pub errors:   Vec<ValidationError<S>>,
    pub warnings: Vec<ValidationWarning<S>>,
}

impl<S> Default for ValidationReport<S> {
    fn default() -> Self {
        Self {
            errors:   Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl<S> ValidationReport<S> {
    /// No errors. Warnings may still be present.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// The warnings, or the errors that block the load.
    pub fn into_result(self) -> Result<Vec<ValidationWarning<S>>, Vec<ValidationError<S>>> {
        if self.errors.is_empty() {
            Ok(self.warnings)
        } else {
            Err(self.errors)
        }
    }
}

/// Validate a graph in HIR form: `expand` and config placeholders are allowed.
///
/// Errors only. Use [`check`] when the warnings matter too.
pub fn validate<S>(graph: &Graph<S>) -> Result<(), Vec<ValidationError<S>>> {
    validate_with(graph, None)
}

/// Validate a graph and return both errors and warnings.
pub fn check<S>(graph: &Graph<S>) -> ValidationReport<S> {
    check_with(graph, None)
}

/// Validate a graph against a step registry and return both errors and
/// warnings.
pub(crate) fn check_with<S>(
    graph: &Graph<S>,
    registry: Option<&dyn StepKinds>,
) -> ValidationReport<S> {
    let mut warnings = Vec::new();
    check_scope_reentry(&graph.body, &mut warnings);
    check_run_on_cancel(&graph.body, &mut warnings);
    check_join_across_generations(&graph.body, &mut warnings);
    ValidationReport {
        errors: collect(graph, registry),
        warnings,
    }
}

/// Validate a graph and resolve every step kind against `registry`.
pub fn validate_with<S>(
    graph: &Graph<S>,
    registry: Option<&dyn StepKinds>,
) -> Result<(), Vec<ValidationError<S>>> {
    done(collect(graph, registry))
}

/// Only the registry-dependent pass: every step kind resolves and every
/// literal config parses. For a graph whose structural passes already ran —
/// one a frontend just lowered and validated.
pub fn validate_step_kinds<S>(
    graph: &Graph<S>,
    registry: &dyn StepKinds,
) -> Result<(), Vec<ValidationError<S>>> {
    let mut errors = Vec::new();
    check_step_kinds(&graph.body, registry, &mut errors);
    done(errors)
}

fn done<S>(errors: Vec<ValidationError<S>>) -> Result<(), Vec<ValidationError<S>>> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

pub(crate) fn collect<S>(
    graph: &Graph<S>,
    registry: Option<&dyn StepKinds>,
) -> Vec<ValidationError<S>> {
    let mut errors = Vec::new();
    check_structure(&graph.body, &mut errors);
    if let Some(registry) = registry {
        check_step_kinds(&graph.body, registry, &mut errors);
    }
    check_completion(graph, &mut errors);
    collect_body_tail(&graph.body, &mut errors);
    // Not in `collect_body_tail`: a splice fragment's nodes gain routing
    // groups when the fragment attaches, so their fan-in is known only then.
    check_quorum_fan_in(&graph.body, &mut errors);
    errors
}

/// Validate the shared structural body without cloning it into a whole graph.
/// Run-level completion checks stay with [`collect`].
pub(crate) fn collect_body<S>(
    body: &GraphBody<S>,
    registry: Option<&dyn StepKinds>,
) -> Vec<ValidationError<S>> {
    let mut errors = Vec::new();
    check_structure(body, &mut errors);
    if let Some(registry) = registry {
        check_step_kinds(body, registry, &mut errors);
    }
    collect_body_tail(body, &mut errors);
    errors
}

fn collect_body_tail<S>(body: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    check_edge_ids(body, errors);
    check_routing_shape(body, errors);
    check_exprs_resolve(body, errors);
    check_back_edges(body, errors);
    check_budgets(body, errors);
    check_loop_head_joins(body, errors);
    check_all_join_arms(body, errors);
    check_expansions(body, errors);
}

// ── Structure ─────────────────────────────────────────────────────────────

fn check_structure<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    for (index, scope) in graph.scopes.iter().enumerate() {
        if scope.id.index() != index {
            errors.push(ValidationError::ScopeIdMismatch {
                index,
                declared: scope.id,
            });
        }
    }

    for (index, node) in graph.nodes.iter().enumerate() {
        if node.id.index() != index {
            errors.push(ValidationError::NodeIdMismatch {
                index,
                declared: node.id,
            });
        }
        if graph.scope(node.scope).is_none() {
            errors.push(ValidationError::UnknownScope {
                node:  node.id,
                scope: node.scope,
            });
        }
        if let Some(anchor) = node.cancel_group
            && !graph.node(anchor).is_some_and(|target| {
                target.cancel_group == Some(anchor) && target.scope == node.scope
            })
        {
            errors.push(ValidationError::InvalidCancelGroup {
                node: node.id,
                anchor,
            });
        }
        for edge in node.routing.edges() {
            if graph.node(edge.to).is_none() {
                errors.push(ValidationError::UnknownTarget {
                    from: node.id,
                    edge: edge.id,
                    to:   edge.to,
                });
            }
        }
    }

    if graph.entry.is_empty() {
        errors.push(ValidationError::NoEntry);
    }
    let mut seen = HashSet::new();
    for &entry in &graph.entry {
        if graph.node(entry).is_none() {
            errors.push(ValidationError::UnknownEntry(entry));
            continue;
        }
        if !seen.insert(entry) {
            errors.push(ValidationError::DuplicateEntry(entry));
        }
        // A loop head may be the entry: the seed starts generation 0, and the back
        // edge starts each later one. A forward edge into an entry is a
        // contradiction, though, because the entry is seeded rather than joined.
        if graph.edges().any(|edge| edge.to == entry && !edge.back) {
            errors.push(ValidationError::EntryHasIncoming(entry));
        }
    }
}

/// The one registry-dependent pass: every node's step kind resolves, and its
/// literal config passes the kind's own load-time check.
fn check_step_kinds<S>(
    graph: &GraphBody<S>,
    registry: &dyn StepKinds,
    errors: &mut Vec<ValidationError<S>>,
) {
    for node in &graph.nodes {
        match registry.get(&node.step.kind) {
            None => errors.push(ValidationError::UnknownStepKind {
                node: node.id,
                kind: node.step.kind.clone(),
            }),
            Some(kind) => {
                if let Err(failure) = kind.validate_config(&node.step.config) {
                    errors.push(ValidationError::BadStepConfig {
                        node:    node.id,
                        class:   failure.class,
                        message: failure.message,
                    });
                }
            }
        }
    }
}

/// `Completion::TerminalNode` must name a node that exists. Nothing more: the
/// node is *expected* to be terminal, but the semantics only need a final
/// record, so "terminal" and outside-expansion topology rules belong to
/// frontends.
fn check_completion<S>(graph: &Graph<S>, errors: &mut Vec<ValidationError<S>>) {
    if let Completion::TerminalNode(node) = graph.completion
        && graph.node(node).is_none()
    {
        errors.push(ValidationError::CompletionUnknownNode(node));
    }
    if let ResultProjection::NodeOutput(node) = graph.result
        && graph.node(node).is_none()
    {
        errors.push(ValidationError::ResultUnknownNode(node));
    }
}

/// Invariant 5: edge ids are unique across the whole graph, and none reuses the
/// reserved seed id.
fn check_edge_ids<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    let mut seen = HashSet::new();
    let mut reported = HashSet::new();
    for edge in graph.edges() {
        if edge.id == EdgeId::SEED && reported.insert(edge.id) {
            errors.push(ValidationError::ReservedEdgeId(edge.id));
        }
        if !seen.insert(edge.id) && reported.insert(edge.id) {
            errors.push(ValidationError::DuplicateEdgeId(edge.id));
        }
    }
}

/// Invariants 2 and 3.
fn check_routing_shape<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    for node in &graph.nodes {
        if node.routing.groups.len() != 1
            && node
                .routing
                .edges()
                .any(|edge| edge.transition == EdgeTransition::Restart)
        {
            errors.push(ValidationError::RestartWithMultipleGroups { node: node.id });
        }
        for (group_index, group) in node.routing.groups.iter().enumerate() {
            if group.arms.is_empty() {
                errors.push(ValidationError::EmptyGroup {
                    node:  node.id,
                    group: group_index,
                });
                continue;
            }
            let last = group.arms.len() - 1;
            for (arm_index, arm) in group.arms.iter().enumerate() {
                if matches!(arm.guard, Guard::Always) && arm_index != last {
                    errors.push(ValidationError::AlwaysNotLast {
                        node: node.id,
                        arm:  arm_index,
                    });
                }
            }
            if let SelectionPolicy::Tiered(tiers) = &group.policy {
                let arms: HashSet<_> = group.arms.iter().map(|arm| arm.id).collect();
                for (tier_index, tier) in tiers.iter().enumerate() {
                    for candidate in &tier.candidates {
                        if !arms.contains(&candidate.edge) {
                            errors.push(ValidationError::UnknownTierEdge {
                                node:  node.id,
                                group: group_index,
                                tier:  tier_index,
                                edge:  candidate.edge,
                            });
                        }
                    }
                }
            }
        }
    }
}

// ── Invariant 6 (references) ──────────────────────────────────────────────

fn check_exprs_resolve<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    let table = &graph.exprs;
    let check = |site: String, id: ExprId<S>, errors: &mut Vec<ValidationError<S>>| {
        if table.get(id).is_none() {
            errors.push(ValidationError::UnknownExpr {
                site: SmolStr::new(&site),
                expr: id,
            });
        }
    };

    for (id, expr) in table.iter() {
        let site = format!("expression {}", id.raw());
        for child in children(expr) {
            check(site.clone(), child, errors);
        }
    }

    for scope in &graph.scopes {
        for (key, value) in &scope.env {
            if let ExprOrValue::Expr(id) = value {
                check(format!("scope {} env `{key}`", scope.id), *id, errors);
            }
        }
    }

    for node in &graph.nodes {
        if let Some(pre) = node.precondition {
            check(format!("node {} precondition", node.id), pre, errors);
        }
        if let Some(Expansion::ForEach { items, .. }) = &node.expand {
            check(format!("node {} expansion items", node.id), *items, errors);
        }
        for edge in node.routing.edges() {
            if let Guard::Expr(id) = edge.guard {
                check(format!("edge {} guard", edge.id), id, errors);
            }
            if let Some(map) = edge.map {
                check(format!("edge {} map", edge.id), map, errors);
            }
        }
        for (group_index, group) in node.routing.groups.iter().enumerate() {
            if let SelectionPolicy::Tiered(tiers) = &group.policy {
                for (tier_index, tier) in tiers.iter().enumerate() {
                    for (candidate_index, candidate) in tier.candidates.iter().enumerate() {
                        if let Guard::Expr(id) = candidate.when {
                            check(
                                format!(
                                    "node {} group {group_index} tier {tier_index} candidate {candidate_index} guard",
                                    node.id
                                ),
                                id,
                                errors,
                            );
                        }
                        if let Some(id) = candidate.rank {
                            check(
                                format!(
                                    "node {} group {group_index} tier {tier_index} candidate {candidate_index} rank",
                                    node.id
                                ),
                                id,
                                errors,
                            );
                        }
                    }
                }
            }
        }
    }
}

fn children<S>(expr: &Expr<S>) -> Vec<ExprId<S>> {
    match expr {
        Expr::Lit(_) | Expr::Var(_) => Vec::new(),
        Expr::Field(base, _) => vec![*base],
        Expr::Index(base, idx) => vec![*base, *idx],
        Expr::Unary(_, arg) => vec![*arg],
        Expr::Binary(_, lhs, rhs) => vec![*lhs, *rhs],
        Expr::Cond {
            cond,
            then,
            otherwise,
        } => vec![*cond, *then, *otherwise],
        Expr::Array(items) => items.clone(),
        Expr::Object(fields) => fields.iter().map(|(_, v)| *v).collect(),
        Expr::Call(_, args) => args.clone(),
    }
}

// ── Invariant 1 ───────────────────────────────────────────────────────────

/// Every cycle contains at least one back edge — equivalently, the graph with
/// back edges removed is acyclic. Reports one representative cycle per
/// offending strongly connected component.
fn check_back_edges<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    // Iterative DFS over forward edges only, tracking the current path so a
    // rediscovered grey node yields the cycle itself, not just "a cycle exists".
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Grey,
        Black,
    }

    let n = graph.nodes.len();
    let mut color = vec![Color::White; n];
    let mut reported: HashSet<BTreeSet<u32>> = HashSet::new();

    let successors = |node: NodeId<S>| -> Vec<NodeId<S>> {
        graph
            .node(node)
            .into_iter()
            .flat_map(|nd| nd.routing.edges())
            .filter(|e| !e.back)
            .map(|e| e.to)
            .filter(|to| to.index() < n)
            .collect()
    };

    #[expect(
        clippy::cast_possible_truncation,
        reason = "`n` counts the nodes and node ids are `u32`, so no index into that \
                  table exceeds `u32::MAX`"
    )]
    for start in (0..n).map(|i| NodeId::<S>::new(i as u32)) {
        if color[start.index()] != Color::White {
            continue;
        }
        // (node, index of the next successor to visit)
        let mut stack: Vec<(NodeId<S>, usize)> = vec![(start, 0)];
        let mut path: Vec<NodeId<S>> = vec![start];
        color[start.index()] = Color::Grey;

        while let Some((node, cursor)) = stack.pop() {
            let succ = successors(node);
            if cursor < succ.len() {
                stack.push((node, cursor + 1));
                let next = succ[cursor];
                match color[next.index()] {
                    Color::Grey => {
                        let at = path.iter().position(|p| *p == next).unwrap_or(0);
                        let cycle: Vec<NodeId<S>> = path[at..].to_vec();
                        let key: BTreeSet<u32> = cycle.iter().map(|c| c.raw()).collect();
                        if reported.insert(key) {
                            errors.push(ValidationError::CycleWithoutBackEdge(cycle));
                        }
                    }
                    Color::White => {
                        color[next.index()] = Color::Grey;
                        path.push(next);
                        stack.push((next, 0));
                    }
                    Color::Black => {}
                }
            } else {
                color[node.index()] = Color::Black;
                path.pop();
            }
        }
    }
}

// ── Invariant 4 ───────────────────────────────────────────────────────────

fn check_budgets<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    for node in &graph.nodes {
        if node.budget.max_firings == 0 {
            errors.push(ValidationError::ZeroBudget(node.id));
        }
    }

    // Anything downstream of a back edge can fire once per generation, so its cap
    // is what makes the run terminate.
    for node in loop_reachable(graph) {
        if let Some(nd) = graph.node(node)
            && !nd.budget.is_finite()
        {
            errors.push(ValidationError::UnboundedLoopBudget(node));
        }
    }
}

/// Nodes that can fire more than once: everything forward-reachable from a back
/// edge's target. The finite-budget rule above and the engine's `depends_on`
/// ambiguity rule both count against this one definition.
pub fn loop_reachable<S>(graph: &GraphBody<S>) -> BTreeSet<NodeId<S>> {
    let mut queue: VecDeque<NodeId<S>> = graph
        .edges()
        .filter(|e| e.back)
        .map(|e| e.to)
        .filter(|to| graph.node(*to).is_some())
        .collect();
    let mut seen: BTreeSet<NodeId<S>> = queue.iter().copied().collect();
    while let Some(node) = queue.pop_front() {
        if let Some(nd) = graph.node(node) {
            for edge in nd.routing.edges() {
                if graph.node(edge.to).is_some() && seen.insert(edge.to) {
                    queue.push_back(edge.to);
                }
            }
        }
    }
    seen
}

// ── Invariant 7 ───────────────────────────────────────────────────────────

fn check_expansions<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    for node in &graph.nodes {
        if matches!(
            &node.expand,
            Some(Expansion::ForEach {
                target: ExpandTarget::Node,
                ..
            })
        ) {
            check_cancel_group_region(graph, node.id, &HashSet::from([node.id]), errors);
        }
        let Some(Expansion::ForEach {
            target: ExpandTarget::Subgraph { entry, exit },
            ..
        }) = &node.expand
        else {
            continue;
        };
        let (entry, exit) = (*entry, *exit);
        if graph.node(entry).is_none() || graph.node(exit).is_none() {
            errors.push(ValidationError::ExitUnreachable {
                node: node.id,
                entry,
                exit,
            });
            continue;
        }

        // The region: everything reachable from the entry without passing through
        // the exit. The exit belongs to it but is not traversed.
        let mut region: HashSet<NodeId<S>> = HashSet::from([entry]);
        let mut queue = VecDeque::from([entry]);
        while let Some(node_id) = queue.pop_front() {
            if node_id == exit {
                continue;
            }
            let Some(nd) = graph.node(node_id) else {
                continue;
            };
            for edge in nd.routing.edges() {
                if graph.node(edge.to).is_some() && region.insert(edge.to) {
                    queue.push_back(edge.to);
                }
            }
        }

        if !region.contains(&exit) || entry == exit && graph.node(entry).is_none() {
            errors.push(ValidationError::ExitUnreachable {
                node: node.id,
                entry,
                exit,
            });
            continue;
        }
        check_cancel_group_region(graph, node.id, &region, errors);

        // Postdominance: no node inside the region may finish the region without
        // reaching the exit, so every non-exit member must have somewhere to go.
        for &member in &region {
            if member == exit {
                continue;
            }
            let Some(nd) = graph.node(member) else {
                continue;
            };
            if nd.routing.groups.is_empty() {
                errors.push(ValidationError::ExitNotPostdominator {
                    node: node.id,
                    entry,
                    exit,
                    offender: member,
                });
            }
        }

        // Boundary: edges may only enter at the entry and leave from the exit.
        for source in &graph.nodes {
            let inside = region.contains(&source.id) && source.id != exit;
            for edge in source.routing.edges() {
                let target_inside = region.contains(&edge.to);
                let crosses_in = !inside && source.id != exit && target_inside && edge.to != entry;
                let crosses_out = inside && !target_inside;
                if crosses_in || crosses_out {
                    errors.push(ValidationError::BoundaryCrossing {
                        node: node.id,
                        edge: edge.id,
                    });
                }
            }
        }
    }
}

fn check_cancel_group_region<S>(
    graph: &GraphBody<S>,
    expansion: NodeId<S>,
    region: &HashSet<NodeId<S>>,
    errors: &mut Vec<ValidationError<S>>,
) {
    let crossing: BTreeSet<_> = graph
        .nodes
        .iter()
        .filter_map(|node| {
            node.cancel_group
                .filter(|anchor| region.contains(anchor) != region.contains(&node.id))
        })
        .collect();
    errors.extend(crossing.into_iter().map(|anchor| {
        ValidationError::CancelGroupCrossesExpansion {
            node: expansion,
            anchor,
        }
    }));
}

// ── Invariant 8 ───────────────────────────────────────────────────────────

/// A node with an incoming back edge is a loop head, and a loop head must join
/// with `Any`.
///
/// The reason is structural. A forward edge into the head only ever carries
/// generation 0, and a back edge only ever carries generation 1 and up. Tokens
/// are matched per `(node, generation)`, so no generation ever holds a token on
/// both, and `All` is unsatisfiable forever.
///
/// The corollary users meet first: a node cannot be both a multi-branch `All`
/// join and a loop head. Put a dedicated join node in front of the loop head
/// and let the back edge target the head.
fn check_loop_head_joins<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    let mut heads: BTreeSet<NodeId<S>> = BTreeSet::new();
    for edge in graph.edges() {
        if edge.back && graph.node(edge.to).is_some() {
            heads.insert(edge.to);
        }
    }
    for head in heads {
        if let Some(node) = graph.node(head)
            && node.join != JoinPolicy::Any
        {
            errors.push(ValidationError::LoopHeadMustJoinAny(head));
        }
    }
}

// ── Invariant 10 ──────────────────────────────────────────────────────────

/// Nodes whose join invariant 10 leaves alone. A loop head already has to
/// join with `Any` (invariant 8), and a restart target is entered by the
/// successor execution directly, without waiting on its join.
fn join_exempt<S>(graph: &GraphBody<S>) -> BTreeSet<NodeId<S>> {
    graph
        .edges()
        .filter(|edge| edge.back || edge.transition == EdgeTransition::Restart)
        .map(|edge| edge.to)
        .collect()
}

/// Invariant 10, for `All`: the join may not count two arms of one routing
/// group.
///
/// A group emits at most one token each time its node fires, and a node fires
/// at most once per generation, so two arms of one group never both deliver
/// to the same `(node, generation)`. An `All` join over both waits forever,
/// and under `Completion::AnyFailure` the run still reports success.
/// Expansion does not change this: every clone copies the group whole.
fn check_all_join_arms<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    let exempt = join_exempt(graph);
    for source in &graph.nodes {
        for group in &source.routing.groups {
            let mut arms: BTreeMap<NodeId<S>, Vec<EdgeId<S>>> = BTreeMap::new();
            for arm in &group.arms {
                arms.entry(arm.to).or_default().push(arm.id);
            }
            for (node, edges) in arms {
                let joins_all = graph
                    .node(node)
                    .is_some_and(|target| target.join == JoinPolicy::All);
                if let [first, second, ..] = edges[..]
                    && joins_all
                    && !exempt.contains(&node)
                {
                    errors.push(ValidationError::AllJoinExclusiveArms {
                        node,
                        from: source.id,
                        first,
                        second,
                    });
                }
            }
        }
    }
}

/// Invariant 10, for `Quorum { n }`: at least `n` routing groups can feed the
/// node, where an entry's seed counts as one group.
///
/// Arms of one group count once, for the reason [`check_all_join_arms`]
/// gives. The node a `for_each` body exits to is exempt: each clone adds a
/// group at run time, so its fan-in is known only then. A `for_each` node's
/// own quorum is checked like any other; its clones start without it.
fn check_quorum_fan_in<S>(graph: &GraphBody<S>, errors: &mut Vec<ValidationError<S>>) {
    let exempt = join_exempt(graph);
    let mut feeders: BTreeMap<NodeId<S>, BTreeSet<(NodeId<S>, usize)>> = BTreeMap::new();
    for source in &graph.nodes {
        for (index, group) in source.routing.groups.iter().enumerate() {
            for arm in &group.arms {
                feeders
                    .entry(arm.to)
                    .or_default()
                    .insert((source.id, index));
            }
        }
    }
    let mut collectors = BTreeSet::new();
    for node in &graph.nodes {
        let Some(Expansion::ForEach { target, .. }) = &node.expand else {
            continue;
        };
        let exit = match target {
            ExpandTarget::Node => node.id,
            ExpandTarget::Subgraph { exit, .. } => *exit,
        };
        if let Some(exit) = graph.node(exit) {
            collectors.extend(exit.routing.edges().map(|edge| edge.to));
        }
    }

    for node in &graph.nodes {
        let JoinPolicy::Quorum { n } = node.join else {
            continue;
        };
        if exempt.contains(&node.id) || collectors.contains(&node.id) {
            continue;
        }
        let seed = usize::from(graph.entry.contains(&node.id));
        let fan_in = feeders.get(&node.id).map_or(0, BTreeSet::len) + seed;
        if fan_in < n.max(1) as usize {
            errors.push(ValidationError::QuorumExceedsFanIn {
                node: node.id,
                n,
                fan_in,
            });
        }
    }
}

// ── Warnings ──────────────────────────────────────────────────────────────

/// Warn where a path can leave a scope and come back.
///
/// A scope is released once no live firing, pending token or deferred join
/// needs it, and release is irreversible, so re-entry gets a fresh runtime and
/// workspace. The static condition is that some node outside the scope is both
/// reachable from it and able to reach it.
///
/// **Suppression.** A re-entry node is safe when it joins with `All` and has at
/// least one incoming forward edge from inside the scope. For such a node to
/// fire it needs the inside edge's token, and there are only two cases. Either
/// that token is emitted while the scope is still held, in which case it is a
/// pending token pinning the scope until the join resolves and release cannot
/// happen before re-entry. Or the inside arm never emits — its guard fails, or
/// its group falls through — in which case the `All` join is permanently
/// unsatisfiable, the node never fires, and there is no re-entry at all. Both
/// branches are safe, so suppressing is sound, and the test is purely
/// structural.
///
/// `Any` and `Quorum` re-entry nodes keep the warning: those can genuinely fire
/// on the outside token alone, after the scope has been released.
fn check_scope_reentry<S>(graph: &GraphBody<S>, warnings: &mut Vec<ValidationWarning<S>>) {
    let mut by_scope: BTreeMap<ScopeId<S>, BTreeSet<NodeId<S>>> = BTreeMap::new();
    for node in &graph.nodes {
        by_scope.entry(node.scope).or_default().insert(node.id);
    }

    let mut successors: HashMap<NodeId<S>, Vec<NodeId<S>>> = HashMap::new();
    let mut incoming: HashMap<NodeId<S>, Vec<(NodeId<S>, bool)>> = HashMap::new();
    for node in &graph.nodes {
        for edge in node.routing.edges() {
            if graph.node(edge.to).is_some() {
                successors.entry(node.id).or_default().push(edge.to);
                incoming
                    .entry(edge.to)
                    .or_default()
                    .push((node.id, edge.back));
            }
        }
    }
    let predecessors: HashMap<NodeId<S>, Vec<NodeId<S>>> = incoming
        .iter()
        .map(|(node, sources)| (*node, sources.iter().map(|(from, _)| *from).collect()))
        .collect();

    for (scope, members) in &by_scope {
        let downstream = reachable(members, &successors);
        let upstream = reachable(members, &predecessors);
        // Nodes outside the scope that sit on a leave-and-return path.
        let outside: BTreeSet<NodeId<S>> = downstream
            .intersection(&upstream)
            .filter(|node| !members.contains(node))
            .copied()
            .collect();
        if outside.is_empty() {
            continue;
        }

        for member in members {
            let sources = incoming.get(member).map_or(&[][..], Vec::as_slice);
            let Some((via, _)) = sources.iter().find(|(from, _)| outside.contains(from)) else {
                continue;
            };
            let joins_on_all = graph
                .node(*member)
                .is_some_and(|node| node.join == JoinPolicy::All);
            let has_inside_forward_edge = sources
                .iter()
                .any(|(from, back)| !*back && members.contains(from));
            if joins_on_all && has_inside_forward_edge {
                continue;
            }
            warnings.push(ValidationWarning::ScopeReentry {
                scope: *scope,
                at:    *member,
                via:   *via,
            });
        }
    }
}

/// Warn where `run_on_cancel` sits on an expansion node: a cancelled scope
/// never splices, so the flag can never admit anything there. v1 ignores it;
/// the fix is to flag the template nodes inside the region, which clones
/// inherit.
/// Warn where a join meets tokens a loop carries and tokens no loop reaches.
///
/// A back arm raises a token's generation, and every token after it keeps
/// that generation, so only a node reachable from a loop head can fire past
/// generation 0. A join matches tokens of one generation. An `All` join, or a
/// `Quorum` that needs tokens from both sides, over an edge from such a node
/// and an edge from a node no loop reaches can therefore fire only when the
/// loop exits in generation 0. Loop heads are left to invariant 8.
///
/// The warning is not exact: it also fires for a loop that happens never to
/// iterate, and it misses a join between two loops that exit in different
/// generations.
fn check_join_across_generations<S>(
    graph: &GraphBody<S>,
    warnings: &mut Vec<ValidationWarning<S>>,
) {
    let mut successors: BTreeMap<NodeId<S>, Vec<NodeId<S>>> = BTreeMap::new();
    let mut heads = BTreeSet::new();
    for node in &graph.nodes {
        for edge in node.routing.edges() {
            successors.entry(node.id).or_default().push(edge.to);
            if edge.back {
                heads.insert(edge.to);
            }
        }
    }
    let mut looped = BTreeSet::new();
    let mut queue: VecDeque<NodeId<S>> = heads.iter().copied().collect();
    while let Some(node) = queue.pop_front() {
        if looped.insert(node)
            && let Some(next) = successors.get(&node)
        {
            queue.extend(next.iter().copied());
        }
    }

    for node in &graph.nodes {
        let need = match node.join {
            JoinPolicy::All => None,
            JoinPolicy::Quorum { n } => Some(n.max(1) as usize),
            JoinPolicy::Any => continue,
        };
        if heads.contains(&node.id) {
            continue;
        }
        let mut inside = BTreeSet::new();
        let mut outside = BTreeSet::new();
        for source in &graph.nodes {
            for (index, group) in source.routing.groups.iter().enumerate() {
                if !group.arms.iter().any(|arm| arm.to == node.id) {
                    continue;
                }
                if looped.contains(&source.id) {
                    inside.insert((source.id, index));
                } else {
                    outside.insert((source.id, index));
                }
            }
        }
        let (Some(&(inside_node, _)), Some(&(outside_node, _))) = (inside.first(), outside.first())
        else {
            continue;
        };
        if need.is_some_and(|need| inside.len() >= need || outside.len() >= need) {
            continue;
        }
        warnings.push(ValidationWarning::JoinAcrossGenerations {
            node:    node.id,
            inside:  inside_node,
            outside: outside_node,
        });
    }
}

fn check_run_on_cancel<S>(graph: &GraphBody<S>, warnings: &mut Vec<ValidationWarning<S>>) {
    for node in &graph.nodes {
        if node.run_on_cancel && node.expand.is_some() {
            warnings.push(ValidationWarning::RunOnCancelExpansion { node: node.id });
        }
    }
}

/// Every node reachable from `seeds` in one or more steps.
fn reachable<S>(
    seeds: &BTreeSet<NodeId<S>>,
    edges: &HashMap<NodeId<S>, Vec<NodeId<S>>>,
) -> BTreeSet<NodeId<S>> {
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<NodeId<S>> = seeds
        .iter()
        .flat_map(|node| edges.get(node).into_iter().flatten().copied())
        .collect();
    while let Some(node) = queue.pop_front() {
        if !seen.insert(node) {
            continue;
        }
        queue.extend(edges.get(&node).into_iter().flatten().copied());
    }
    seen
}
