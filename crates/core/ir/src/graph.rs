//! The token-flow graph: nodes, explicit routing, joins, scopes.
//!
//! Every structure that carries a graph-structure id takes the id-space marker
//! `S` (default [`Live`], so ordinary call sites never see it). A
//! [`GraphFragment`](crate::GraphFragment) reuses these same types over
//! [`Local`](crate::Local) ids rather than declaring a parallel mirror family.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::ops::Deref;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use crate::container::{ContainerOptions, ServiceOptions};
use crate::expr::ExprTable;
use crate::flow::{FailureClass, Outcome, Status, StatusKind, UnderlyingFailure};
use crate::ids::{Attempt, EdgeId, ExprId, Live, NodeId, ScopeId, StepKindId};
use crate::splice::SplicePolicy;

// ── Guards & edges ────────────────────────────────────────────────────────

/// Condition on an edge arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Guard<S = Live> {
    /// Always passes. May only appear as a group's final arm (invariant 2).
    Always,
    /// Boolean expression over the completing node's outcome and contexts.
    Expr(ExprId<S>),
}

/// One arm of a select group: where a token goes, and when.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge<S = Live> {
    pub id:         EdgeId<S>,
    pub to:         NodeId<S>,
    pub guard:      Guard<S>,
    /// Payload for the emitted token; `None` means the source outcome's
    /// `output`.
    pub map:        Option<ExprId<S>>,
    /// Back edge: traversal increments the token's `Generation`.
    /// Every cycle must contain at least one (invariant 1).
    pub back:       bool,
    /// Relative weight used by weighted selection policies.
    #[serde(default = "default_edge_weight")]
    pub weight:     u32,
    /// Stable name that lowering and routing middleware can address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label:      Option<SmolStr>,
    /// What selecting this edge does to the current engine instance.
    #[serde(default)]
    pub transition: EdgeTransition,
}

const fn default_edge_weight() -> u32 {
    1
}

/// What selecting an edge does after routing is resolved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeTransition {
    /// Emit an ordinary token into this execution.
    #[default]
    Continue,
    /// Finish this execution and request a successor at the edge target.
    Restart,
}

impl<S> Edge<S> {
    /// An unconditional edge carrying the source output.
    pub fn always(id: EdgeId<S>, to: NodeId<S>) -> Self {
        Self {
            id,
            to,
            guard: Guard::Always,
            map: None,
            back: false,
            weight: 1,
            label: None,
            transition: EdgeTransition::Continue,
        }
    }

    /// A guarded edge carrying the source output.
    pub fn when(id: EdgeId<S>, to: NodeId<S>, guard: ExprId<S>) -> Self {
        Self {
            id,
            to,
            guard: Guard::Expr(guard),
            map: None,
            back: false,
            weight: 1,
            label: None,
            transition: EdgeTransition::Continue,
        }
    }

    #[must_use]
    pub fn with_map(mut self, map: ExprId<S>) -> Self {
        self.map = Some(map);
        self
    }

    /// Mark this edge as a back edge, so crossing it bumps the generation.
    #[must_use]
    pub fn with_back(mut self) -> Self {
        self.back = true;
        self
    }

    #[must_use]
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight;
        self
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn with_transition(mut self, transition: EdgeTransition) -> Self {
        self.transition = transition;
        self
    }
}

// ── Routing: AND of XORs ──────────────────────────────────────────────────

/// How one routing group chooses an eligible edge.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum SelectionPolicy<S = Live> {
    /// Arms are tried in order and the first passing arm wins.
    #[default]
    FirstMatch,
    /// The first tier with an eligible candidate wins.
    Tiered(Vec<Tier<S>>),
}

/// One priority tier in a [`SelectionPolicy::Tiered`] policy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tier<S = Live> {
    pub candidates: Vec<Candidate<S>>,
    pub pick:       PickPolicy,
}

/// One edge's eligibility inside a priority tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate<S = Live> {
    pub edge: EdgeId<S>,
    pub when: Guard<S>,
    /// Used only by [`PickPolicy::LowestRankThenArmOrder`]. A null result
    /// excludes this candidate from the tier.
    pub rank: Option<ExprId<S>>,
}

