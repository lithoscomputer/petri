//! Awaited extension points around one firing, for a host that must finish
//! work before execution continues.
//!
//! An observer reports; it cannot hold the run. These callbacks can. The
//! driver awaits each one at a fixed place in a completed node's life:
//!
//! 1. [`ExecutionHooks::before_attempt`], before every attempt is dispatched
//!    (retries included). A paused firing keeps its identity, starts no
//!    attempt, and a cancel settles it as `Cancelled`.
//!    [`ExecutionHooks::scope_acquired`] runs once per acquisition of a scope's
//!    environment, after the executor acquired it and before the first attempt
//!    in it is dispatched, with the environment itself: where a host prepares
//!    the workspace (restores it onto a snapshot after a crash, say) before any
//!    work runs in it.
//! 2. The attempt runs; the step's own result policy applies inside the step (a
//!    Fabro stage finalizes an exhausted retry there, explicit routes first),
//!    and the driver applies the node's exhaustion policy
//!    ([`RetryPolicy::finalize`](ir::RetryPolicy::finalize)) to the returned
//!    outcome, once. The engine records the finish it is given.
//! 3. [`ExecutionHooks::prepare_result`], once per attempt, after that and
//!    before the `StepFinished` record is appended. On an attempt the engine
//!    will retry (`will_retry`) it is an interception point; on the final
//!    attempt it is the completion preparation: the outcome is the effective
//!    one the engine will record, the host may change it, and routing is
//!    resolved afterwards from the record, so a change here is what the routes
//!    see. The original is recorded first as a [`Note`].
//! 4. The canonical `StepFinished` record.
//! 5. [`ExecutionHooks::after_record`], once per final outcome, before the
//!    routing decision is asked for.
//! 6. Route selection by the [`DecisionResolver`](crate::DecisionResolver).
//! 7. [`ExecutionHooks::transition`], with the selected routes, before they are
//!    recorded and applied. A fatal error blocks advancement; a best-effort
//!    problem is recorded and the run continues.
//! 8. Advancement: the `RoutingResolved` record, the routes, the tokens.
//!
//! Every callback runs on its own task and its answer re-enters the driver
//! through the signal channel, so a slow host never blocks the loop. With no
//! hooks installed none of this runs: the fast path is unchanged.
//!
//! # Recording
//!
//! Each callback may return [`Note`]s. The driver appends every note as an
//! `Event::StepProgressRecorded` carrying `{"$note": {...}}` before the record
//! the callback preceded, so a note is durable, replayed, and visible to every
//! observer in order. Notes are masked before the append.
//!
//! # Recovery
//!
//! The decisions these callbacks influence are recorded as ordinary
//! external events (`Admitted`, `StepFinished`, `RoutingResolved`). Replay
//! consumes them and calls nothing. Resume reissues only the pending
//! admission or routing decision, under the same [`DecisionId`], and
//! re-dispatches an attempt whose finish never landed; a callback may
//! therefore run twice for one operation across a crash, and a host that
//! performs external effects deduplicates on the operation identity.
//!
//! # Context
//!
//! Every callback receives a [`HookContext`] beside its request: the run
//! key, the invocation, the execution, and the parent call when the
//! execution is a child. One hooks object serves every execution of a run,
//! so the context is what tells a child's callback apart from its sibling's
//! when both carry the same local firing and attempt ids. The operation
//! identity a host deduplicates an external effect on is `(run key,
//! execution, `DecisionId`, effect kind)`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use engine::{Admission, DecisionId, GroupDecision, RouteDecision};
use executor::{ExecEnv, WorkspaceId};
use ir::{Attempt, EdgeId, ExecutionId, FiringId, InvocationId, Outcome, Status, StepEvent, Value};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use store::RunKey;

use crate::view::FiringView;

/// The key a note rides under in a `StepEvent::Custom` value.
pub const NOTE_KEY: &str = "$note";

/// The note kind the driver records when a host adjusted an attempt's result.
/// Its payload is [`ResultPreparedNote`].
pub const RESULT_PREPARED_KIND: &str = "result_prepared";

