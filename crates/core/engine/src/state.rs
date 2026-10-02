//! Coordination state. Everything the engine knows lives here; nothing else
//! does.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use ir::{
    Attempt, CancelScopeId, Completion, EdgeId, EdgeTransition, EvalError, FiringId, Generation,
    Graph, Node, NodeId, NodeRecord, Outcome, RunContext, RunStatus, ScopeId, Status, Token, Value,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::event::{DecisionId, EngineExit, EngineStart, ResolvedFiring, RoutingProposal};
use crate::log::EventLog;

/// A node execution attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Firing {
    pub id:             FiringId,
    pub node:           NodeId,
    pub generation:     Generation,
    /// Which try is running, 1-based. A retry advances this and never touches
    /// `generation`.
    pub attempt:        Attempt,
    /// Resource scope: where the step runs.
    pub scope:          ScopeId,
    /// Innermost cancel scope the firing belongs to.
    pub cancel_scope:   CancelScopeId,
    pub inputs:         Vec<Token>,
    /// The host reported `StepStarted`.
    pub started:        bool,
    /// A `ScheduleRetry` is out; the firing stays live until `RetryElapsed`
    /// arrives, which is what keeps its scope held and the run
    /// non-quiescent.
    pub awaiting_retry: bool,
    /// A `Control::Cancel` has been delivered; the outcome will not be routed.
    pub cancelling:     bool,
}

/// One unresolved durable admission command.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingAdmission {
    /// Present for `AttemptStart`; execution admission has no firing payload.
    pub resolved: Option<ResolvedFiring>,
}

/// One unresolved durable routing command and the outcome-time snapshot used
/// to validate and apply its eventual result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingRouting {
    pub firing:          FiringId,
    pub node:            Node,
    pub generation:      Generation,
    pub attempt:         Attempt,
    pub inputs:          Vec<Token>,
    pub outcome:         Outcome,
    pub run:             RunContext,
    pub restart_allowed: bool,
    pub groups:          Vec<RoutingProposal>,
}

/// An edge application prepared by `RoutingResolved` and consumed by the
/// matching core `RouteApplied` record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum PreparedRoute {
    Edge {
        group:      u32,
        edge:       EdgeId,
        target:     NodeId,
        generation: Generation,
        payload:    Value,
        transition: EdgeTransition,
    },
    Jump {
        target:     NodeId,
        generation: Generation,
    },
    None {
        group: u32,
    },
}

/// The immutable restart request chosen by routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestartIntent {
    pub edge:   EdgeId,
    pub target: NodeId,
    pub source: FiringId,
}

impl Firing {
    /// Park this firing for a retry backoff: it stays live — which holds its
    /// scope and keeps the run non-quiescent — but its step is no longer
    /// running, and the firing waits for `RetryElapsed`. The two flags move
    /// together; this is the one place that writes the pair.
    pub(crate) fn park_for_retry(&mut self) {
        self.awaiting_retry = true;
        self.started = false;
    }
}

/// What a firing produced, kept for status folding and for the `outputs`
/// context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FiringRecord {
    pub firing:     FiringId,
    pub node:       NodeId,
    pub name:       SmolStr,
    pub generation: Generation,
    /// The attempt this outcome came from. Only final attempts are recorded
    /// here.
    pub attempt:    Attempt,
    pub outcome:    Outcome,
}

/// A dynamic set of firings cancellable as a unit. Scopes nest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CancelScope {
    pub id:        CancelScopeId,
    pub parent:    Option<CancelScopeId>,
    pub children:  Vec<CancelScopeId>,
    /// Nodes covered by this scope. The root scope covers everything and leaves
    /// this empty.
    pub nodes:     BTreeSet<NodeId>,
    pub cancelled: bool,
    /// The forced tier: nothing in the scope fires or routes any more, and
    /// `run_on_cancel` admits nothing. Killed implies cancelled.
    #[serde(default)]
    pub killed:    bool,
}

/// Identity of one applied splice batch, whoever produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SpliceBatchId(pub u32);

impl fmt::Display for SpliceBatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One admissible unit of work: a node at one generation. What `Replace`
/// retracts — a struct, not a bare tuple, because these serialize into
/// [`AppliedSplice`] records and read back out of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AdmissionKey {
    pub node:       NodeId,
    pub generation: Generation,
}

/// Bookkeeping for one applied splice batch — a `ForEach` expansion or an
/// outcome upload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppliedSplice {
    pub batch:             SpliceBatchId,
    /// The node whose firing produced the batch: the expansion source, or the
    /// uploader. Core-derived — stamped at apply time, never taken from a
    /// request — and stable across generations, so a loop-head uploader can
    /// replace its own earlier batches.
    pub owner:             NodeId,
    /// Every node the batch added, for admission control and cancellation.
    pub nodes:             BTreeSet<NodeId>,
    /// Resource scopes this batch introduced, in fragment order.
    pub scopes:            Vec<ScopeId>,
    pub cancel_scope:      CancelScopeId,
    pub origin:            SpliceOrigin,
    pub policy:            BatchPolicy,
    pub effects:           Vec<SpliceEffect>,
    /// Firings currently occupying this batch's admission slots.
    pub(crate) live_count: u32,
}

/// Invocation-stable identity of a resource scope. Dynamic identities use
/// the producer's ancestry, not execution-local node or scope allocations.
///
/// On the wire, `{"declared": <scope id>}` or `{"spliced": [..]}`: the
/// snake-case tag every enum inside a stored record carries.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeIdentity {
    Declared(ScopeId),
    /// Alternating producer node and producer-local batch ordinals, followed
    /// by the scope's local ordinal. The first node is an original graph ID;
    /// subsequent nodes are offsets within their parent's splice batch.
    Spliced(Vec<u32>),
}

impl AppliedSplice {
    /// Firings currently occupying this batch's admission slots.
    pub fn live_count(&self) -> u32 {
        self.live_count
    }
}

/// Runtime facts for one graph node. The vector holding these records stays
/// aligned with `Graph::nodes`, so node metadata needs no parallel maps or
/// scans.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct NodeRuntime {
    cancel_scope:   CancelScopeId,
    batch:          Option<SpliceBatchId>,
    clone_bindings: Option<BTreeMap<SmolStr, Value>>,
    superseded:     bool,
}

