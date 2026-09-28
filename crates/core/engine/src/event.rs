//! The engine's interface: what goes in ([`Event`]) and what comes out
//! ([`Command`]).
//!
//! The core is sans-IO. It never runs a step, never reads a clock and never
//! blocks. A host turns commands into effects and feeds the results back as
//! events.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use ir::{
    Attempt, CancelScopeId, Control, EdgeId, FiringId, Generation, Node, NodeId, Outcome,
    PickPolicy, RunStatus, SandboxInstance, SandboxLeaseId, ScopeId, StepEvent, Token, Value,
    WorkspaceId, placeholder,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// Default execution limit for one invocation.
pub const DEFAULT_MAX_EXECUTIONS: u32 = 32;

/// The complete, replayable specification for one engine instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineStart {
    pub entry:           EntryPoint,
    pub context:         BTreeMap<SmolStr, Value>,
    pub prior_firings:   BTreeMap<NodeId, u32>,
    pub execution_index: u32,
    pub max_executions:  u32,
}

impl Default for EngineStart {
    fn default() -> Self {
        Self {
            entry:           EntryPoint::GraphEntries,
            context:         BTreeMap::new(),
            prior_firings:   BTreeMap::new(),
            execution_index: 0,
            max_executions:  DEFAULT_MAX_EXECUTIONS,
        }
    }
}

/// How an execution receives its first synthetic seed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryPoint {
    #[default]
    GraphEntries,
    Node(NodeId),
}

/// How one reset-free engine instance ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineExit {
    Terminal {
        status: RunStatus,
    },
    Restart {
        edge:   EdgeId,
        target: NodeId,
        source: FiringId,
    },
}

/// A stable name for one configured decision middleware.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MiddlewareKey(SmolStr);

impl MiddlewareKey {
    pub fn new(value: impl Into<SmolStr>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MiddlewareKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// An engine-local decision identity, stable across crash and reissue. The
/// variant is the decision's kind; there is no separate "point" to keep in
/// agreement with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionId {
    ExecutionStart,
    AttemptStart { firing: FiringId, attempt: Attempt },
    Route { firing: FiringId, attempt: Attempt },
}

impl DecisionId {
    pub const fn attempt_start(firing: FiringId, attempt: Attempt) -> Self {
        Self::AttemptStart { firing, attempt }
    }

    pub const fn route(firing: FiringId, attempt: Attempt) -> Self {
        Self::Route { firing, attempt }
    }
}

/// A recorded admission decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Admit,
    Skip { outcome: Outcome },
    Block { reason: SmolStr },
}

/// The eligible form of one edge sent to the host's routing pipeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingCandidate {
    pub edge:       EdgeId,
    pub weight:     u32,
    pub target:     SmolStr,
    pub rank:       Option<f64>,
    pub transition: ir::EdgeTransition,
    /// The edge is a back edge — a cycle continuation inside this
    /// execution. The failure circuit breaker reads it: a tripped
    /// signature blocks a cycle's own continuation, never the graph's
    /// explicit exit from it. Defaults false for logs recorded before
    /// the field existed.
    #[serde(default)]
    pub back:       bool,
}

/// The core's proposal for one routing group.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingProposal {
    pub group:      u32,
    pub tier:       Option<u32>,
    pub pick:       Option<PickPolicy>,
    pub candidates: Vec<RoutingCandidate>,
}

/// The random draw recorded for a weighted tier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeightedDraw {
    pub tier:       u32,
    pub candidates: Vec<EdgeId>,
    pub roll:       u64,
    pub total:      u64,
}

/// One middleware change in configured chain order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intervention {
    Override {
        middleware: MiddlewareKey,
        edge:       EdgeId,
    },
    Jump {
        middleware: MiddlewareKey,
        target:     NodeId,
    },
    Block {
        middleware: MiddlewareKey,
        reason:     SmolStr,
    },
}

/// The final routing decision the core validates and applies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteDecision {
    Emit(EdgeId),
    Jump(NodeId),
    None,
    Block { reason: SmolStr },
}

/// The recorded result for one routing group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupDecision {
    pub group:    u32,
    pub draw:     Option<WeightedDraw>,
    pub trace:    Vec<Intervention>,
    pub decision: RouteDecision,
}

/// The core record that states which resolved route was applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteApplied {
    Edge {
        firing: FiringId,
        group:  u32,
        edge:   EdgeId,
    },
    Jump {
        firing: FiringId,
        target: NodeId,
    },
    None {
        firing: FiringId,
        group:  u32,
    },
}