/// How an active tier chooses from its eligible candidates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PickPolicy {
    /// Candidate order.
    #[default]
    First,
    /// Highest edge weight, then lexical target-node name.
    HighestWeightThenLexical,
    /// Weighted random choice. The host records the draw.
    WeightedRandom,
    /// Lowest evaluated numeric rank, then candidate order.
    LowestRankThenArmOrder,
}

/// One XOR-select: **at most one** token is emitted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingGroup<S = Live> {
    #[serde(default = "default_selection_policy")]
    pub policy:      SelectionPolicy<S>,
    /// Declared edges. Policies may refer only to these ids.
    pub arms:        Vec<Edge<S>>,
    pub fallthrough: Fallthrough,
}

fn default_selection_policy<S>() -> SelectionPolicy<S> {
    SelectionPolicy::FirstMatch
}

impl<S> RoutingGroup<S> {
    /// A group that emits nothing when no arm matches (OR-split, loop exit).
    pub fn new(arms: Vec<Edge<S>>) -> Self {
        Self {
            policy: SelectionPolicy::FirstMatch,
            arms,
            fallthrough: Fallthrough::NoEmit,
        }
    }

    /// A group that must match: used by frontends requiring totality.
    pub fn total(arms: Vec<Edge<S>>) -> Self {
        Self {
            policy: SelectionPolicy::FirstMatch,
            arms,
            fallthrough: Fallthrough::Error,
        }
    }

    #[must_use]
    pub fn with_policy(mut self, policy: SelectionPolicy<S>) -> Self {
        self.policy = policy;
        self
    }
}

/// Compatibility name for the original routing-group API.
pub type SelectGroup<S = Live> = RoutingGroup<S>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fallthrough {
    /// No arm matched -> emit nothing.
    #[default]
    NoEmit,
    /// No arm matched -> run error.
    Error,
}

/// A node's routing: an AND of XORs.
///
/// `groups.len() == 1` is pure selection, the default. `groups.len() > 1` is an
/// explicit fan-out: the groups emit concurrently. Fan-out is never implicit —
/// it takes writing more than one group.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Routing<S = Live> {
    pub groups: Vec<RoutingGroup<S>>,
}

impl<S> Routing<S> {
    /// Terminal node: no outgoing tokens.
    pub fn terminal() -> Self {
        Self { groups: Vec::new() }
    }

    /// One group of one unconditional arm — a plain `next:`.
    pub fn next(edge: Edge<S>) -> Self {
        Self {
            groups: vec![SelectGroup::new(vec![edge])],
        }
    }

    /// One group of several guarded arms — pick exactly one successor.
    pub fn select(arms: Vec<Edge<S>>) -> Self {
        Self {
            groups: vec![SelectGroup::new(arms)],
        }
    }

    /// One group per edge — an explicit AND-split.
    pub fn fan_out(edges: Vec<Edge<S>>) -> Self {
        Self {
            groups: edges
                .into_iter()
                .map(|e| RoutingGroup::new(vec![e]))
                .collect(),
        }
    }

    pub fn groups(groups: Vec<RoutingGroup<S>>) -> Self {
        Self { groups }
    }

    pub fn edges(&self) -> impl Iterator<Item = &Edge<S>> {
        self.groups.iter().flat_map(|g| g.arms.iter())
    }
}

// ── Joins ─────────────────────────────────────────────────────────────────

/// How incoming tokens are matched into a firing. Tokens are matched per
/// `(node, generation)`; incoming edges are counted **as of firing time**, so
/// edges spliced in by an expansion are included.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinPolicy {
    /// A token present on every incoming edge of the same generation.
    #[default]
    All,
    /// The first token fires the node; later same-generation tokens are
    /// dropped.
    Any,
    /// Tokens on `n` distinct incoming edges of the same generation.
    Quorum { n: u32 },
}

// ── Nodes ─────────────────────────────────────────────────────────────────

/// What a node runs, and with what configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepRef {
    pub kind:   StepKindId,
    /// May embed unresolved expression placeholders in HIR (see [`Expansion`]).
    pub config: Value,
}

impl StepRef {
    pub fn new(kind: impl Into<StepKindId>, config: Value) -> Self {
        Self {
            kind: kind.into(),
            config,
        }
    }
}

