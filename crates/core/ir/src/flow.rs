//! Values that flow while a graph runs: tokens, outcomes, run context, control
//! signals.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol_str::SmolStr;

use crate::ids::{EdgeId, FiringId, Generation};
use crate::splice::SpliceRequest;

/// One unit of flow, sitting on an edge and waiting for a join.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub edge:       EdgeId,
    /// Named `generation` rather than `gen`: `gen` is a reserved keyword in
    /// edition 2024.
    pub generation: Generation,
    pub payload:    Value,
    /// The firing that emitted this token. Seed tokens use `FiringId(0)`.
    pub from:       FiringId,
}

impl Token {
    pub fn new(edge: EdgeId, generation: Generation, payload: Value, from: FiringId) -> Self {
        Self {
            edge,
            generation,
            payload,
            from,
        }
    }

    /// A token seeded onto an entry node or an expansion clone. `edge` is the
    /// synthetic seed edge the engine allocated for that node, so joins count
    /// it like any other incoming edge.
    pub fn seeded(edge: EdgeId, generation: Generation, payload: Value) -> Self {
        Self {
            edge,
            generation,
            payload,
            from: FiringId::new(0),
        }
    }
}

/// How a firing ended.
///
/// **This enum is closed.** These six variants are the complete and permanent
/// status vocabulary. Any future frontend concept must map onto them; none may
/// extend them. Attractor's first-class `RETRY` outcome, for instance, lowers
/// to `Failure` with `class: "retry_requested"` plus a matching `retry_on`, not
/// to a new variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Success,
    /// Soft failure or partial completion. Routing-visible, and success-like.
    ///
    /// `underlying` is the failure this partial success was converted from,
    /// whenever one was: what the step really ended with, a failure or a
    /// timeout. It is not a cause — a policy made the conversion (`soft_fail`,
    /// `continue-on-error`, `AcceptPartial`) — and it keeps the log from
    /// recording a clean success for something that failed (§3.1 rule 3).
    PartialSuccess {
        underlying: Option<UnderlyingFailure>,
    },
    Failure(FailureInfo),
    /// The precondition was false, or an upstream skip propagated.
    Skipped,
    Cancelled,
    TimedOut,
}

/// The failure a [`Status::PartialSuccess`] was converted from: one of the two
/// statuses [`Status::is_failure`] names, kept whole so a timeout stays a
/// timeout.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnderlyingFailure {
    Failure(FailureInfo),
    TimedOut,
}

impl UnderlyingFailure {
    /// The failure `status` is, when it is one.
    pub fn of(status: &Status) -> Option<Self> {
        match status {
            Status::Failure(info) => Some(Self::Failure(info.clone())),
            Status::TimedOut => Some(Self::TimedOut),
            Status::Success
            | Status::PartialSuccess { .. }
            | Status::Skipped
            | Status::Cancelled => None,
        }
    }

    /// The failure's info. A timeout carries none.
    pub fn failure_info(&self) -> Option<&FailureInfo> {
        match self {
            Self::Failure(info) => Some(info),
            Self::TimedOut => None,
        }
    }

    /// The status the step really ended with.
    pub fn status(&self) -> Status {
        match self {
            Self::Failure(info) => Status::Failure(info.clone()),
            Self::TimedOut => Status::TimedOut,
        }
    }
}

/// A [`Status`] with its payload stripped, for matching on the variant alone.
///
/// Deliberately unordered: no total order over these six means anything, and
/// declaration order is not one. Contrast
/// [`SplicePolicy`](crate::SplicePolicy), whose derived order *is* its
/// authority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusKind {
    Success,
    PartialSuccess,
    Failure,
    Skipped,
    Cancelled,
    TimedOut,
}

/// The one place the status-to-kind mapping lives, so the two cannot drift.
impl From<&Status> for StatusKind {
    fn from(status: &Status) -> Self {
        match status {
            Status::Success => Self::Success,
            Status::PartialSuccess { .. } => Self::PartialSuccess,
            Status::Failure(_) => Self::Failure,
            Status::Skipped => Self::Skipped,
            Status::Cancelled => Self::Cancelled,
            Status::TimedOut => Self::TimedOut,
        }
    }
}