/// The note kind the driver records when a transition callback reported a
/// problem or an override. Its payload is [`TransitionNote`].
pub const TRANSITION_KIND: &str = "transition";

/// The failure class of an outcome the host's result preparation failed on.
pub const RESULT_PREPARATION_CLASS: ir::FailureClass =
    ir::FailureClass::new_static("result_preparation_failed");

/// The note kind the driver records when an executor-enforced attempt budget
/// stops counting: the attempt asked a question. Its payload is
/// [`BudgetNote`].
pub const BUDGET_PAUSED_KIND: &str = "budget_paused";

/// The note kind the driver records when the budget counts again: the last
/// pending question of the attempt was answered. Its payload is
/// [`BudgetNote`].
pub const BUDGET_RESUMED_KIND: &str = "budget_resumed";

/// What the driver records about an attempt budget's pause or resume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetNote {
    pub attempt:           Attempt,
    /// The active-work time left on the attempt.
    pub remaining_ms:      u64,
    /// Questions of this attempt still waiting for an answer.
    pub pending_questions: u32,
}

/// The reason prefix on a routing block a fatal transition produced.
pub const TRANSITION_BLOCKED_PREFIX: &str = "transition failed: ";

/// A small structured fact a callback wants in the durable record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub kind:    SmolStr,
    #[serde(default)]
    pub payload: Value,
}

impl Note {
    pub fn new(kind: impl Into<SmolStr>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
        }
    }

    /// The progress event that records this note.
    pub fn to_step_event(&self) -> StepEvent {
        StepEvent::Custom(serde_json::json!({ NOTE_KEY: self }))
    }

    /// The note a progress event carries, if it is one.
    pub fn from_step_event(event: &StepEvent) -> Option<Self> {
        let StepEvent::Custom(value) = event else {
            return None;
        };
        serde_json::from_value(value.get(NOTE_KEY)?.clone()).ok()
    }
}

/// What the driver records when a host changed an attempt's result: the
/// attempt's original status and output, kept beside the effective record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultPreparedNote {
    pub attempt:   Attempt,
    /// The status the host was handed: the step's, with the node's
    /// exhaustion policy applied.
    pub original:  Status,
    /// The status the host made effective.
    pub effective: Status,
    /// The step's output before the adjustment.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub output:    Value,
    /// The host's reason, when it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason:    Option<String>,
}

/// What the driver records about a transition callback's answer, when there
/// is something to say: a best-effort problem, an override, or a block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransitionNote {
    pub attempt:   Attempt,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems:  Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<RouteOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked:   Option<String>,
}

/// The firing that called a nested invocation: where a child execution's
/// context points back to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentLink {
    /// The calling execution.
    pub execution: ExecutionId,
    pub firing:    FiringId,
    pub attempt:   Attempt,
    /// The call slot the calling step named.
    pub slot:      SmolStr,
}

/// Which execution a callback belongs to. The driver hands the same
/// [`ExecutionHooks`] object every execution of a run, and hands this beside
/// every request so the host can tell them apart: two child invocations of
/// one workflow reuse the same local firing and attempt ids, and only the
/// execution differs.
///
/// A host that performs an external effect from a callback keys it on
/// `(run_key, execution, DecisionId, effect kind)` and deduplicates on that
/// key: a resume reissues a pending decision under the same `DecisionId` in
/// the same execution, so the key is stable across a crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookContext {
    /// The run's identity in its store.
    pub run_key:    RunKey,
    pub invocation: InvocationId,
    pub execution:  ExecutionId,
    /// The call that started this invocation; `None` for the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent:     Option<ParentLink>,
}

impl HookContext {
    pub fn new(run_key: RunKey, invocation: InvocationId, execution: ExecutionId) -> Self {
        Self {
            run_key,
            invocation,
            execution,
            parent: None,
        }
    }

    #[must_use]
    pub fn with_parent(mut self, parent: ParentLink) -> Self {
        self.parent = Some(parent);
        self
    }
}

/// The awaited admission of one attempt.
pub struct AdmitAttempt {
    pub decision: DecisionId,
    pub view:     Arc<FiringView>,
}