/// Who enforces a node's per-attempt `timeout`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutPolicy {
    /// The driver arms the attempt timer and cancels the step when it
    /// expires. The timer counts active work only: while the step has a
    /// question pending with the host, the clock stops and resumes with the
    /// remaining time once every pending question is answered.
    #[default]
    ExecutorEnforced,
    /// The step consumes the timeout itself (a command deadline sent to the
    /// sandbox, an agent's own turn deadline, a human gate's answer
    /// deadline). The driver arms no timer of its own around it.
    HandlerManaged,
}

impl TimeoutPolicy {
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's `skip_serializing_if` passes the field by reference"
    )]
    fn is_default(&self) -> bool {
        *self == Self::ExecutorEnforced
    }
}

/// Hard limits on a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Hard cap on **firings** of this node across all generations. Must be at
    /// least 1. Retries do not count: a firing that takes four attempts is
    /// still one firing.
    pub max_firings:    u32,
    /// Applies **per attempt**, not per firing. Node-total wall clock is not a
    /// thing the core tracks.
    pub timeout:        Duration,
    /// Who enforces `timeout`. The default keeps the driver's timer.
    #[serde(default, skip_serializing_if = "TimeoutPolicy::is_default")]
    pub timeout_policy: TimeoutPolicy,
}

impl Budget {
    pub const UNBOUNDED_FIRINGS: u32 = u32::MAX;

    pub fn new(max_firings: u32, timeout: Duration) -> Self {
        Self {
            max_firings,
            timeout,
            timeout_policy: TimeoutPolicy::ExecutorEnforced,
        }
    }

    /// The same budget, enforced by the step instead of the driver.
    #[must_use]
    pub fn with_timeout_policy(mut self, policy: TimeoutPolicy) -> Self {
        self.timeout_policy = policy;
        self
    }

    /// A node that fires once, with a one-hour ceiling.
    pub fn once() -> Self {
        Self::new(1, Duration::from_secs(3600))
    }

    /// A node inside a loop: capped at `max_firings` iterations.
    pub fn looped(max_firings: u32) -> Self {
        Self::new(max_firings, Duration::from_secs(3600))
    }

    /// Whether the firing cap is finite (invariant 4 for looped nodes).
    pub fn is_finite(&self) -> bool {
        self.max_firings < Self::UNBOUNDED_FIRINGS
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::once()
    }
}

/// How many times a node may be attempted, and when.
///
/// A retry is not a loop iteration: it advances [`Attempt`], never
/// [`Generation`](crate::Generation). Attempt counters never carry across
/// firings, so a later generation retries from scratch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// 1 means no retries.
    pub max_attempts:  NonZeroU32,
    pub backoff:       Backoff,
    pub retry_on:      RetryOn,
    pub on_exhaustion: Exhaustion,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::none()
    }
}

impl RetryPolicy {
    /// One attempt, no retries.
    pub fn none() -> Self {
        Self {
            max_attempts:  NonZeroU32::new(1).expect("1 is non-zero"),
            backoff:       Backoff::default(),
            retry_on:      RetryOn::default(),
            on_exhaustion: Exhaustion::Fail,
        }
    }

    /// At most `n` attempts in total. `n == 0` is read as 1.
    pub fn attempts(n: u32) -> Self {
        Self {
            max_attempts: NonZeroU32::new(n.max(1)).expect("max(1) is non-zero"),
            ..Self::none()
        }
    }