impl Status {
    /// The lowercase tag the status functions in expressions match on.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::PartialSuccess { .. } => "partial_success",
            Self::Failure(_) => "failure",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    /// The inverse of [`Status::tag`]. `failure` supplies the payload when
    /// the tag names a failure; an unknown tag returns `None`.
    pub fn from_tag(tag: &str, failure: impl FnOnce() -> FailureInfo) -> Option<Self> {
        Some(match tag {
            "success" => Self::Success,
            "partial_success" => Self::PartialSuccess { underlying: None },
            "failure" => Self::Failure(failure()),
            "skipped" => Self::Skipped,
            "cancelled" => Self::Cancelled,
            "timed_out" => Self::TimedOut,
            _ => return None,
        })
    }

    /// The variant, without its payload. See [`From<&Status> for StatusKind`],
    /// which holds the only copy of this mapping.
    pub fn kind(&self) -> StatusKind {
        StatusKind::from(self)
    }

    /// **The** definition of success-likeness. Joins, cancel scopes, default
    /// success guards and retry all call this; none of them open-codes the
    /// match.
    ///
    /// A guard that needs to tell the two apart tests `partial_success()`
    /// explicitly.
    pub fn is_success_like(&self) -> bool {
        matches!(self, Self::Success | Self::PartialSuccess { .. })
    }

    /// Whether this status counts as a failure when folding the run status.
    /// `PartialSuccess`, `Skipped` and `Cancelled` do not.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failure(_) | Self::TimedOut)
    }

    /// The failure info behind this status, if any: the failure itself, or the
    /// one a `PartialSuccess` was converted from.
    pub fn failure_info(&self) -> Option<&FailureInfo> {
        match self {
            Self::Failure(info) => Some(info),
            Self::PartialSuccess { underlying } => underlying
                .as_ref()
                .and_then(UnderlyingFailure::failure_info),
            // `TimedOut` is a failure by `is_failure`, but it carries no info.
            Self::Success | Self::Skipped | Self::Cancelled | Self::TimedOut => None,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self::Failure(FailureInfo::new(message))
    }

    /// Rebuild this status with every failure message passed through `f` — the
    /// driver's masking hook before a finish record is appended. `class` is
    /// untouched: routing (`retry_on`) matches on it, and classes are static
    /// tags or `exit_status:{n}`, never free text. The match is exhaustive on
    /// purpose, so a variant that grows a message cannot dodge the mapping.
    #[must_use]
    pub fn map_messages(self, f: impl Fn(String) -> String) -> Self {
        let map_info = |info: FailureInfo| FailureInfo {
            message: f(info.message),
            class:   info.class,
        };
        match self {
            Self::Failure(info) => Self::Failure(map_info(info)),
            Self::PartialSuccess { underlying } => Self::PartialSuccess {
                underlying: underlying.map(|underlying| match underlying {
                    UnderlyingFailure::Failure(info) => UnderlyingFailure::Failure(map_info(info)),
                    UnderlyingFailure::TimedOut => UnderlyingFailure::TimedOut,
                }),
            },
            status @ (Self::Success | Self::Skipped | Self::Cancelled | Self::TimedOut) => status,
        }
    }

    /// A soft failure that keeps the real failure on the record.
    pub fn partial(underlying: FailureInfo) -> Self {
        Self::PartialSuccess {
            underlying: Some(UnderlyingFailure::Failure(underlying)),
        }
    }

    /// A partial completion that was never a failure to begin with.
    pub fn partial_clean() -> Self {
        Self::PartialSuccess { underlying: None }
    }
}

/// What kind of failure a [`FailureInfo`] carries — the tag `retry_on` and
/// soft-fail routing match on. Step kinds set it: a static family name
/// (`"workspace_setup"`, `"retry_requested"`) or one of the two computed
/// families, [`FailureClass::exit_status`] and [`FailureClass::signal`].
/// Serializes as the bare string, so the replay format is unchanged; empty
/// means unclassified.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FailureClass(SmolStr);

impl FailureClass {
    /// A static class tag, allocation-free — how the step kinds' class
    /// constants are built.
    pub const fn new_static(class: &'static str) -> Self {
        Self(SmolStr::new_static(class))
    }

    pub fn new(class: impl Into<SmolStr>) -> Self {
        Self(class.into())
    }

    /// The conventional class for a process step that exited non-zero.
    pub fn exit_status(code: i32) -> Self {
        Self(SmolStr::new(format!("exit_status:{code}")))
    }