impl RouteApplied {
    /// The firing whose routing this record applies.
    pub fn firing(&self) -> FiringId {
        match self {
            Self::Edge { firing, .. } | Self::Jump { firing, .. } | Self::None { firing, .. } => {
                *firing
            }
        }
    }

    /// The applied record projected back onto the decision it came from.
    pub fn decision(&self) -> RouteDecision {
        match self {
            Self::Edge { edge, .. } => RouteDecision::Emit(*edge),
            Self::Jump { target, .. } => RouteDecision::Jump(*target),
            Self::None { .. } => RouteDecision::None,
        }
    }
}

/// Where a polite cancel is aimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelTarget {
    /// A cancel scope. The run's root scope cancels everything.
    Scope(CancelScopeId),
    /// One declared node group, including spliced descendants. An unknown
    /// node or a node without a group is a logged no-op. This never marks
    /// the root cancelled.
    Group(NodeId),
}

/// Something that happened. Every event is appended to the log before `apply`.
///
/// On the wire an event is an object tagged by `event`, named
/// `<subject>.<verb>` after the variant, with the variant's fields beside the
/// tag: `{"event": "step.finished", "firing": 3, "attempt": 1, "outcome": …}`.
/// The public event stream carries the same object, unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    #[serde(rename = "execution.started")]
    ExecutionStarted {
        #[serde(flatten)]
        start: EngineStart,
    },
    /// A token was placed on an edge. The core emits these for its own routing
    /// and seeding; a host may also inject one.
    #[serde(rename = "token.emitted")]
    TokenEmitted {
        #[serde(flatten)]
        token: Token,
    },
    #[serde(rename = "step.started")]
    StepStarted { firing: FiringId, attempt: Attempt },
    /// Logs, artifacts and step-defined progress. Carries no coordination
    /// meaning.
    #[serde(rename = "step.progress.recorded")]
    StepProgressRecorded { firing: FiringId, ev: StepEvent },
    #[serde(rename = "step.finished")]
    StepFinished {
        firing:  FiringId,
        attempt: Attempt,
        outcome: Outcome,
    },
    /// The host's admission pipeline decided on an execution start or an
    /// attempt; `decision_id` says which.
    #[serde(rename = "admission.decided")]
    AdmissionDecided {
        decision_id: DecisionId,
        decision:    Admission,
        trace:       Vec<MiddlewareKey>,
    },
    #[serde(rename = "routing.resolved")]
    RoutingResolved {
        decision_id: DecisionId,
        groups:      Vec<GroupDecision>,
    },
    #[serde(rename = "route.applied")]
    RouteApplied {
        #[serde(flatten)]
        applied: RouteApplied,
    },
    /// The driver waited out a retry's backoff. It applies jitter and does the
    /// sleeping; the core never sees a clock or an RNG.
    #[serde(rename = "retry.elapsed")]
    RetryElapsed {
        firing:       FiringId,
        next_attempt: Attempt,
    },
    /// The result of a `for_each` expansion: clones spliced into the live
    /// graph.
    #[serde(rename = "node.expanded")]
    NodeExpanded {
        node:   NodeId,
        splice: SubgraphSplice,
    },
    /// External cancellation, the polite tier, aimed at a scope or at a node
    /// group. Live firings get `Control::Cancel`, cancelled outcomes route,
    /// and `run_on_cancel` cleanup is admitted (§5).
    #[serde(rename = "cancel.requested")]
    CancelRequested { target: CancelTarget },
    /// External kill, the forced tier. Tokens drop, nothing routes, nothing is
    /// admitted — `run_on_cancel` included — and every live firing in the
    /// closure gets `Control::Kill`, already-cancelling ones included. In
    /// the log so the mode of stopping is recorded, never inferred (§5).
    #[serde(rename = "kill.requested")]
    KillRequested { scope: CancelScopeId },
    /// The host asks the core to deliver a control to one live firing — a human
    /// gate's answer, a supervisor's steering. Question and answer are both in
    /// the log, so replay and resume reproduce a pending interaction.
    ///
    /// Only `Control::Deliver` produces a command, and only for a firing that
    /// is live, not cancelling and not awaiting a retry. Everything else —
    /// a dead or unknown firing, a `Cancel` or `Kill` (which have their own
    /// scope-routed events whose closure bookkeeping a raw per-firing path
    /// would bypass) — is a logged no-op, never a `RunError`: a late answer
    /// must not fail the run (§6).
    #[serde(rename = "control.requested")]
    ControlRequested { firing: FiringId, ctl: Control },
    /// The driver acquired a scope's environment: which sandbox on which
    /// provider, the workspace it holds, the lease it lives under when a
    /// coordinator allocated one, and how long the acquisition took. Recorded
    /// once per acquisition, before any attempt runs in the scope; a resumed
    /// execution and an inherited sandbox's execution each record their own.
    /// Observation only: the core changes nothing.
    #[serde(rename = "scope.acquired")]
    ScopeAcquired {
        scope:       ScopeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease:       Option<SandboxLeaseId>,
        workspace:   WorkspaceId,
        #[serde(flatten)]
        sandbox:     SandboxInstance,
        duration_ms: u64,
    },
    /// The driver could not acquire a scope's environment. Every firing in the
    /// scope then fails, routably, with `error` as its failure message; the
    /// run itself does not abort. `causes` is the error's source chain, the
    /// outermost first; `provider` is the kind the lease was reserved on,
    /// when one was. Observation only: the core changes nothing.
    #[serde(rename = "scope.failed")]
    ScopeFailed {
        scope:       ScopeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease:       Option<SandboxLeaseId>,
        workspace:   WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider:    Option<SmolStr>,
        error:       String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        causes:      Vec<String>,
        duration_ms: u64,
    },
}