/// What a host decided before an attempt: admit it, skip the node with this
/// outcome, or block it, plus any notes to record first.
pub struct AttemptDecision {
    pub admission: Admission,
    pub notes:     Vec<Note>,
}

impl AttemptDecision {
    pub fn admit() -> Self {
        Self {
            admission: Admission::Admit,
            notes:     Vec::new(),
        }
    }

    #[must_use]
    pub fn with_notes(mut self, notes: Vec<Note>) -> Self {
        self.notes = notes;
        self
    }
}

/// Where a finished attempt's outcome came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultOrigin {
    /// The step kind returned it.
    Step,
    /// The driver produced it: a dispatch failure, a forced stop, a cancel
    /// settled at resume.
    Driver,
}

/// An attempt's result, before it is recorded.
pub struct PrepareResult {
    pub view:       Arc<FiringView>,
    /// The outcome as the engine will record it if the host leaves it alone,
    /// already masked: the step's result with the node's exhaustion policy
    /// applied, so an exhausted retry under `Exhaustion::AcceptPartial`
    /// arrives as the `PartialSuccess` it becomes, its failure in
    /// `underlying`.
    pub outcome:    Outcome,
    pub origin:     ResultOrigin,
    /// Whether the engine will schedule another attempt if this outcome is
    /// recorded unchanged: the node's retry policy applied to the status.
    /// `false` on the attempt whose outcome is the firing's, which is the
    /// one to prepare.
    pub will_retry: bool,
    /// Whether this is the last attempt the policy allows.
    pub exhausted:  bool,
}

/// The changes a host may make to an effective result. Splice requests are
/// the step's own plan and cannot be changed here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultAdjustment {
    pub status:          Option<Status>,
    pub output:          Option<Value>,
    pub context_updates: Option<BTreeMap<SmolStr, Value>>,
    /// Merged into `metrics.custom`, key by key.
    pub metrics:         BTreeMap<SmolStr, Value>,
    /// Recorded in the original-evidence note.
    pub reason:          Option<String>,
}

impl ResultAdjustment {
    pub fn is_empty(&self) -> bool {
        self.status.is_none()
            && self.output.is_none()
            && self.context_updates.is_none()
            && self.metrics.is_empty()
    }

    /// Apply to `outcome`, returning whether anything changed.
    pub(crate) fn apply(self, outcome: &mut Outcome) -> bool {
        let mut changed = false;
        if let Some(status) = self.status {
            changed |= outcome.status != status;
            outcome.status = status;
        }
        if let Some(output) = self.output {
            changed |= outcome.output != output;
            outcome.output = output;
        }
        if let Some(updates) = self.context_updates {
            changed |= outcome.context_updates != updates;
            outcome.context_updates = updates;
        }
        for (key, value) in self.metrics {
            let previous = outcome.metrics.custom.insert(key, value.clone());
            changed |= previous.as_ref() != Some(&value);
        }
        changed
    }
}

/// What a host decided about an attempt's result.
pub struct Prepared {
    pub adjustment: ResultAdjustment,
    pub notes:      Vec<Note>,
}

impl Prepared {
    /// Record the result as the step reported it.
    pub fn unchanged() -> Self {
        Self {
            adjustment: ResultAdjustment::default(),
            notes:      Vec::new(),
        }
    }
}

/// Result preparation failed. A fatal failure replaces the outcome with a
/// `Failure` of class [`RESULT_PREPARATION_CLASS`]; a best-effort failure
/// records the message and leaves the result as reported.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct PrepareError {
    pub message: String,
    pub fatal:   bool,
}

impl PrepareError {
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal:   true,
        }
    }

    pub fn best_effort(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fatal:   false,
        }
    }
}

/// Workflow execution for this driver has ended: its status is final and no
/// environment has been released yet. Only the driver that owns the run
/// reports it (a bare driver, or the coordinator's root invocation on a
/// terminal exit); a restart successor and an internal child invocation
/// report nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFinished {
    pub status:  ir::RunStatus,
    /// The failure the run ended on, when a stage recorded one.
    pub failure: Option<String>,
}