    /// The conventional class for a process ended by a foreign signal.
    pub fn signal(signal: i32) -> Self {
        Self(SmolStr::new(format!("signal:{signal}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the failure is unclassified.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&str> for FailureClass {
    fn from(class: &str) -> Self {
        Self(SmolStr::new(class))
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for FailureClass {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<str> for FailureClass {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureInfo {
    pub message: String,
    /// See [`FailureClass`].
    #[serde(default)]
    pub class:   FailureClass,
}

impl FailureInfo {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            class:   FailureClass::default(),
        }
    }

    #[must_use]
    pub fn with_class(mut self, class: impl Into<FailureClass>) -> Self {
        self.class = class.into();
        self
    }

    /// The conventional failure for a process step that exited non-zero.
    pub fn exit_status(code: i32) -> Self {
        Self::new(format!("step exited with status {code}"))
            .with_class(FailureClass::exit_status(code))
    }
}

/// The sandbox an executor acquired for a scope, as recorded when the scope
/// was acquired: the provider it lives on, the provider's own id for it,
/// what it runs, and the directory the scope's steps run in. The facts a
/// host needs to reach the same sandbox again after the run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxInstance {
    /// The provider kind: `host`, `docker`, `daytona`, or a plugin's kind.
    pub provider:          SmolStr,
    /// The provider's id for the sandbox: a container id, a remote sandbox
    /// id, the host provider's registry id.
    pub instance:          SmolStr,
    /// The image the sandbox runs, when the provider knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image:             Option<SmolStr>,
    /// The snapshot the sandbox was created from, when the provider knows
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot:          Option<SmolStr>,
    /// The directory commands run in by default: the scope's workspace as
    /// the sandbox sees it.
    pub working_directory: SmolStr,
}

/// Counters reported by a step. Free-form so step kinds can add their own.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code:   Option<i32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub custom:      BTreeMap<SmolStr, Value>,
}

impl Metrics {
    #[must_use]
    pub fn with_duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    #[must_use]
    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = Some(code);
        self
    }
}

/// The result of one attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub status:          Status,
    /// Structured output: the value guards and `map` expressions see.
    pub output:          Value,
    #[serde(default)]
    pub metrics:         Metrics,
    /// Writes into [`RunContext::kv`], merged in event order by the core. This
    /// is the only path that writes run-scoped key/value state.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context_updates: BTreeMap<SmolStr, Value>,
    /// Ordered splice requests. Only a firing's **final** attempt applies them,
    /// in one transaction: every request prepares or none applies, and a
    /// rejection converts the whole outcome to `Failure{class:
    /// invalid_splice}`. A non-final attempt's requests are recorded in the
    /// event log and change nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub splices:         Vec<SpliceRequest>,
}

impl Outcome {
    pub fn new(status: Status, output: Value) -> Self {
        Self {
            status,
            output,
            metrics: Metrics::default(),
            context_updates: BTreeMap::new(),
            splices: Vec::new(),
        }
    }

    pub fn success(output: impl Into<Value>) -> Self {
        Self::new(Status::Success, output.into())
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self::new(Status::failure(message), Value::Null)
    }

    /// A soft failure. The real failure stays on the record.
    pub fn partial(underlying: FailureInfo, output: impl Into<Value>) -> Self {
        Self::new(Status::partial(underlying), output.into())
    }

    pub fn skipped() -> Self {
        Self::new(Status::Skipped, Value::Null)
    }

    pub fn cancelled() -> Self {
        Self::new(Status::Cancelled, Value::Null)
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    #[must_use]
    pub fn with_context_update(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.context_updates.insert(SmolStr::new(key), value.into());
        self
    }

    /// Append one splice request. Order is preserved: a later request may
    /// reference a node an earlier one added.
    #[must_use]
    pub fn with_splice(mut self, request: SpliceRequest) -> Self {
        self.splices.push(request);
        self
    }

    #[must_use]
    pub fn with_splices(mut self, requests: Vec<SpliceRequest>) -> Self {
        self.splices = requests;
        self
    }
}

// ── Run context ───────────────────────────────────────────────────────────

/// What one node instance left behind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    /// The final attempt's status, raw.
    pub status:     Status,
    pub output:     Value,
    /// The latest generation to complete.
    pub generation: Generation,
    /// How many attempts the final firing took.
    pub attempts:   u32,
}

impl NodeRecord {
    /// The shape expressions see under `nodes.<id>`.
    pub fn to_value(&self) -> Value {
        serde_json::json!({
            "status": self.status.tag(),
            "output": self.output.clone(),
            "generation": self.generation.raw(),
            "attempts": self.attempts,
            "success_like": self.status.is_success_like(),
        })
    }
}

/// Run-scoped state that expressions can read.
///
/// Written **only** inside `apply`, in event order: node records when a
/// firing's final attempt finishes, and `kv` merged from
/// `Outcome::context_updates` in that same order, last write winning. No other
/// write path exists, which is what keeps the core pure and replay
/// byte-identical.
///
/// **`kv` merges on final attempts only**, like the node records. Guards read
/// `kv` concurrently — a parallel node's routing can consult it mid-run — so
/// merging a retried attempt's writes would let work that was later discarded
/// steer routing elsewhere in the graph. The invariant is that retries are
/// invisible everywhere except the event log. Per-attempt data is not lost:
/// every attempt's finish record carries its full outcome, `context_updates`
/// included, so tooling reads it from the log. It simply never enters the
/// routing-visible store.
///
/// This is derived state — reconstructible from the event log. It is part of
/// the engine state, but it is never checkpointed as a separate artifact.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunContext {
    /// Keyed by node instance name, so a matrix clone records under `build#2`.
    ///
    /// Both maps are `Arc`-backed copy-on-write: the engine snapshots the run
    /// context per routing decision, so a snapshot is two reference bumps, and
    /// a mutation deep-copies only while a snapshot is still outstanding.
    pub nodes: Arc<BTreeMap<SmolStr, NodeRecord>>,
    pub kv:    Arc<BTreeMap<SmolStr, Value>>,
}

impl RunContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn node(&self, name: &str) -> Option<&NodeRecord> {
        self.nodes.get(name)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.kv.get(key)
    }