impl Event {
    /// A polite cancel of one scope.
    pub const fn cancel_scope(scope: CancelScopeId) -> Self {
        Self::CancelRequested {
            target: CancelTarget::Scope(scope),
        }
    }

    /// A polite cancel of one node group.
    pub const fn cancel_group(node: NodeId) -> Self {
        Self::CancelRequested {
            target: CancelTarget::Group(node),
        }
    }
}

/// Everything an executor needs to run one step, with every expression already
/// resolved.
///
/// The type is the enforcement point for the boundary invariant: **no
/// unresolved expression placeholder crosses it, and a secret reference is the
/// only non-literal form that may.** Its fields are private and the only way to
/// build one is [`ResolvedFiring::new`]. Deserialization goes through the same
/// check, so a value read back off the wire carries the invariant too.
///
/// Secret references survive on purpose. A `ResolvedFiring` is serialized into
/// the event log, so resolving a secret here would write it to disk.
/// `{"$secret": "NAME"}` crosses instead, and the value is fetched at spawn
/// time straight into the child's environment. The reference must name a
/// string; anything else is rejected here rather than reaching a step. Whether
/// a secret reference is in a position that allows one is the step kind's
/// business, not the boundary's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "ResolvedFiringRepr", try_from = "ResolvedFiringRepr")]
pub struct ResolvedFiring {
    id:         FiringId,
    node:       NodeId,
    generation: Generation,
    attempt:    Attempt,
    scope:      ScopeId,
    inputs:     Vec<Token>,
    config:     Value,
}

impl ResolvedFiring {
    /// Build one, refusing a config that still holds an expression placeholder.
    pub fn new(
        id: FiringId,
        node: NodeId,
        generation: Generation,
        attempt: Attempt,
        scope: ScopeId,
        inputs: Vec<Token>,
        config: Value,
    ) -> Result<Self, UnresolvedConfig> {
        if let Some(path) = placeholder::placeholder_path(&config) {
            return Err(UnresolvedConfig {
                node,
                path,
                reason: BoundaryViolation::UnresolvedExpression,
            });
        }
        if let Some(path) = placeholder::malformed_secret_ref(&config) {
            return Err(UnresolvedConfig {
                node,
                path,
                reason: BoundaryViolation::MalformedSecretRef,
            });
        }
        Ok(Self {
            id,
            node,
            generation,
            attempt,
            scope,
            inputs,
            config,
        })
    }

    pub fn id(&self) -> FiringId {
        self.id
    }

    pub fn node(&self) -> NodeId {
        self.node
    }

    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Which try this is, 1-based. The full identity of an attempt is
    /// `(node, generation, attempt)`.
    pub fn attempt(&self) -> Attempt {
        self.attempt
    }

    /// The resource scope the step runs in.
    pub fn scope(&self) -> ScopeId {
        self.scope
    }

    /// The tokens whose arrival satisfied the node's join.
    pub fn inputs(&self) -> &[Token] {
        &self.inputs
    }

    /// The step's configuration. Guaranteed free of expression placeholders.
    pub fn config(&self) -> &Value {
        &self.config
    }

    pub fn into_config(self) -> Value {
        self.config
    }
}

/// What was wrong at the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundaryViolation {
    /// An `{"$expr": id}` placeholder was never resolved.
    UnresolvedExpression,
    /// A `{"$secret": ...}` whose name is not a string.
    MalformedSecretRef,
}

impl fmt::Display for BoundaryViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnresolvedExpression => f.write_str("an unresolved expression"),
            Self::MalformedSecretRef => {
                f.write_str("a secret reference whose name is not a string")
            }
        }
    }
}