    #[must_use]
    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }

    #[must_use]
    pub fn with_retry_on(mut self, retry_on: RetryOn) -> Self {
        self.retry_on = retry_on;
        self
    }

    /// Exhausting the attempts yields `PartialSuccess` instead of failing.
    #[must_use]
    pub fn accepting_partial(mut self) -> Self {
        self.on_exhaustion = Exhaustion::AcceptPartial;
        self
    }

    /// Whether another attempt is left after `attempt` has finished.
    pub fn has_attempt_after(&self, attempt: Attempt) -> bool {
        attempt.raw() < self.max_attempts.get()
    }

    /// The exhaustion policy applied to `attempt`'s returned `outcome`: a
    /// retryable failure with no attempt left becomes a `PartialSuccess`
    /// under [`Exhaustion::AcceptPartial`], the real failure — a timeout
    /// included — kept in `underlying` so the log never records a clean
    /// success for something that failed; every other outcome stands. The
    /// driver applies this to every returned attempt before a host prepares
    /// the result, so the record is what the host saw; the engine applies
    /// it only to the failure it makes itself when it rejects an attempt's
    /// splices.
    #[must_use]
    pub fn finalize(&self, attempt: Attempt, outcome: Outcome) -> Outcome {
        let exhausted = self.should_retry(&outcome.status) && !self.has_attempt_after(attempt);
        if self.on_exhaustion != Exhaustion::AcceptPartial || !exhausted {
            return outcome;
        }
        let underlying = UnderlyingFailure::of(&outcome.status);
        Outcome {
            status: Status::PartialSuccess { underlying },
            ..outcome
        }
    }

    /// Whether this status is one the policy retries.
    ///
    /// Success-like statuses are never retried; that test goes through
    /// [`Status::is_success_like`], the single classification point.
    pub fn should_retry(&self, status: &Status) -> bool {
        if status.is_success_like() {
            return false;
        }
        if self.retry_on.statuses.contains(&status.kind()) {
            return true;
        }
        status.failure_info().is_some_and(|info| {
            !info.class.is_empty() && self.retry_on.failure_classes.contains(&info.class)
        })
    }

    /// The delay before the attempt following `failed_attempt`:
    /// `initial * factor^(n - 1)`, capped at `max`.
    ///
    /// Computed by repeated multiplication rather than `powi`, so the result is
    /// bit-identical on every replay. The driver adds jitter and does the
    /// waiting; the core never sees a clock or an RNG.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the delay is clamped non-negative and never exceeds the configured cap, \
                  and a float-to-integer cast saturates, so an out-of-range cap yields the \
                  longest representable delay rather than a wrapped one"
    )]
    pub fn base_delay(&self, failed_attempt: Attempt) -> Duration {
        let mut nanos = self.backoff.initial.as_nanos() as f64;
        let cap = self.backoff.max.as_nanos() as f64;
        for _ in 1..failed_attempt.raw().max(1) {
            nanos *= self.backoff.factor;
            if nanos >= cap {
                return self.backoff.max;
            }
        }
        if nanos >= cap {
            return self.backoff.max;
        }
        Duration::from_nanos(nanos.max(0.0) as u64)
    }
}

/// Exponential backoff between attempts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Backoff {
    pub initial: Duration,
    /// Exponential factor; `1.0` is a fixed delay.
    pub factor:  f64,
    pub max:     Duration,
    /// Applied by the driver, never by the core — jitter is randomness, and the
    /// core has none.
    pub jitter:  bool,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            factor:  2.0,
            max:     Duration::from_secs(60),
            jitter:  true,
        }
    }
}

/// Which failed outcomes are worth another attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryOn {
    pub statuses:        Vec<StatusKind>,
    /// Matched against [`FailureInfo::class`](crate::FailureInfo::class).
    pub failure_classes: Vec<FailureClass>,
}

impl Default for RetryOn {
    fn default() -> Self {
        Self {
            statuses:        vec![StatusKind::Failure, StatusKind::TimedOut],
            failure_classes: Vec::new(),
        }
    }
}

impl RetryOn {
    pub fn statuses(statuses: Vec<StatusKind>) -> Self {
        Self {
            statuses,
            failure_classes: Vec::new(),
        }
    }

    pub fn classes<C: Clone + Into<FailureClass>>(classes: &[C]) -> Self {
        Self {
            statuses:        Vec::new(),
            failure_classes: classes.iter().cloned().map(Into::into).collect(),
        }
    }

    #[must_use]
    pub fn with_classes<C: Clone + Into<FailureClass>>(mut self, classes: &[C]) -> Self {
        self.failure_classes = classes.iter().cloned().map(Into::into).collect();
        self
    }
}

/// What happens when the attempts run out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exhaustion {
    /// The last attempt's outcome stands.
    #[default]
    Fail,
    /// The failure becomes `PartialSuccess`, carrying the real failure in
    /// `underlying`.
    AcceptPartial,
}