    /// Record a completed firing. Called by the core only, on a final attempt.
    pub fn record(&mut self, name: SmolStr, record: NodeRecord) {
        Arc::make_mut(&mut self.nodes).insert(name, record);
    }

    /// Merge `context_updates`, last write winning. Called by the core only.
    pub fn merge(&mut self, updates: &BTreeMap<SmolStr, Value>) {
        if updates.is_empty() {
            return;
        }
        let kv = Arc::make_mut(&mut self.kv);
        for (key, value) in updates {
            kv.insert(key.clone(), value.clone());
        }
    }

    /// The `nodes` map as expressions see it.
    pub fn nodes_value(&self) -> Value {
        Value::Object(
            self.nodes
                .iter()
                .map(|(name, record)| (name.to_string(), record.to_value()))
                .collect(),
        )
    }

    /// The `kv` map as expressions see it.
    pub fn kv_value(&self) -> Value {
        Value::Object(
            self.kv
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect(),
        )
    }
}

// ── Step progress and control ─────────────────────────────────────────────

/// Progress reported by a running step. Carries no coordination meaning.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepEvent {
    Log { stream: LogStream, line: String },
    Artifact { name: SmolStr, uri: String },
    Custom(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// A signal delivered to a live firing.
///
/// Reserved seam: `Pause` lands here in v2 and reuses the same cancel-scope
/// machinery, so this enum is non-exhaustive from day one. `Steer` and
/// `Approve` shipped as [`Control::Deliver`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Control {
    /// Ask the step to stop: the polite ladder (TERM, grace, KILL).
    Cancel,
    /// Stop the step now: straight to `SIGKILL`, no ladder, no grace. Delivered
    /// by a `KillRequested` — to every live firing in the killed closure,
    /// ones already politely cancelling included, which a plain `Cancel`
    /// cannot say.
    Kill,
    /// A value delivered to a waiting step: a human's answer, a supervisor's
    /// instruction. May be delivered repeatedly to one firing (steering is a
    /// stream). Never starts the cancellation ladder or the kill tier.
    Deliver(Value),
}

/// How a whole run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Success,
    Failed,
    Cancelled,
}

/// The text a reader sees, in the same lowercase shape as [`Status::tag`], so
/// a run and its steps read the same way. `Debug` stays for diagnostics.
impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        })
    }
}