impl NodeRuntime {
    fn declared() -> Self {
        Self {
            cancel_scope:   CancelScopeId::ROOT,
            batch:          None,
            clone_bindings: None,
            superseded:     false,
        }
    }
}

/// The engine's id allocators. Splice preparation copies this value and
/// advances the copy, so a rejected transaction cannot move canonical state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_field_names,
    reason = "`Allocators` flattens into the serialized `EngineState`, so these names are the \
              state's own field names; each also says it holds the next id, not a count"
)]
pub(crate) struct Allocators {
    /// High-water mark for splice-allocated node ids.
    pub next_node:         u32,
    pub next_edge:         u32,
    pub next_cancel_scope: u32,
}

impl Allocators {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`NodeId` is a `u32`, so a graph can never hold more nodes than a `u32` counts"
    )]
    pub(crate) fn reserve_nodes(&mut self, count: usize) -> u32 {
        let base = self.next_node;
        self.next_node += count as u32;
        base
    }

    pub(crate) fn take_edge(&mut self) -> EdgeId {
        let id = EdgeId::new(self.next_edge);
        self.next_edge += 1;
        id
    }

    pub(crate) fn take_cancel_scope(&mut self) -> CancelScopeId {
        let id = CancelScopeId::new(self.next_cancel_scope);
        self.next_cancel_scope += 1;
        id
    }
}

/// The operation that created a splice batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpliceOrigin {
    Expansion,
    Outcome,
}

/// Scheduler behavior for every node in a splice batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchPolicy {
    pub max_parallel: Option<u32>,
    pub fail_fast:    bool,
}

/// A state change applied with a splice batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpliceEffect {
    Supersede(NodeId),
    Retract(AdmissionKey),
}

/// Anything that makes a run fail without a step failing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum RunError {
    #[error("node {node}: evaluating {site} failed: {error}")]
    Eval {
        node:  NodeId,
        site:  SmolStr,
        error: EvalError,
    },
    #[error("node {node} exceeded its firing budget of {max_firings}")]
    BudgetExceeded {
        node:        NodeId,
        max_firings: u32,
    },
    #[error("node {node}: select group {group} matched no arm and requires one")]
    NoArmMatched { node: NodeId, group: usize },
    #[error("node {node}: `for_each` items evaluated to {got}, not an array")]
    ItemsNotArray { node: NodeId, got: SmolStr },
    #[error("node {node}: step config still holds an unresolved expression at `{path}`")]
    UnresolvedConfig { node: NodeId, path: String },
    #[error("firing {firing} is not waiting for a retry")]
    UnexpectedRetry { firing: FiringId },
    #[error("firing {firing} reported attempt {reported} while running attempt {running}")]
    AttemptMismatch {
        firing:   FiringId,
        reported: Attempt,
        running:  Attempt,
    },
    #[error("node {node}: expansion subgraph entry must be the expanding node")]
    ExpansionEntryMismatch { node: NodeId },
    #[error("token refers to unknown edge {0}")]
    UnknownEdge(EdgeId),
    #[error("unknown node {0}")]
    UnknownNode(NodeId),
    #[error("unknown firing {0}")]
    UnknownFiring(FiringId),
    #[error("execution start has max_executions 0")]
    ZeroExecutionLimit,
    #[error("execution start refers to unknown entry node {0}")]
    UnknownEntryNode(NodeId),
    #[error("decision {0:?} is not pending")]
    UnknownDecision(DecisionId),
    #[error("decision {decision:?} does not match its pending point")]
    DecisionMismatch { decision: DecisionId },
    #[error("routing result for firing {firing} is invalid: {reason}")]
    InvalidRouting { firing: FiringId, reason: SmolStr },
    #[error("routing for firing {firing} was blocked: {reason}")]
    RoutingBlocked { firing: FiringId, reason: SmolStr },
    #[error("admission at {decision:?} was blocked: {reason}")]
    AdmissionBlocked {
        decision: DecisionId,
        reason:   SmolStr,
    },
    #[error("event arrived before ExecutionStarted")]
    NotStarted,
    #[error("event arrived after the run finished")]
    AlreadyFinished,
}