/// A config reached the executor boundary in a state it may not cross in.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("step config for node {node} holds {reason} at `{path}`")]
pub struct UnresolvedConfig {
    pub node:   NodeId,
    pub path:   String,
    pub reason: BoundaryViolation,
}

/// The wire shape. Private, so the only public way in is through the checked
/// constructor.
#[derive(Serialize, Deserialize)]
struct ResolvedFiringRepr {
    id:         FiringId,
    node:       NodeId,
    generation: Generation,
    attempt:    Attempt,
    scope:      ScopeId,
    inputs:     Vec<Token>,
    config:     Value,
}

impl From<ResolvedFiring> for ResolvedFiringRepr {
    fn from(f: ResolvedFiring) -> Self {
        Self {
            id:         f.id,
            node:       f.node,
            generation: f.generation,
            attempt:    f.attempt,
            scope:      f.scope,
            inputs:     f.inputs,
            config:     f.config,
        }
    }
}

impl TryFrom<ResolvedFiringRepr> for ResolvedFiring {
    type Error = UnresolvedConfig;

    fn try_from(r: ResolvedFiringRepr) -> Result<Self, Self::Error> {
        Self::new(
            r.id,
            r.node,
            r.generation,
            r.attempt,
            r.scope,
            r.inputs,
            r.config,
        )
    }
}

/// Something the host must do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Ask the host's durable admission pipeline for one decision.
    Admit {
        decision_id: DecisionId,
    },
    /// Ask the host's durable routing pipeline to resolve every group once.
    ResolveRouting {
        decision_id:     DecisionId,
        restart_allowed: bool,
        groups:          Vec<RoutingProposal>,
    },
    /// Run a step. The payload carries the config already resolved against the
    /// firing's context, so the host never reads the graph's unresolved copy.
    StartStep(ResolvedFiring),
    DeliverControl {
        firing: FiringId,
        ctl:    Control,
    },
    /// Wait out `base_delay`, then feed back [`Event::RetryElapsed`].
    ///
    /// The delay is computed deterministically from the node's [`ir::Backoff`].
    /// The driver adds jitter, which is why jitter lives there and not
    /// here.
    ScheduleRetry {
        firing:       FiringId,
        next_attempt: Attempt,
        base_delay:   Duration,
    },
    // reserved: external expansion. The core resolves `items` itself and splices in
    // the same `apply` call, so it never emits this. The variant is the seam for a
    // host that resolves items externally and feeds back `Event::NodeExpanded` — do
    // not delete it as dead code.
    ExpandNode {
        node:       NodeId,
        generation: Generation,
        expr:       ir::ExprId,
    },
    AcquireScope {
        scope: ScopeId,
    },
    ReleaseScope {
        scope: ScopeId,
    },
    FinishExecution {
        exit: EngineExit,
    },
}

/// One `for_each` element: a cloned node or subgraph, ready to splice in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpliceClone {
    /// Position in the `items` array; bound as `index` inside the clone.
    pub index:     u32,
    /// The element itself; bound as `item` inside the clone.
    pub item:      Value,
    /// Fully formed clone nodes. Their ids are already allocated in graph
    /// order.
    pub nodes:     Vec<Node>,
    /// Where the clone starts. Seeded with `seed_edge`.
    pub entry:     NodeId,
    /// Synthetic incoming edge for `entry`, so its join counts like any other.
    pub seed_edge: EdgeId,
}

/// The whole expansion: every clone, plus the cancel scope that covers them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubgraphSplice {
    /// A fresh cancel scope over the clones. `fail_fast` cancels this one.
    pub cancel_scope: CancelScopeId,
    /// The expanding node, which is the region's entry.
    pub source:       NodeId,
    /// The original region the clones replace. Every node in it is superseded:
    /// it never executes, and its outgoing edges stop counting toward
    /// downstream joins, so a collector waits for the clones instead of the
    /// originals.
    pub region:       Vec<NodeId>,
    /// Generation the expansion happened in; clones start there.
    pub generation:   Generation,
    /// Payload the expanding node's join produced, seeded into every clone.
    pub payload:      Value,
    pub clones:       Vec<SpliceClone>,
    /// Admission control: at most this many clone firings run at once.
    pub max_parallel: Option<u32>,
    /// The first clone failure cancels the siblings.
    pub fail_fast:    bool,
}

impl SubgraphSplice {
    /// Every node id this splice added.
    pub fn cloned_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.clones
            .iter()
            .flat_map(|c| c.nodes.iter().map(|n| n.id))
    }
}