/// Parallel `for_each` / matrix.
///
/// Sequential `for_each` is **not** an expansion: it desugars to a cycle over
/// back edges and generations. The engine has no loop primitive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Expansion<S = Live> {
    ForEach {
        /// Evaluates to an array at runtime; one clone per element, with `item`
        /// and `index` bound into the clone's expression context.
        items:        ExprId<S>,
        target:       ExpandTarget<S>,
        /// Scheduler admission control across the spliced clones.
        max_parallel: Option<u32>,
        /// The first clone failure cancels sibling clones, via the splice's
        /// cancel scope.
        fail_fast:    bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpandTarget<S = Live> {
    /// Clone this node only.
    Node,
    /// Clone the subgraph between `entry` and `exit` (loop bodies, matrix
    /// jobs).
    Subgraph { entry: NodeId<S>, exit: NodeId<S> },
}

/// A unit of work in the graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node<S = Live> {
    pub id:                NodeId<S>,
    pub name:              SmolStr,
    pub scope:             ScopeId<S>,
    pub step:              StepRef,
    pub join:              JoinPolicy,
    /// Precondition evaluated in the node's own context. False means the node
    /// completes `Skipped` without executing; routing still runs, so `always()`
    /// and `failure()` guards downstream still see it.
    pub precondition:      Option<ExprId<S>>,
    pub routing:           Routing<S>,
    pub budget:            Budget,
    /// How many attempts this node gets. The default is one.
    pub retry:             RetryPolicy,
    /// The node may fire inside a cancelled scope (§5): its precondition is
    /// evaluated, and absent-or-true means it runs for real. Unset — the
    /// default — means the node completes `Cancelled` without evaluating
    /// anything, so un-marked work can never restart after a cancel,
    /// whatever its gates say. Kill admits nothing, flag or no flag.
    #[serde(default)]
    pub run_on_cancel:     bool,
    /// Nodes with the same anchor form an independently cancellable group.
    /// The anchor names itself. Expansions remap the anchor with the nodes,
    /// so each clone has its own group. Outcome splices inherit their owner's
    /// cancellation scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_group:      Option<NodeId<S>>,
    /// This node's failure is control flow, not a run failure: under
    /// [`Completion::AnyFailure`] the status fold passes over a failed record
    /// here, and a failed clone does not trigger its splice's fail-fast cancel.
    /// The record itself still says `failure` — routing, status guards and
    /// `run.failed` see it unchanged. Set by frontends for constructs like
    /// GHA's job-level `continue-on-error`.
    #[serde(default)]
    pub tolerates_failure: bool,
    /// What outcome-driven splices this node's firings may request (§14 made
    /// real). `Deny` — the default — rejects every request as
    /// `invalid_splice`; anything higher authorizes operations up to it,
    /// per the total order on [`SplicePolicy`].
    #[serde(default)]
    pub splice_policy:     SplicePolicy,
    /// Frontend-supplied metadata: display label, source span, classes. Opaque
    /// to the engine; `apply` never reads it. Hosts and observers render
    /// with it.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta:              Value,
    /// HIR only; lowered away before execution.
    pub expand:            Option<Expansion<S>>,
}

impl<S> Node<S> {
    pub fn new(id: NodeId<S>, name: &str, scope: ScopeId<S>, step: StepRef) -> Self {
        Self {
            id,
            name: SmolStr::new(name),
            scope,
            step,
            join: JoinPolicy::All,
            precondition: None,
            routing: Routing::terminal(),
            budget: Budget::once(),
            retry: RetryPolicy::none(),
            run_on_cancel: false,
            cancel_group: None,
            tolerates_failure: false,
            splice_policy: SplicePolicy::Deny,
            meta: Value::Null,
            expand: None,
        }
    }

    #[must_use]
    pub fn with_join(mut self, join: JoinPolicy) -> Self {
        self.join = join;
        self
    }

    #[must_use]
    pub fn with_precondition(mut self, expr: ExprId<S>) -> Self {
        self.precondition = Some(expr);
        self
    }

    #[must_use]
    pub fn with_routing(mut self, routing: Routing<S>) -> Self {
        self.routing = routing;
        self
    }

    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Opt in to firing inside a cancelled scope (§5).
    #[must_use]
    pub fn with_run_on_cancel(mut self) -> Self {
        self.run_on_cancel = true;
        self
    }