/// The whole state machine's state. Pure data: no handles, no clocks, no IO.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineState {
    /// The live graph. Expansions splice clones into it, so it grows during a
    /// run — and only through [`Self::push_spliced_node`], which keeps it
    /// aligned with `node_runtime`.
    pub(crate) graph: Graph,
    /// Every event applied so far, in order.
    pub log:          EventLog,

    /// Tokens waiting on a join: node, then generation, then edge. Nested
    /// rather than keyed by a `(node, generation)` tuple so the whole state
    /// serializes to JSON, where a map key has to be a primitive.
    pending:        BTreeMap<NodeId, BTreeMap<Generation, BTreeMap<EdgeId, Token>>>,
    /// `(node, generation)` pairs that already fired. Later tokens for them are
    /// dropped, which is what makes `JoinPolicy::Any` fire exactly once.
    fired:          BTreeSet<(NodeId, Generation)>,
    /// Synthetic forced entries bypass their node's ordinary join once.
    #[serde(default)]
    forced_entries: BTreeSet<(NodeId, Generation)>,
    /// Joins that are satisfied but held back by `max_parallel`.
    deferred:       VecDeque<(NodeId, Generation)>,

    live:               BTreeMap<FiringId, Firing>,
    firing_counts:      BTreeMap<NodeId, u32>,
    history:            Vec<FiringRecord>,
    /// Firings whose latest recorded outcome is `Cancelled` — the admission
    /// check runs per token, so this is kept alongside `history` rather
    /// than scanned out of it.
    #[serde(default)]
    cancelled_outcomes: BTreeSet<FiringId>,
    /// Run-scoped state expressions read as `nodes.*` and `kv.*`.
    ///
    /// Derived: every write happens in `apply`, in event order, so replaying
    /// the log rebuilds it exactly. It is never checkpointed as a separate
    /// artifact.
    run:                RunContext,

    cancel_scopes:        BTreeMap<CancelScopeId, CancelScope>,
    /// Per-node runtime facts, indexed directly by `NodeId`.
    node_runtime:         Vec<NodeRuntime>,
    splices:              Vec<AppliedSplice>,
    /// Admissions retracted by a `Replace`: their parked tokens were dropped,
    /// and later tokens for these exact keys are swallowed. Kept flat
    /// beside the per-batch records because the swallow check runs per
    /// token.
    #[serde(default)]
    retracted_admissions: BTreeSet<AdmissionKey>,
    /// Firings settled by a cancel or kill while awaiting a retry backoff. The
    /// driver's sleeper cannot be recalled, so the one matching late
    /// `RetryElapsed` consumes its tombstone silently; any other invalid
    /// `RetryElapsed` still errors.
    #[serde(default)]
    retry_tombstones:     BTreeSet<FiringId>,

    /// External decision commands that keep an execution non-quiescent.
    #[serde(default)]
    pending_admissions: BTreeMap<DecisionId, PendingAdmission>,
    #[serde(default)]
    pending_routing:    BTreeMap<FiringId, PendingRouting>,
    /// Core route records prepared by one resolved command. A vector rather
    /// than tuple map keys keeps the state JSON-compatible.
    #[serde(default)]
    prepared_routes:    BTreeMap<FiringId, VecDeque<PreparedRoute>>,

    /// Synthetic incoming edges for entry nodes and clone entries.
    seed_edges:  BTreeMap<EdgeId, NodeId>,
    /// Resource scopes currently held. A scope is held from the moment one of
    /// its nodes starts until nothing in it can run again, so a chain of
    /// steps in one job does not tear the job down between steps.
    held_scopes: BTreeSet<ScopeId>,

    next_firing: u64,
    #[serde(flatten)]
    allocators:  Allocators,

    start:          Option<EngineStart>,
    exit:           Option<EngineExit>,
    restart_intent: Option<RestartIntent>,
    cancelled:      bool,
    errors:         Vec<RunError>,
}

impl EngineState {
    /// A state ready to receive `Event::ExecutionStarted`.
    pub fn new(graph: Graph) -> Self {
        let next_edge = graph
            .edges()
            .map(|e| e.id.raw())
            .filter(|id| *id != EdgeId::<ir::Live>::SEED.raw())
            .max()
            .map_or(0, |m| m + 1);
        let mut cancel_scopes = BTreeMap::new();
        cancel_scopes.insert(CancelScopeId::ROOT, CancelScope {
            id:        CancelScopeId::ROOT,
            parent:    None,
            children:  Vec::new(),
            nodes:     BTreeSet::new(),
            cancelled: false,
            killed:    false,
        });
        let nodes = graph
            .nodes
            .iter()
            .filter(|node| node.cancel_group.is_some())
            .map(|node| node.id)
            .collect();
        let node_runtime = vec![NodeRuntime::declared(); graph.nodes.len()];
        let mut state = Self {
            graph,
            log: EventLog::new(),
            pending: BTreeMap::new(),
            fired: BTreeSet::new(),
            forced_entries: BTreeSet::new(),
            deferred: VecDeque::new(),
            live: BTreeMap::new(),
            firing_counts: BTreeMap::new(),
            history: Vec::new(),
            cancelled_outcomes: BTreeSet::new(),
            run: RunContext::new(),
            cancel_scopes,
            node_runtime,
            splices: Vec::new(),
            retracted_admissions: BTreeSet::new(),
            retry_tombstones: BTreeSet::new(),
            pending_admissions: BTreeMap::new(),
            pending_routing: BTreeMap::new(),
            prepared_routes: BTreeMap::new(),
            seed_edges: BTreeMap::new(),
            held_scopes: BTreeSet::new(),
            next_firing: 1,
            allocators: Allocators {
                next_node: 0,
                next_edge,
                next_cancel_scope: 1,
            },
            start: None,
            exit: None,
            restart_intent: None,
            cancelled: false,
            errors: Vec::new(),
        };
        state.add_cancel_groups(CancelScopeId::ROOT, &nodes);
        state
    }

    // ── Read-only views ────────────────────────────────────────────────────