/// A scope's environment is about to be released. The environment is still
/// usable when the callback runs; a release that is part of the run's end
/// waits for [`ExecutionHooks::run_finished`] first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeReleased {
    pub scope:   ir::ScopeId,
    pub outcome: executor::ScopeOutcome,
}

/// A scope's environment was acquired and no attempt has run in it yet.
///
/// The environment is the one the scope's steps will receive: a host runs a
/// process in it and reads or writes its workspace's files through it, on
/// this machine and in a remote sandbox alike. The handle is this
/// execution's own; an inherited sandbox is acquired by each execution that
/// runs in it, and each is told.
#[derive(Clone)]
pub struct ScopeAcquired {
    pub scope:     ir::ScopeId,
    /// The workspace the environment holds, as the executor named it: the
    /// lease's workspace when a coordinator allocated one.
    pub workspace: WorkspaceId,
    pub env:       Arc<dyn ExecEnv>,
}

impl fmt::Debug for ScopeAcquired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopeAcquired")
            .field("scope", &self.scope)
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

/// The host could not prepare a scope's environment. The acquisition fails
/// with this message: every firing in the scope fails, routably, as it
/// would had the executor refused the scope.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ScopeAcquiredError {
    pub message: String,
}

impl ScopeAcquiredError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// A final outcome has been recorded and routing is about to be asked for.
pub struct Recorded {
    pub view:    Arc<FiringView>,
    /// The recorded, effective outcome.
    pub outcome: Outcome,
}

/// The routes selected for a completed firing, before they are recorded and
/// applied.
pub struct Transition {
    pub decision: DecisionId,
    pub view:     Arc<FiringView>,
    pub outcome:  Outcome,
    /// One decision per routing group, in declared order. `Emit` names the
    /// edge; `None` is a no-route completion.
    pub groups:   Vec<GroupDecision>,
}

/// A host's replacement for one group's selected edge. The engine validates
/// that the edge is an arm of the group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteOverride {
    pub group: u32,
    pub edge:  EdgeId,
}

/// What a transition callback finished with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TransitionReport {
    pub overrides: Vec<RouteOverride>,
    /// Best-effort work that failed. Recorded; execution continues.
    pub problems:  Vec<String>,
    pub notes:     Vec<Note>,
}

/// Required transition work failed: advancement is blocked and the firing's
/// routing is recorded as blocked with this reason.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TransitionError {
    pub message: String,
}

impl TransitionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// The host's awaited extension points. Every method has an immediate
/// default, so an implementation names only the points it uses.
///
/// Callbacks run on their own tasks, concurrently across firings, and never
/// on the driver loop. They see an immutable [`FiringView`] and change the
/// run only through their return values. A callback that never returns holds
/// its firing open; a root kill aborts pending callbacks and records the
/// original results. Every callback receives the [`HookContext`] of the
/// execution it belongs to; a wrapper forwards it unchanged.
#[async_trait::async_trait]
pub trait ExecutionHooks: Send + Sync {
    /// Before an attempt is dispatched, once per attempt. The view carries
    /// the resolved config. Returning `Skip` or `Block` ends the visit without
    /// an attempt; `Admit` hands on to the decision resolver.
    async fn before_attempt(
        &self,
        context: &HookContext,
        request: AdmitAttempt,
    ) -> AttemptDecision {
        let _ = (context, request);
        AttemptDecision::admit()
    }

    /// After an attempt returned, with the step's result policy and the
    /// node's exhaustion policy applied, and before its record is appended.
    /// Once per attempt; the one with `will_retry == false` is the final
    /// completion, and what it returns is what routing sees.
    async fn prepare_result(
        &self,
        context: &HookContext,
        request: PrepareResult,
    ) -> Result<Prepared, PrepareError> {
        let _ = (context, request);
        Ok(Prepared::unchanged())
    }

    /// After a final outcome is recorded, before routing is resolved. Notes
    /// returned here are appended before the `RoutingResolved` record.
    async fn after_record(&self, context: &HookContext, recorded: Recorded) -> Vec<Note> {
        let _ = (context, recorded);
        Vec::new()
    }