    /// Grant this node's firings splice authority. The default is `Deny`.
    #[must_use]
    pub fn with_splice_policy(mut self, policy: SplicePolicy) -> Self {
        self.splice_policy = policy;
        self
    }

    /// Attach frontend metadata. The core carries it and never reads it.
    #[must_use]
    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = meta;
        self
    }

    #[must_use]
    pub fn with_expansion(mut self, expand: Expansion<S>) -> Self {
        self.expand = Some(expand);
        self
    }
}

// ── Scopes ────────────────────────────────────────────────────────────────

/// Either a literal value or an expression resolved at firing time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ExprOrValue<S = Live> {
    Value(Value),
    Expr(ExprId<S>),
}

/// Where a scope's steps run, plus the placement hints that go with it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSpec {
    pub target:       RuntimeTarget,
    /// Uninterpreted placement labels, populated by frontends from whatever
    /// their format calls them (`runs-on`, agent tags, an instance class).
    ///
    /// The core never reads these. Whichever executor runs the scope maps the
    /// labels it knows and rejects unknown ones per label. No matching
    /// rules, queues or capability types until there is a distributed agent
    /// system to consume them.
    #[serde(default)]
    pub requirements: Vec<SmolStr>,
}

/// The *kind* of environment a scope needs. Not an executor: several executors
/// can provide the same kind. A process on some machine and a container from
/// some image are the two kinds any CI format can ask for; where and how they
/// are provided — this machine, a Docker daemon, a cloud API — is the
/// executor's business and never named here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RuntimeTarget {
    /// A process with the machine's own filesystem and tools.
    HostProcess,
    /// A process inside a container started from `image`.
    Container {
        image:       SmolStr,
        /// Typed container options, lowered by the frontend from whatever
        /// its format calls them. The core never reads them.
        #[serde(default)]
        options:     ContainerOptions,
        /// Registry auth for pulling the image. Carries a secret *name*, never
        /// a value: the graph stays serializable and plaintext exists
        /// only inside the executor's acquire.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credentials: Option<RegistryCredentials>,
    },
}

/// Registry auth for pulling an image. The password is a secret name, resolved
/// by the executor at the point of use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryCredentials {
    pub username:        SmolStr,
    /// The name of the secret holding the password.
    pub password_secret: SmolStr,
}

/// A sidecar container with its scope's lifetime: started at acquisition,
/// reachable by its name, healthy before the scope's first step, torn down with
/// the scope. Format-generic — "service container" is a concept, not a GitHub
/// feature.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServiceSpec<S = Live> {
    /// The alias other processes in the scope reach it by.
    pub name:        SmolStr,
    pub image:       SmolStr,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env:         BTreeMap<SmolStr, ExprOrValue<S>>,
    /// Typed service options, lowered by the frontend (the health check
    /// rides here). No port publications: a service is reached by its
    /// name on the scope's network, never through a published port.
    #[serde(default)]
    pub options:     ServiceOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<RegistryCredentials>,
}

impl<S> ServiceSpec<S> {
    pub fn new(name: &str, image: &str) -> Self {
        Self {
            name:        SmolStr::new(name),
            image:       SmolStr::new(image),
            env:         BTreeMap::new(),
            options:     ServiceOptions::default(),
            credentials: None,
        }
    }
}

impl Default for RuntimeSpec {
    fn default() -> Self {
        Self {
            target:       RuntimeTarget::HostProcess,
            requirements: Vec::new(),
        }
    }
}

impl RuntimeSpec {
    pub fn host_process() -> Self {
        Self::default()
    }

    pub fn container(image: &str) -> Self {
        Self {
            target:       RuntimeTarget::Container {
                image:       SmolStr::new(image),
                options:     ContainerOptions::default(),
                credentials: None,
            },
            requirements: Vec::new(),
        }
    }