    /// The live graph. Read-only: splices grow it only through the engine's
    /// own paired push, which keeps it aligned with the per-node runtime
    /// facts.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    pub fn is_started(&self) -> bool {
        self.start.is_some()
    }

    pub fn is_finished(&self) -> bool {
        self.exit.is_some()
    }

    pub fn start(&self) -> Option<&EngineStart> {
        self.start.as_ref()
    }

    pub fn exit(&self) -> Option<&EngineExit> {
        self.exit.as_ref()
    }

    /// The root scope was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }

    pub fn errors(&self) -> &[RunError] {
        &self.errors
    }

    pub fn history(&self) -> &[FiringRecord] {
        &self.history
    }

    pub fn live_firings(&self) -> impl Iterator<Item = &Firing> {
        self.live.values()
    }

    pub fn firing(&self, id: FiringId) -> Option<&Firing> {
        self.live.get(&id)
    }

    /// The node a firing belongs to, live or finished.
    ///
    /// [`EngineState::firing`] sees only live firings, and by the time an
    /// observer runs for a finish record the firing is already retired — so
    /// this searches history too. The stable way for a host to resolve a
    /// firing to its node, and from there its name and `meta`.
    pub fn firing_node(&self, id: FiringId) -> Option<NodeId> {
        self.live.get(&id).map(|f| f.node).or_else(|| {
            self.history
                .iter()
                .rev()
                .find(|r| r.firing == id)
                .map(|r| r.node)
        })
    }

    /// Run-scoped state as expressions see it.
    pub fn run_context(&self) -> &RunContext {
        &self.run
    }

    /// Latest output of a node instance, by name.
    pub fn output(&self, name: &str) -> Option<&Value> {
        self.run.node(name).map(|record| &record.output)
    }

    /// How many times a node has fired, across generations.
    pub fn firing_count(&self, node: NodeId) -> u32 {
        self.firing_counts.get(&node).copied().unwrap_or(0)
    }

    /// Declared-node firing budgets carried into a restart successor.
    pub fn prior_firings(&self) -> BTreeMap<NodeId, u32> {
        self.firing_counts
            .iter()
            .filter(|(node, _)| self.graph.node(**node).is_some())
            .map(|(node, count)| (*node, *count))
            .collect()
    }

    /// Tokens still waiting on a join, with the `(node, generation)` they wait
    /// at.
    pub fn pending_tokens(&self) -> impl Iterator<Item = ((NodeId, Generation), &Token)> {
        self.pending.iter().flat_map(|(node, generations)| {
            generations.iter().flat_map(move |(generation, tokens)| {
                tokens.values().map(move |t| ((*node, *generation), t))
            })
        })
    }

    pub fn pending_count(&self) -> usize {
        self.pending
            .values()
            .flat_map(|g| g.values())
            .map(BTreeMap::len)
            .sum()
    }

    /// The synthetic incoming edges allocated for entry nodes and expansion
    /// clone entries, with the node each one feeds.
    ///
    /// These are allocated from the free edge-id space above every id the graph
    /// declares, so they never collide with a declared edge, and they are never
    /// written into a [`ir::Routing`] group — they exist only here. Joins count
    /// them alongside real incoming edges, which is what lets an entry node
    /// use any join policy without a special case.
    pub fn seed_edges(&self) -> impl Iterator<Item = (EdgeId, NodeId)> + '_ {
        self.seed_edges.iter().map(|(edge, node)| (*edge, *node))
    }

    pub fn cancel_scope(&self, id: CancelScopeId) -> Option<&CancelScope> {
        self.cancel_scopes.get(&id)
    }

    pub fn splices(&self) -> &[AppliedSplice] {
        &self.splices
    }

    /// Resolve a live scope to the same identity after replay or restart,
    /// even when independent producers splice in a different order.
    pub fn scope_identity(&self, scope: ScopeId) -> Option<ScopeIdentity> {
        self.graph.scope(scope)?;
        let Some((mut batch, local)) = self.splices.iter().find_map(|batch| {
            batch
                .scopes
                .iter()
                .position(|candidate| *candidate == scope)
                .map(|local| (batch, local))
        }) else {
            return Some(ScopeIdentity::Declared(scope));
        };
        let mut path = vec![u32::try_from(local).expect("scope IDs fit u32")];
        loop {
            let preceding = &self.splices[..batch.batch.0 as usize];
            let ordinal = preceding
                .iter()
                .filter(|prior| prior.owner == batch.owner)
                .count();
            path.push(u32::try_from(ordinal).expect("splice batch IDs fit u32"));
            let parent = preceding.iter().find_map(|parent| {
                parent
                    .nodes
                    .iter()
                    .position(|node| *node == batch.owner)
                    .map(|local| (parent, local))
            });
            let Some((parent, local)) = parent else {
                path.push(batch.owner.raw());
                break;
            };
            path.push(u32::try_from(local).expect("node IDs fit u32"));
            batch = parent;
        }
        path.reverse();
        Some(ScopeIdentity::Spliced(path))
    }

    /// Nodes replaced by expansion clones.
    pub fn is_superseded(&self, node: NodeId) -> bool {
        self.node_runtime
            .get(node.index())
            .is_some_and(|runtime| runtime.superseded)
    }

    /// Nothing is running and nothing more can start.
    ///
    /// Joins are checked on every token arrival, so once no firing is live and
    /// nothing is deferred, no pending token can ever satisfy a join. A firing
    /// waiting out a retry backoff is still live, so a run mid-backoff is not
    /// quiescent.
    pub fn is_quiescent(&self) -> bool {
        self.live.is_empty()
            && self.deferred.is_empty()
            && self.pending_admissions.is_empty()
            && self.pending_routing.is_empty()
            && self.prepared_routes.is_empty()
    }

    /// Firings waiting out a retry backoff.
    pub fn awaiting_retry(&self) -> impl Iterator<Item = &Firing> {
        self.live.values().filter(|f| f.awaiting_retry)
    }

    /// The run status folded from node outcomes, as it stands right now, under
    /// the graph's [`Completion`] policy.
    pub fn folded_status(&self) -> RunStatus {
        if self.cancelled {
            return RunStatus::Cancelled;
        }
        // Engine errors are never control flow: they fail the run under both
        // policies.
        if !self.errors.is_empty() {
            return RunStatus::Failed;
        }
        match self.graph.completion {
            Completion::AnyFailure => {
                // A failure on a node that tolerates it is control flow, not a
                // run failure. `run.failed` deliberately still counts it: the
                // record says `failure`, and guards read records.
                let hard_failure = self.history.iter().any(|r| {
                    r.outcome.status.is_failure()
                        && !self.graph.node(r.node).is_some_and(|n| n.tolerates_failure)
                });
                if hard_failure {
                    RunStatus::Failed
                } else {
                    RunStatus::Success
                }
            }
            // Success iff the terminal node has a success-like final record.
            // Failures elsewhere are control flow; a missing record means the run
            // never got there, which fails it whatever else succeeded.
            Completion::TerminalNode(id) => {
                let record = self
                    .graph
                    .node(id)
                    .and_then(|node| self.run.node(&node.name));
                match record {
                    Some(record) if record.status.is_success_like() => RunStatus::Success,
                    _ => RunStatus::Failed,
                }
            }
        }
    }

    /// Whether anything has failed so far: an engine error, or any failed
    /// record in history.
    ///
    /// This is what the `run.failed` static means — "any failure so far", under
    /// every completion policy. It is deliberately not [`Self::folded_status`]:
    /// under [`Completion::TerminalNode`] the fold reads `Failed` until the
    /// exit record exists, which would poison `run.failed` guards mid-run.
    /// Two names, two meanings.
    pub fn has_any_failure(&self) -> bool {
        !self.errors.is_empty() || self.history.iter().any(|r| r.outcome.status.is_failure())
    }

    // ── Mutation used by `apply` ───────────────────────────────────────────

    pub(crate) fn mark_started(&mut self, start: EngineStart) {
        self.run.merge(&start.context);
        self.firing_counts.clone_from(&start.prior_firings);
        self.start = Some(start);
    }

    pub(crate) fn mark_finished(&mut self, exit: EngineExit) {
        self.exit = Some(exit);
    }

    pub(crate) fn force_entry(&mut self, node: NodeId, generation: Generation) {
        self.forced_entries.insert((node, generation));
    }

    pub(crate) fn is_forced_entry(&self, key: (NodeId, Generation)) -> bool {
        self.forced_entries.contains(&key)
    }

    pub(crate) fn take_forced_entry(&mut self, key: (NodeId, Generation)) {
        self.forced_entries.remove(&key);
    }

    pub(crate) fn mark_cancelled(&mut self) {
        self.cancelled = true;
    }

    pub(crate) fn insert_pending_admission(&mut self, id: DecisionId, pending: PendingAdmission) {
        self.pending_admissions.insert(id, pending);
    }

    pub(crate) fn take_pending_admission(&mut self, id: DecisionId) -> Option<PendingAdmission> {
        self.pending_admissions.remove(&id)
    }

    pub fn pending_admissions(&self) -> impl Iterator<Item = (DecisionId, &PendingAdmission)> {
        self.pending_admissions
            .iter()
            .map(|(id, pending)| (*id, pending))
    }

    pub fn has_pending_admission(&self, id: DecisionId) -> bool {
        self.pending_admissions.contains_key(&id)
    }

    pub fn is_awaiting_admission(&self, firing: FiringId) -> bool {
        self.live.get(&firing).is_some_and(|firing| {
            self.has_pending_admission(DecisionId::attempt_start(firing.id, firing.attempt))
        })
    }

    pub(crate) fn remove_admission_for_firing(&mut self, firing: FiringId) {
        self.pending_admissions.retain(
            |id, _| !matches!(id, DecisionId::AttemptStart { firing: f, .. } if *f == firing),
        );
    }

    pub(crate) fn insert_pending_routing(&mut self, pending: PendingRouting) {
        let node = pending.node.id;
        let previous = self.pending_routing.insert(pending.firing, pending);
        debug_assert!(previous.is_none());
        if previous.is_none() {
            // A routing decision is still part of the firing's admission slot.
            // This closes the asynchronous host-decision gap between one clone
            // node finishing and its successor starting.
            self.adjust_batch_live(node, 1);
        }
    }

    pub(crate) fn take_pending_routing(&mut self, firing: FiringId) -> Option<PendingRouting> {
        let pending = self.pending_routing.remove(&firing)?;
        self.adjust_batch_live(pending.node.id, -1);
        Some(pending)
    }

    pub fn pending_routings(&self) -> impl Iterator<Item = &PendingRouting> {
        self.pending_routing.values()
    }

    pub fn has_pending_routing(&self, id: DecisionId) -> bool {
        let DecisionId::Route { firing, attempt } = id else {
            return false;
        };
        self.pending_routing
            .get(&firing)
            .is_some_and(|pending| pending.attempt == attempt)
    }

    pub(crate) fn set_prepared_routes(
        &mut self,
        firing: FiringId,
        routes: VecDeque<PreparedRoute>,
    ) {
        if routes.is_empty() {
            self.prepared_routes.remove(&firing);
        } else {
            self.prepared_routes.insert(firing, routes);
        }
    }

    pub(crate) fn prepared_route(&self, firing: FiringId) -> Option<&PreparedRoute> {
        self.prepared_routes.get(&firing).and_then(VecDeque::front)
    }

    pub(crate) fn take_prepared_route(&mut self, firing: FiringId) -> Option<PreparedRoute> {
        let routes = self.prepared_routes.get_mut(&firing)?;
        let route = routes.pop_front();
        if routes.is_empty() {
            self.prepared_routes.remove(&firing);
        }
        route
    }

    /// Withdraw the routing decisions still open for nodes a kill reached: a
    /// killed closure routes nothing, whenever its outcome was recorded.
    pub(crate) fn withdraw_killed_routings(&mut self) {
        let killed: Vec<FiringId> = self
            .pending_routing
            .values()
            .filter(|pending| self.is_node_killed(pending.node.id))
            .map(|pending| pending.firing)
            .collect();
        for firing in killed {
            self.take_pending_routing(firing);
        }
    }

    pub(crate) fn clear_pending_decisions(&mut self) {
        self.pending_admissions.clear();
        let nodes: Vec<_> = self
            .pending_routing
            .values()
            .map(|pending| pending.node.id)
            .collect();
        self.pending_routing.clear();
        for node in nodes {
            self.adjust_batch_live(node, -1);
        }
        self.prepared_routes.clear();
    }

    pub(crate) fn begin_restart(&mut self, intent: RestartIntent) {
        if self.restart_intent.is_none() {
            self.restart_intent = Some(intent);
        }
    }

    pub(crate) fn restart_intent(&self) -> Option<RestartIntent> {
        self.restart_intent
    }

    pub(crate) fn clear_restart(&mut self) {
        self.restart_intent = None;
    }

    pub(crate) fn push_error(&mut self, error: RunError) {
        self.errors.push(error);
    }

    pub(crate) fn next_firing_id(&mut self) -> FiringId {
        let id = FiringId::new(self.next_firing);
        self.next_firing += 1;
        id
    }

    pub(crate) fn next_cancel_scope_id(&mut self) -> CancelScopeId {
        self.allocators.take_cancel_scope()
    }

    pub(crate) fn next_edge_id(&mut self) -> EdgeId {
        self.allocators.take_edge()
    }

    /// Allocate the id for a node a splice will add: never behind the live
    /// graph, and monotonic across allocations. Two expansions queued in the
    /// same cascade both build their splices before either applies, so a plain
    /// `nodes.len()` read would hand the second one the first one's ids — this
    /// counter is what keeps every clone at the slot its id names.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`NodeId` is a `u32`, so a graph can never hold more nodes than a `u32` counts"
    )]
    pub(crate) fn next_node_id(&mut self) -> NodeId {
        self.allocators.next_node = self.allocators.next_node.max(self.graph.nodes.len() as u32);
        NodeId::new(self.allocators.reserve_nodes(1))
    }

    pub(crate) fn register_seed_edge(&mut self, edge: EdgeId, node: NodeId) {
        self.seed_edges.insert(edge, node);
    }

    /// Where a token on this edge is headed. Covers seed edges as well as real
    /// ones.
    pub(crate) fn edge_target(&self, edge: EdgeId) -> Option<NodeId> {
        self.graph
            .edge(edge)
            .map(|e| e.to)
            .or_else(|| self.seed_edges.get(&edge).copied())
    }

    /// Edges that count toward a node's join right now: its real incoming
    /// edges, minus those from superseded sources, plus any seed edge aimed
    /// at it.
    pub(crate) fn incoming_edges(&self, node: NodeId) -> Vec<EdgeId> {
        let mut ids: Vec<EdgeId> = self
            .graph
            .nodes
            .iter()
            .filter(|n| !self.is_superseded(n.id))
            .flat_map(|n| n.routing.edges())
            .filter(|e| e.to == node)
            .map(|e| e.id)
            .collect();
        ids.extend(
            self.seed_edges
                .iter()
                .filter(|(_, target)| **target == node)
                .map(|(edge, _)| *edge),
        );
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub(crate) fn has_fired(&self, key: (NodeId, Generation)) -> bool {
        self.fired.contains(&key)
    }

    pub(crate) fn mark_fired(&mut self, key: (NodeId, Generation)) {
        self.fired.insert(key);
    }

    pub(crate) fn store_token(&mut self, node: NodeId, token: Token) {
        self.pending
            .entry(node)
            .or_default()
            .entry(token.generation)
            .or_default()
            .insert(token.edge, token);
    }

    pub(crate) fn tokens_for(&self, key: (NodeId, Generation)) -> Option<&BTreeMap<EdgeId, Token>> {
        self.pending.get(&key.0).and_then(|g| g.get(&key.1))
    }

    pub(crate) fn take_tokens(&mut self, key: (NodeId, Generation)) -> Vec<Token> {
        let Some(generations) = self.pending.get_mut(&key.0) else {
            return Vec::new();
        };
        let taken = generations
            .remove(&key.1)
            .map(|m| m.into_values().collect())
            .unwrap_or_default();
        if generations.is_empty() {
            self.pending.remove(&key.0);
        }
        taken
    }

    /// Drop every pending token aimed at one of these nodes.
    pub(crate) fn drop_tokens_for_nodes(&mut self, nodes: &BTreeSet<NodeId>) {
        self.pending.retain(|node, _| !nodes.contains(node));
        self.deferred.retain(|(node, _)| !nodes.contains(node));
    }

    pub(crate) fn drop_all_tokens(&mut self) {
        self.pending.clear();
        self.deferred.clear();
    }

    pub(crate) fn bump_firing_count(&mut self, node: NodeId) {
        *self.firing_counts.entry(node).or_insert(0) += 1;
    }

    pub(crate) fn insert_firing(&mut self, firing: Firing) {
        let node = firing.node;
        let previous = self.live.insert(firing.id, firing);
        debug_assert!(previous.is_none());
        if previous.is_none() {
            self.adjust_batch_live(node, 1);
        }
    }

    pub(crate) fn firing_mut(&mut self, id: FiringId) -> Option<&mut Firing> {
        self.live.get_mut(&id)
    }

    pub(crate) fn remove_firing(&mut self, id: FiringId) -> Option<Firing> {
        let firing = self.live.remove(&id)?;
        self.adjust_batch_live(firing.node, -1);
        Some(firing)
    }

    /// Record a firing's final outcome: history, the node's run-context record,
    /// and the `kv` merge, in that order.
    ///
    /// This is the only write path into [`RunContext`], and it runs inside
    /// `apply`, in event order. Intermediate retry attempts never reach it.
    pub(crate) fn record_outcome(&mut self, record: FiringRecord) {
        self.run.record(record.name.clone(), NodeRecord {
            status:     record.outcome.status.clone(),
            output:     record.outcome.output.clone(),
            generation: record.generation,
            attempts:   record.attempt.raw(),
        });
        self.run.merge(&record.outcome.context_updates);
        if matches!(record.outcome.status, Status::Cancelled) {
            self.cancelled_outcomes.insert(record.firing);
        } else {
            self.cancelled_outcomes.remove(&record.firing);
        }
        self.history.push(record);
    }

    pub(crate) fn clone_bindings_for(&self, node: NodeId) -> Option<&BTreeMap<SmolStr, Value>> {
        self.node_runtime
            .get(node.index())
            .and_then(|runtime| runtime.clone_bindings.as_ref())
    }

    /// The `for_each` item index a node was cloned for, when it is an
    /// expansion clone: its position in the items array, the `index` bound
    /// inside the clone. `None` for every other node.
    pub fn clone_index(&self, node: NodeId) -> Option<u32> {
        self.clone_bindings_for(node)?
            .get("index")?
            .as_u64()
            .and_then(|index| u32::try_from(index).ok())
    }

    /// The `for_each` item a node was cloned for, when it is an expansion
    /// clone: the `item` bound inside the clone. `None` for every other node.
    pub fn clone_item(&self, node: NodeId) -> Option<&Value> {
        self.clone_bindings_for(node)?.get("item")
    }

    pub(crate) fn supersede(&mut self, node: NodeId) {
        if let Some(runtime) = self.node_runtime.get_mut(node.index()) {
            runtime.superseded = true;
        }
    }

    pub(crate) fn add_cancel_scope(
        &mut self,
        id: CancelScopeId,
        parent: CancelScopeId,
        nodes: BTreeSet<NodeId>,
    ) {
        self.cancel_scopes.insert(id, CancelScope {
            id,
            parent: Some(parent),
            children: Vec::new(),
            nodes,
            cancelled: false,
            killed: false,
        });
        if let Some(p) = self.cancel_scopes.get_mut(&parent) {
            p.children.push(id);
        }
    }

    /// Give each declared group its own child of the containing scope.
    pub(crate) fn add_cancel_groups(&mut self, parent: CancelScopeId, nodes: &BTreeSet<NodeId>) {
        let mut groups: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
        for id in nodes {
            if let Some(anchor) = self.graph.node(*id).and_then(|node| node.cancel_group) {
                groups.entry(anchor).or_default().insert(*id);
            }
        }
        for members in groups.into_values() {
            let scope = self.next_cancel_scope_id();
            for id in &members {
                self.node_runtime[id.index()].cancel_scope = scope;
            }
            self.add_cancel_scope(scope, parent, members);
        }
    }

    pub(crate) fn cancel_scope_of(&self, node: NodeId) -> CancelScopeId {
        self.node_runtime
            .get(node.index())
            .map_or(CancelScopeId::ROOT, |runtime| runtime.cancel_scope)
    }

    /// A scope and everything nested inside it.
    pub(crate) fn cancel_scope_closure(&self, root: CancelScopeId) -> BTreeSet<CancelScopeId> {
        let mut out = BTreeSet::new();
        let mut queue = VecDeque::from([root]);
        while let Some(id) = queue.pop_front() {
            if !out.insert(id) {
                continue;
            }
            if let Some(scope) = self.cancel_scopes.get(&id) {
                queue.extend(scope.children.iter().copied());
            }
        }
        out
    }

    pub(crate) fn mark_scope_cancelled(&mut self, id: CancelScopeId) {
        if let Some(scope) = self.cancel_scopes.get_mut(&id) {
            scope.cancelled = true;
        }
    }

    pub(crate) fn is_scope_cancelled(&self, id: CancelScopeId) -> bool {
        self.cancel_scopes
            .get(&id)
            .is_some_and(|scope| scope.cancelled)
    }

    /// Whether any scope on the node's chain, innermost to root, satisfies
    /// `pred`.
    fn any_enclosing_scope(&self, node: NodeId, pred: impl Fn(&CancelScope) -> bool) -> bool {
        let mut current = Some(self.cancel_scope_of(node));
        while let Some(id) = current {
            let Some(scope) = self.cancel_scopes.get(&id) else {
                return false;
            };
            if pred(scope) {
                return true;
            }
            current = scope.parent;
        }
        false
    }

    /// Whether the node sits in a cancelled scope.
    pub(crate) fn is_node_cancelled(&self, node: NodeId) -> bool {
        self.cancelled || self.any_enclosing_scope(node, |scope| scope.cancelled)
    }

    /// Mark a scope killed. Killed implies cancelled.
    pub(crate) fn mark_scope_killed(&mut self, id: CancelScopeId) {
        if let Some(scope) = self.cancel_scopes.get_mut(&id) {
            scope.cancelled = true;
            scope.killed = true;
        }
    }

    /// Whether the node sits in a killed scope. A root kill marks the root
    /// scope, and every node's scope chain ends there, so no separate run
    /// flag is needed.
    ///
    /// Public because a resumed driver needs the stop tier: a cancelling firing
    /// is finished rather than re-spawned, and which tier stopped it comes from
    /// the replayed state — killed scopes are in the log.
    pub fn is_node_killed(&self, node: NodeId) -> bool {
        self.any_enclosing_scope(node, |scope| scope.killed)
    }

    /// Whether this firing's recorded final outcome is `Cancelled`. Seed tokens
    /// carry `FiringId(0)`, which no record ever uses.
    pub(crate) fn outcome_was_cancelled(&self, firing: FiringId) -> bool {
        self.cancelled_outcomes.contains(&firing)
    }

    pub(crate) fn add_retry_tombstone(&mut self, firing: FiringId) {
        self.retry_tombstones.insert(firing);
    }

    /// Consume the tombstone for a settled awaiting-retry firing, if one
    /// exists.
    pub(crate) fn take_retry_tombstone(&mut self, firing: FiringId) -> bool {
        self.retry_tombstones.remove(&firing)
    }

    pub(crate) fn nodes_in_scopes(&self, scopes: &BTreeSet<CancelScopeId>) -> BTreeSet<NodeId> {
        let mut nodes = BTreeSet::new();
        for id in scopes {
            if let Some(scope) = self.cancel_scopes.get(id) {
                nodes.extend(scope.nodes.iter().copied());
            }
        }
        nodes
    }

    pub(crate) fn push_splice(&mut self, splice: AppliedSplice) {
        debug_assert_eq!(splice.batch.0 as usize, self.splices.len());
        self.splices.push(splice);
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "`SpliceBatchId` is a `u32`, so a run can never apply more batches than a \
                  `u32` counts"
    )]
    pub(crate) fn next_splice_batch(&self) -> SpliceBatchId {
        SpliceBatchId(self.splices.len() as u32)
    }

    /// Append a spliced node to the live graph and record its runtime facts in
    /// one motion, so `graph.nodes` and `node_runtime` can never grow
    /// independently.
    ///
    /// The id check stays a `debug_assert`: preparation allocated the id
    /// against this exact graph length, and this method being the only
    /// growth path is what makes the two vectors impossible to desync.
    pub(crate) fn push_spliced_node(
        &mut self,
        node: Node,
        cancel_scope: CancelScopeId,
        batch: SpliceBatchId,
        clone_bindings: Option<BTreeMap<SmolStr, Value>>,
    ) {
        debug_assert_eq!(node.id.index(), self.graph.nodes.len());
        debug_assert_eq!(self.graph.nodes.len(), self.node_runtime.len());
        self.graph.body.nodes.push(node);
        self.node_runtime.push(NodeRuntime {
            cancel_scope,
            batch: Some(batch),
            clone_bindings,
            superseded: false,
        });
    }

    pub(crate) fn splice_for_node(&self, node: NodeId) -> Option<&AppliedSplice> {
        let batch = self.node_runtime.get(node.index())?.batch?;
        self.splices.get(batch.0 as usize)
    }

    fn adjust_batch_live(&mut self, node: NodeId, change: i32) {
        let Some(batch) = self
            .node_runtime
            .get(node.index())
            .and_then(|runtime| runtime.batch)
        else {
            return;
        };
        let splice = self
            .splices
            .get_mut(batch.0 as usize)
            .expect("node references an applied splice batch");
        if change > 0 {
            splice.live_count += change.cast_unsigned();
        } else {
            let decrease = change.unsigned_abs();
            debug_assert!(splice.live_count >= decrease);
            splice.live_count -= decrease;
        }
    }

    /// A copy of the id allocators for a preparation transaction. Preparation
    /// advances the copy; a rejected transaction drops it, so canonical state
    /// never moves — not even allocator movement leaks.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "`NodeId` is a `u32`, so a graph can never hold more nodes than a `u32` counts"
    )]
    pub(crate) fn allocator_snapshot(&self) -> Allocators {
        let mut allocators = self.allocators;
        allocators.next_node = allocators.next_node.max(self.graph.nodes.len() as u32);
        allocators
    }

    /// Commit a prepared transaction's allocator movement.
    pub(crate) fn adopt_allocators(&mut self, allocators: Allocators) {
        debug_assert!(allocators.next_node >= self.allocators.next_node);
        debug_assert!(allocators.next_edge >= self.allocators.next_edge);
        debug_assert!(allocators.next_cancel_scope >= self.allocators.next_cancel_scope);
        self.allocators = allocators;
    }

    /// Every admission with parked tokens or a `max_parallel` deferral: the
    /// retractable candidates. A pending key can have no live firing and no
    /// final record — firing consumes its tokens first — so no further filter
    /// is needed.
    pub(crate) fn pending_admission_keys(&self) -> Vec<AdmissionKey> {
        let mut keys: BTreeSet<AdmissionKey> = self
            .pending
            .iter()
            .flat_map(|(node, generations)| {
                generations
                    .iter()
                    .filter(|(_, tokens)| !tokens.is_empty())
                    .map(move |(generation, _)| AdmissionKey {
                        node:       *node,
                        generation: *generation,
                    })
            })
            .collect();
        keys.extend(self.deferred.iter().map(|(node, generation)| AdmissionKey {
            node:       *node,
            generation: *generation,
        }));
        keys.into_iter().collect()
    }

    /// Whether this node was admitted in more than one generation. The
    /// `depends_on` ambiguity rule only needs this Boolean, so stop at the
    /// first generation that differs from the first one seen.
    pub(crate) fn has_multiple_admission_generations(&self, node: NodeId) -> bool {
        let mut generations = self
            .fired
            .iter()
            .filter_map(|(n, generation)| (*n == node).then_some(*generation))
            .chain(
                self.pending
                    .get(&node)
                    .into_iter()
                    .flat_map(|pending| pending.keys().copied()),
            )
            .chain(
                self.deferred
                    .iter()
                    .filter_map(|(n, generation)| (*n == node).then_some(*generation)),
            )
            .chain(
                self.retracted_admissions
                    .iter()
                    .filter_map(|key| (key.node == node).then_some(key.generation)),
            );
        let Some(first) = generations.next() else {
            return false;
        };
        generations.any(|generation| generation != first)
    }

    /// Retract admissions: drop their parked tokens and deferred joins, and
    /// remember the keys so later tokens for them are swallowed. Running
    /// firings, history, and future generations are untouched.
    pub(crate) fn retract_admissions(&mut self, keys: &[AdmissionKey]) {
        let keys: BTreeSet<AdmissionKey> = keys.iter().copied().collect();
        for key in &keys {
            if let Some(generations) = self.pending.get_mut(&key.node) {
                generations.remove(&key.generation);
                if generations.is_empty() {
                    self.pending.remove(&key.node);
                }
            }
        }
        self.deferred.retain(|(node, generation)| {
            !keys.contains(&AdmissionKey {
                node:       *node,
                generation: *generation,
            })
        });
        self.retracted_admissions.extend(keys);
    }

    /// Whether this exact `(node, generation)` admission was retracted, so a
    /// late token for it is swallowed.
    pub(crate) fn is_admission_retracted(&self, node: NodeId, generation: Generation) -> bool {
        self.retracted_admissions
            .contains(&AdmissionKey { node, generation })
    }

    pub(crate) fn defer(&mut self, key: (NodeId, Generation)) {
        if !self.deferred.contains(&key) {
            self.deferred.push_back(key);
        }
    }

    pub(crate) fn take_deferred(&mut self) -> Vec<(NodeId, Generation)> {
        self.deferred.drain(..).collect()
    }

    /// Mark a scope held. Returns true the first time, when the host must
    /// acquire it.
    pub(crate) fn acquire_scope(&mut self, scope: ScopeId) -> bool {
        self.held_scopes.insert(scope)
    }

    /// Scopes with a live firing, a pending token or a deferred join still in
    /// them.
    fn needed_scopes(&self) -> BTreeSet<ScopeId> {
        let mut needed: BTreeSet<ScopeId> = self.live.values().map(|f| f.scope).collect();
        needed.extend(
            self.pending_routing
                .values()
                .map(|pending| pending.node.scope),
        );
        let waiting = self
            .pending
            .iter()
            .filter(|(_, generations)| generations.values().any(|t| !t.is_empty()))
            .map(|(node, _)| *node)
            .chain(self.deferred.iter().map(|(node, _)| *node));
        for node in waiting {
            if let Some(nd) = self.graph.node(node) {
                needed.insert(nd.scope);
            }
        }
        needed
    }

    /// Drop the scopes nothing needs any more, and say which they were.
    pub(crate) fn release_unneeded_scopes(&mut self) -> Vec<ScopeId> {
        let needed = self.needed_scopes();
        let released: Vec<ScopeId> = self.held_scopes.difference(&needed).copied().collect();
        for scope in &released {
            self.held_scopes.remove(scope);
        }
        released
    }

    /// Terminal release: drop every held scope, and say which they were.
    /// Nothing can need an environment after `FinishExecution`, and a finished
    /// serialized state must claim no resources — a token parked at an
    /// unsatisfiable join no longer holds its environment past the end of
    /// the run.
    pub(crate) fn release_all_scopes(&mut self) -> Vec<ScopeId> {
        let released: Vec<ScopeId> = self.held_scopes.iter().copied().collect();
        self.held_scopes.clear();
        released
    }

    /// Scopes the host is currently holding.
    pub fn held_scopes(&self) -> impl Iterator<Item = ScopeId> + '_ {
        self.held_scopes.iter().copied()
    }
}

/// A synthesized outcome for a node that never ran.
pub(crate) fn synthetic(status: Status) -> Outcome {
    Outcome::new(status, Value::Null)
}