    /// After routes are selected, before they are recorded and applied. Runs
    /// once per completed firing, no-route completions included.
    async fn transition(
        &self,
        context: &HookContext,
        transition: Transition,
    ) -> Result<TransitionReport, TransitionError> {
        let _ = (context, transition);
        Ok(TransitionReport::default())
    }

    /// Declare required finalization for this run. The coordinator records
    /// this requirement and refuses resume with a different declaration.
    /// The declaration must stay stable for the run. Wrappers must forward
    /// both this method and `finalize_run`.
    fn requires_run_finalization(&self) -> bool {
        false
    }

    /// Complete required host work after terminal workflow execution and
    /// before run-end observations or environment release. Called only when
    /// `requires_run_finalization` is true, only by the run owner, including
    /// failed and cancelled executions; restart exits do not finalize.
    ///
    /// The argument describes execution, not the overall result. Rejection
    /// fails an otherwise successful run; cancellation remains cancelled.
    /// Implementors must support repetition after interruption, or reject
    /// when they cannot establish success. Recovery does not recreate lost
    /// environments or provide independent finalization retries. Pending
    /// finalization keeps the run nonterminal. Cancellation after execution
    /// has ended does not interrupt this awaited callback.
    async fn finalize_run(
        &self,
        context: &HookContext,
        finished: RunFinished,
    ) -> Result<(), ir::FinalizationFailure> {
        let _ = (context, finished);
        Err(ir::FinalizationFailure::new(
            "finalizer_unavailable",
            "required run finalization has no implementation",
        ))
    }

    /// Workflow execution ended, before its environments are released.
    /// Observational: notes never change the result. Required work belongs in
    /// `finalize_run`, which is awaited before this point. Awaited: the
    /// release waits for it.
    /// The notes come back to the driver, which hands them to its host in
    /// the [`ExecutionReport`](crate::ExecutionReport) (`run_notes`); a
    /// coordinator records them at run level, since no firing owns them.
    async fn run_finished(&self, context: &HookContext, finished: RunFinished) -> Vec<Note> {
        let _ = (context, finished);
        Vec::new()
    }

    /// A scope's environment is about to be released. Awaited: the release
    /// waits for it. Notes travel as for [`Self::run_finished`]. A resumed
    /// driver also calls it at its end, after `run_finished`, for each scope
    /// an earlier lifetime acquired and it never holds: that lifetime's notes
    /// died with it. There is no environment then, and a crash after the
    /// notes were recorded repeats them.
    async fn scope_released(&self, context: &HookContext, released: ScopeReleased) -> Vec<Note> {
        let _ = (context, released);
        Vec::new()
    }

    /// A scope's environment was acquired and no attempt has run in it yet.
    /// Awaited: the first attempt in the scope waits for it. Runs on every
    /// acquisition, a resumed execution's and an inherited sandbox's
    /// included. An error fails the acquisition: every firing in the scope
    /// fails, routably, with the message.
    async fn scope_acquired(
        &self,
        context: &HookContext,
        acquired: ScopeAcquired,
    ) -> Result<(), ScopeAcquiredError> {
        let _ = (context, acquired);
        Ok(())
    }
}

/// Apply a transition report to the resolver's decisions: overrides replace
/// the selected edge and are traced as such; a fatal error blocks every
/// group.
pub(crate) fn apply_transition(
    groups: &mut [GroupDecision],
    result: &Result<TransitionReport, TransitionError>,
) {
    match result {
        Ok(report) => {
            for override_ in &report.overrides {
                if let Some(group) = groups.iter_mut().find(|g| g.group == override_.group) {
                    group.trace.insert(0, engine::Intervention::Override {
                        middleware: engine::MiddlewareKey::new("host.transition"),
                        edge:       override_.edge,
                    });
                    group.decision = RouteDecision::Emit(override_.edge);
                }
            }
        }
        Err(error) => {
            let reason = SmolStr::new(format!("{TRANSITION_BLOCKED_PREFIX}{}", error.message));
            for group in groups {
                group.decision = RouteDecision::Block {
                    reason: reason.clone(),
                };
            }
        }
    }
}