    /// Attach placement labels a frontend collected.
    #[must_use]
    pub fn requiring(mut self, labels: &[&str]) -> Self {
        self.requirements = labels.iter().map(|l| SmolStr::new(*l)).collect();
        self
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspacePolicy {
    /// One workspace shared by every node in the scope.
    #[default]
    Shared,
    /// A fresh workspace per node.
    PerNode,
}

/// "Job" generalized: a resource scope, not a sequence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scope<S = Live> {
    pub id:        ScopeId<S>,
    pub env:       BTreeMap<SmolStr, ExprOrValue<S>>,
    pub runtime:   RuntimeSpec,
    pub workspace: WorkspacePolicy,
    /// Sidecar containers with this scope's lifetime, realized at acquisition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services:  Vec<ServiceSpec<S>>,
}

impl<S> Scope<S> {
    pub fn new(id: ScopeId<S>) -> Self {
        Self {
            id,
            env: BTreeMap::new(),
            runtime: RuntimeSpec::default(),
            workspace: WorkspacePolicy::Shared,
            services: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_env(mut self, key: &str, value: ExprOrValue<S>) -> Self {
        self.env.insert(SmolStr::new(key), value);
        self
    }

    #[must_use]
    pub fn with_runtime(mut self, runtime: RuntimeSpec) -> Self {
        self.runtime = runtime;
        self
    }

    #[must_use]
    pub fn with_workspace(mut self, workspace: WorkspacePolicy) -> Self {
        self.workspace = workspace;
        self
    }
}

// ── Graph ─────────────────────────────────────────────────────────────────

/// How the run's status folds from node outcomes.
///
/// Root cancellation and engine `RunError`s outrank both policies: a cancelled
/// run is `Cancelled`, and an engine error fails the run — errors are never
/// control flow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Completion<S = Live> {
    /// Any failed node record fails the run. The CI rule; today's behavior.
    #[default]
    AnyFailure,
    /// Success iff this node has a success-like final record at quiescence.
    /// Failures elsewhere are control flow. The fabro rule at the exit node.
    ///
    /// The node is *expected* to be terminal; the core does not require it —
    /// the semantics only need a final record — and frontends enforce their own
    /// shape.
    TerminalNode(NodeId<S>),
}

/// The value an invocation returns after its final execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResultProjection<S = Live> {
    /// Return JSON null.
    #[default]
    None,
    /// Return the final record's `Outcome.output` for this node.
    NodeOutput(NodeId<S>),
}

fn is_default_completion<S>(completion: &Completion<S>) -> bool {
    matches!(completion, Completion::AnyFailure)
}

/// The topology and execution resources shared by whole graphs and fragments.
/// Identifier space `S` keeps fragment-local ids separate from live run ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(bound(deserialize = "S: Deserialize<'de> + Default"))]
pub struct GraphBody<S = Live> {
    pub nodes:  Vec<Node<S>>,
    pub scopes: Vec<Scope<S>>,
    pub exprs:  ExprTable<S>,
    /// Seeded with one `Generation(0)` token each.
    pub entry:  Vec<NodeId<S>>,
}

impl<S> Default for GraphBody<S> {
    fn default() -> Self {
        Self {
            nodes:  Vec::new(),
            scopes: Vec::new(),
            exprs:  ExprTable::default(),
            entry:  Vec::new(),
        }
    }
}

impl<S> GraphBody<S> {
    pub fn node(&self, id: NodeId<S>) -> Option<&Node<S>> {
        self.nodes.get(id.index())
    }

    pub fn node_mut(&mut self, id: NodeId<S>) -> Option<&mut Node<S>> {
        self.nodes.get_mut(id.index())
    }

    pub fn scope(&self, id: ScopeId<S>) -> Option<&Scope<S>> {
        self.scopes.get(id.index())
    }

    /// Ids of the edges pointing at `node`, in node order. Joins count these.
    pub fn incoming(&self, node: NodeId<S>) -> Vec<EdgeId<S>> {
        let mut ids: Vec<EdgeId<S>> = self
            .nodes
            .iter()
            .flat_map(|n| n.routing.edges())
            .filter(|e| e.to == node)
            .map(|e| e.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// How many distinct edges point at `node`.
    pub fn in_degree(&self, node: NodeId<S>) -> usize {
        self.incoming(node).len()
    }

    pub fn edges(&self) -> impl Iterator<Item = &Edge<S>> {
        self.nodes.iter().flat_map(|n| n.routing.edges())
    }

    pub fn edge(&self, id: EdgeId<S>) -> Option<&Edge<S>> {
        self.edges().find(|e| e.id == id)
    }

    /// The node an edge leaves from.
    pub fn edge_source(&self, id: EdgeId<S>) -> Option<NodeId<S>> {
        self.nodes
            .iter()
            .find(|n| n.routing.edges().any(|e| e.id == id))
            .map(|n| n.id)
    }
}

/// Run-wide policies a host enforces around the engine: they never change
/// routing or the log, only when a run is cancelled or a route is blocked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPolicy {
    /// Cancel the run after this long with no execution event at all. A
    /// pending human question parks the clock. `None` disables the watchdog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_timeout:                Option<Duration>,
    /// How many times one node may fail with the same deterministic or
    /// structural failure signature before the run fails, and how many times
    /// one restart edge may be taken for the same signature before it is
    /// blocked. `None` disables the circuit breaker. At least 1 when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_restart_signature_limit: Option<NonZeroU32>,
    /// The most invocations one run may declare: the root plus every nested
    /// and branch invocation, finished ones included. A host clamps it to
    /// its own hard ceiling and may lower it; `None` leaves the host's
    /// default. Durable with the graph, so a resume enforces the same limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_invocations:              Option<NonZeroU32>,
}

impl RunPolicy {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A whole workflow: the reusable graph body plus run-level configuration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph<S = Live> {
    #[serde(flatten)]
    pub body:       GraphBody<S>,
    /// Host-enforced run policies (§10): the stall watchdog and the failure
    /// circuit breaker. Empty for formats that declare none.
    #[serde(default, skip_serializing_if = "RunPolicy::is_default")]
    pub policy:     RunPolicy,
    /// Per-run parameters, visible to every expression as a static binding of
    /// the same name — the GHA `github`, `vars` and `runner` contexts, a
    /// native format's `params`. Frontends leave this empty; the host fills
    /// it in before the run starts, so the graph a run used is
    /// self-describing and replay needs nothing beyond it.
    ///
    /// Lowest precedence: a firing's own bindings (`env`, `item`, `status`, …)
    /// shadow a parameter of the same name. Read-only for the whole run.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params:     BTreeMap<SmolStr, Value>,
    /// How the run's status folds from node outcomes (§1).
    #[serde(default, skip_serializing_if = "is_default_completion")]
    pub completion: Completion<S>,
    /// Optional invocation result contract.
    #[serde(default, skip_serializing_if = "is_default_result_projection")]
    pub result:     ResultProjection<S>,
}

fn is_default_result_projection<S>(projection: &ResultProjection<S>) -> bool {
    matches!(projection, ResultProjection::None)
}

/// Deliberately read-only forwarding: reads go through `Deref`, while every
/// topology mutation names `.body` explicitly. There is no `DerefMut`.
impl<S> Deref for Graph<S> {
    type Target = GraphBody<S>;

    fn deref(&self) -> &Self::Target {
        &self.body
    }
}

impl<S> Graph<S> {
    pub fn new() -> Self
    where
        S: Default,
    {
        Self::default()
    }

    /// Set a run parameter. Chainable, for hosts filling the graph in before a
    /// run.
    #[must_use]
    pub fn with_param(mut self, name: &str, value: Value) -> Self {
        self.params.insert(SmolStr::new(name), value);
        self
    }
}

impl Graph {
    /// Rewrite `JoinPolicy::Quorum { n: 1 }` to `Any` on every loop head.
    ///
    /// `Quorum { n: 1 }` and `Any` behave identically today, but invariant 8
    /// admits only `Any` on a loop head: one canonical spelling is easier to
    /// grep and to review, and the equivalence is a property of the current
    /// join semantics rather than a guarantee worth making load-bearing. A
    /// frontend that naturally produces `Quorum { n: 1 }` runs this pass
    /// instead of the invariant being relaxed.
    ///
    /// Run it after lowering and before validating. Returns how many nodes it
    /// changed.
    pub fn normalize_loop_heads(&mut self) -> usize {
        let heads: Vec<NodeId> = self
            .edges()
            .filter(|edge| edge.back)
            .map(|edge| edge.to)
            .collect();
        let mut changed = 0;
        for head in heads {
            if let Some(node) = self.body.node_mut(head)
                && node.join == (JoinPolicy::Quorum { n: 1 })
            {
                node.join = JoinPolicy::Any;
                changed += 1;
            }
        }
        changed
    }
}
