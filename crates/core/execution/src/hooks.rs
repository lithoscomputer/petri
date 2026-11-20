//! The host-facing hook service: one interface a local hook executor and an
//! embedding host both implement, and one adapter that calls it from the
//! driver's awaited extension points, so a configured hook runs exactly once
//! whoever serves it.
//!
//! The split is deliberate. A [`HookService`] answers "run the hooks
//! configured for this point, on this subject, and tell me the decision".
//! Which hooks are configured, how a command hook's exit code becomes a
//! decision, and what a timeout means are the service's business: the local
//! implementation reads the workflow's configuration; a platform host reads
//! its own. The [`HookAdapter`] is the one caller: it implements
//! [`ExecutionHooks`] and asks the service at each point, records the
//! service's report as a `hook` note, and turns the decision into the
//! driver's admission, result, or transition answer. Nothing else in Petri
//! calls a hook service for a workflow point, so installing one service
//! means each hook executes once.
//!
//! A few points are not driven by the adapter, because the driver is not
//! where they happen: the tool-boundary points (`BeforeToolUse`,
//! `AfterToolUse`, `AfterToolFailure`) are asked by the agent backend's tool
//! middleware at the actual tool boundary; `ScopeReady` and `RunStarted`, and
//! the admission points of a node whose frontend marked it
//! [`ADMISSION_HOOKS_META`]` = "step"`, are asked by the step that runs first
//! in the scope's environment (the sandbox has to exist before a hook can be
//! placed in it); `ForkStarted` and `ForkCompleted` are asked by the fork
//! step and the fan-in step, the two places that run exactly once per fork
//! visit (a `for_each` fork has one routing group, so the driver cannot see
//! it at admission). Every one of them reaches the same service through the
//! [`HookServiceHandle`] capability, so a host that replaces the service
//! receives every point, and each point is asked exactly once.

use std::sync::Arc;

use driver::FiringView;
use driver::lifecycle::{
    AdmitAttempt, AttemptDecision, ExecutionHooks, HookContext, Note, PrepareError, PrepareResult,
    Prepared, Recorded, RouteOverride, RunFinished, ScopeReleased, Transition, TransitionError,
    TransitionReport,
};
use engine::{Admission, RouteDecision};
pub use ir::placeholder::{ADMISSION_HOOKS_BY_STEP, ADMISSION_HOOKS_META};
use ir::{EdgeId, Outcome, Status, Value};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::{ChildSpan, ExecutionIdentity};

/// The note kind the adapter records for every service report.
pub const HOOK_NOTE_KIND: &str = "hook";

/// The note kind one record of a hook's own agent activity is recorded
/// under: the payload is a [`HookActivity`]. The projector derives
/// `hook_activity` events from it, apart from the stage's own agent activity,
/// so a consumer never counts a hook's model requests as the stage's.
pub const HOOK_ACTIVITY_NOTE_KIND: &str = "hook.activity";

/// Where in a run a hook may be configured. Names describe the point, not
/// any one workflow format's spelling of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    /// Before the first attempt of a visit is dispatched. Driven by the
    /// adapter at admission, or, for a node whose meta says
    /// [`ADMISSION_HOOKS_META`]` = "step"`, by the node's own step once its
    /// environment is in place.
    BeforeVisit,
    /// Before every attempt, retries included. Driven like `BeforeVisit`.
    BeforeAttempt,
    /// After an attempt returned, before its record; the decision may adjust
    /// the effective result.
    AfterAttempt,
    /// After a visit's final record, before routing.
    AfterVisit,
    /// After routes were selected, before advancement; the decision may
    /// override or block.
    RouteSelected,
    /// Before a retry attempt is scheduled.
    Retrying,
    /// A fork's branches are about to start. Asked by the fork node's step,
    /// once per fork visit; the request carries the fork node's view.
    ForkStarted,
    /// A fork's branches have all completed. Asked by the fan-in step, once
    /// every branch is in and before the results are published; the request
    /// carries the fan-in's view and a [`ForkCompletedPayload`] naming the
    /// fork.
    ForkCompleted,
    /// A scope's environment is in place and seeded (checked out), before
    /// the first stage runs in it. Asked by that first step, once per run;
    /// the request carries the step's view and a [`ScopeReadyPayload`].
    ScopeReady,
    /// The run's work is about to start: the environment is ready and no
    /// stage has run. Asked once per run by the same step, after
    /// `ScopeReady`; the request carries the step's view and no payload.
    RunStarted,
    /// The run ended: its status is final, no environment is released yet.
    /// A run-level point: the request carries no firing view; its payload is
    /// [`RunFinishedPayload`].
    RunFinished,
    /// A scope's environment is about to be released. A run-level point: no
    /// firing view; the payload is [`ScopeReleasedPayload`].
    ScopeReleased,
    /// An agent tool is about to run. Served at the tool boundary.
    BeforeToolUse,
    /// An agent tool returned. Served at the tool boundary.
    AfterToolUse,
    /// An agent tool failed. Served at the tool boundary.
    AfterToolFailure,
}

/// What a hook decided, in the vocabulary the driver can act on. A service
/// returns only the variants its point consumes; the adapter ignores a
/// decision a point cannot take and records that it did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum HookDecision {
    Proceed,
    /// Skip the visit with this status (`BeforeVisit`, `BeforeAttempt`).
    Skip {
        status: Status,
    },
    /// Block: the attempt fails (`BeforeVisit`, `BeforeAttempt`) or the
    /// transition is refused (`RouteSelected`).
    Block {
        reason: String,
    },
    /// Replace a routing group's selected edge (`RouteSelected`).
    Override {
        group: u32,
        edge:  EdgeId,
    },
    /// Make this status the effective result (`AfterAttempt`).
    Adjust {
        status: Status,
        reason: String,
    },
}

/// What a hook's own model and tool work cost. A prompt hook makes one
/// request; an agent hook makes one per model turn and runs tools. `usage`
/// is the backend's accounting in the shape the native agent reports under
/// `pebble.usage`, lithos-llm's `Usage`: `tokens` (`input`, `output`,
/// `reasoning`, `cache_read`, `cache_write`) and, when every answer that
/// used tokens was priced, `cost` (`usd_micros`, `source`). It is absent
/// when no answer arrived (a timeout, a failed request).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HookUsage {
    /// Model requests the hook made, answered or not.
    pub requests:     u64,
    /// Tool calls the hook's agent started; none for a prompt hook.
    #[serde(default)]
    pub tool_calls:   u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage:        Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_ms:      Option<u64>,
}

/// One hook the service ran, or could not run, for the record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookRun {
    pub name:        String,
    /// `executed`, `skipped`, `failed_open`, `unsupported`.
    pub state:       SmolStr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message:     Option<String>,
    /// What the hook's own model and tool work cost, when it did any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage:       Option<HookUsage>,
    /// The span the hook's model requests were sent under, beneath the
    /// step attempt's span: a prompt or agent hook run for a step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace:       Option<ChildSpan>,
}

/// The hook operation an activity record belongs to: the point and the
/// hook's name. With the firing and attempt the record is attributed to,
/// this identifies one execution of one hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookOperation {
    pub point: HookPoint,
    pub hook:  String,
}

/// One event a hook's own agent produced, for the record. `backend` names
/// the agent backend and `envelope` is the backend's own event, as the
/// stage's agent activity carries them; the operation says which hook.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookActivity {
    pub hook:     HookOperation,
    pub backend:  SmolStr,
    pub envelope: Value,
}

/// The service's answer for one point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookReport {
    pub point:    HookPoint,
    pub decision: HookDecision,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks:    Vec<HookRun>,
    /// Problems that fail open: recorded, execution continues.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The activity of the hooks' own agents, in order. Not part of the
    /// report's own record: the caller records each entry as its own
    /// [`HOOK_ACTIVITY_NOTE_KIND`] note before the report.
    #[serde(skip)]
    pub activity: Vec<HookActivity>,
}

impl HookReport {
    pub fn proceed(point: HookPoint) -> Self {
        Self {
            point,
            decision: HookDecision::Proceed,
            hooks: Vec::new(),
            warnings: Vec::new(),
            activity: Vec::new(),
        }
    }

    /// The notes that record this report: one per activity entry, then the
    /// report itself when it says something.
    pub fn notes(&self) -> Vec<Note> {
        let mut notes: Vec<Note> = self
            .activity
            .iter()
            .map(|activity| {
                Note::new(
                    HOOK_ACTIVITY_NOTE_KIND,
                    serde_json::to_value(activity).unwrap_or(Value::Null),
                )
            })
            .collect();
        if !self.is_silent() {
            notes.push(Note::new(
                HOOK_NOTE_KIND,
                serde_json::to_value(self).unwrap_or(Value::Null),
            ));
        }
        notes
    }

    /// Whether anything happened worth a record.
    pub fn is_silent(&self) -> bool {
        self.decision == HookDecision::Proceed && self.hooks.is_empty() && self.warnings.is_empty()
    }
}

/// The payload of a [`HookPoint::RunFinished`] request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFinishedPayload {
    pub status:  ir::RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// The payload of a [`HookPoint::ScopeReleased`] request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeReleasedPayload {
    pub scope:   ir::ScopeId,
    /// `succeeded` or `failed`: the scope outcome retention is decided on.
    pub outcome: SmolStr,
}

/// The payload of a [`HookPoint::ScopeReady`] request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeReadyPayload {
    pub scope:     ir::ScopeId,
    /// The workspace path inside the environment.
    pub workspace: String,
}

/// The payload of a [`HookPoint::ForkCompleted`] request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkCompletedPayload {
    /// The fork node's instance name: the node whose branches joined.
    pub fork: SmolStr,
}

/// What a service is asked about.
pub struct HookRequest {
    pub point:    HookPoint,
    /// The firing the point belongs to. `None` for the run-level points
    /// (`RunFinished`, `ScopeReleased`), which have no firing. A point a
    /// step asks itself carries the view the step built from its context:
    /// the node, its `meta`, the firing, attempt and scope, and the step's
    /// resolved config.
    pub view:     Option<Arc<FiringView>>,
    /// The attempt's outcome (`AfterAttempt`, `AfterVisit`, `RouteSelected`).
    pub outcome:  Option<Outcome>,
    /// The selected routes (`RouteSelected`), one decision per group.
    pub routes:   Vec<(u32, RouteDecision)>,
    /// Point-specific data a caller at the tool boundary supplies: the tool
    /// name and input, the result. Opaque here.
    pub payload:  Value,
    /// The run, invocation and execution the point runs in, when the caller
    /// knows them. With the view's firing and attempt it names the step
    /// attempt's span in the run's trace ([`crate::StepTrace`]), which a
    /// hook that calls a model sends its requests beneath. The adapter
    /// always knows them; a step that asks for itself passes its
    /// [`ExecutionIdentity`], which a driver outside the coordinator lacks.
    pub identity: Option<ExecutionIdentity>,
}

/// The one interface a hook executor implements. The local implementation
/// runs configured command, HTTP, prompt and agent hooks; an embedding host
/// runs its own. Exactly one service is installed per run.
///
/// Implementations must return within the point's timeout policy, apply
/// their own fail-open rules, and never call back into the run. Cancellation
/// of the run cancels the awaiting caller; the service should stop its own
/// work when the future is dropped.
#[async_trait::async_trait]
pub trait HookService: Send + Sync {
    async fn run(&self, request: HookRequest) -> HookReport;

    /// The names of the hooks configured for `point`, whatever their
    /// matcher, for a caller that must warn about a boundary it cannot
    /// serve (an external agent that exposes no tool boundary for them).
    /// The default is none: such a caller then warns about nothing, and
    /// still asks the service at every boundary it does have.
    fn configured_hooks(&self, point: HookPoint) -> Vec<String> {
        let _ = point;
        Vec::new()
    }
}

/// The capability type steps and tool middleware look the service up by.
pub struct HookServiceHandle(pub Arc<dyn HookService>);

/// A service with no hooks: every point proceeds. The standalone default.
pub struct NoHooks;

#[async_trait::async_trait]
impl HookService for NoHooks {
    async fn run(&self, request: HookRequest) -> HookReport {
        HookReport::proceed(request.point)
    }
}

/// The one caller of a [`HookService`] for workflow points.
pub struct HookAdapter {
    service: Arc<dyn HookService>,
}

impl HookAdapter {
    pub fn new(service: Arc<dyn HookService>) -> Self {
        Self { service }
    }
}

/// The execution a driver-asked point runs in.
fn identity(context: &HookContext) -> ExecutionIdentity {
    ExecutionIdentity {
        run:        context.run_key.clone(),
        invocation: context.invocation,
        execution:  context.execution,
    }
}

/// Whether a node is a lowering artifact rather than a stage: the frontend
/// marked it `synthetic` (a goal check, a parallel branch's delegate node, a
/// synthetic fan-in). No hook point fires for one; its stage, when it has
/// one, runs elsewhere with hooks of its own.
fn is_synthetic(view: &FiringView) -> bool {
    view.meta().get("synthetic") == Some(&Value::Bool(true))
}

/// Whether the node's own step drives its admission points: the frontend
/// marked it [`ADMISSION_HOOKS_META`]` = "step"`. The driver admits the
/// first firing of a scope before the scope's environment exists, so a
/// node that must run its admission hooks with the environment in place (a
/// hook placed in the sandbox) asks the service from its step instead, in
/// its own order. The adapter then stays away from those two points, so
/// each is asked once.
fn step_admits(view: &FiringView) -> bool {
    view.meta()
        .get(ADMISSION_HOOKS_META)
        .and_then(Value::as_str)
        == Some(ADMISSION_HOOKS_BY_STEP)
}

#[async_trait::async_trait]
impl ExecutionHooks for HookAdapter {
    async fn before_attempt(
        &self,
        context: &HookContext,
        request: AdmitAttempt,
    ) -> AttemptDecision {
        if is_synthetic(&request.view) || step_admits(&request.view) {
            return AttemptDecision::admit();
        }
        let mut notes = Vec::new();
        let mut admission = Admission::Admit;
        let points = if request.view.attempt == ir::Attempt::FIRST {
            vec![HookPoint::BeforeVisit, HookPoint::BeforeAttempt]
        } else {
            vec![HookPoint::Retrying, HookPoint::BeforeAttempt]
        };
        for point in points {
            let report = self
                .service
                .run(HookRequest {
                    point,
                    view: Some(request.view.clone()),
                    outcome: None,
                    routes: Vec::new(),
                    payload: Value::Null,
                    identity: Some(identity(context)),
                })
                .await;
            notes.extend(report.notes());
            match report.decision {
                HookDecision::Skip { status } => {
                    admission = Admission::Skip {
                        outcome: Outcome::new(status, Value::Null),
                    };
                    break;
                }
                HookDecision::Block { reason } => {
                    admission = Admission::Block {
                        reason: SmolStr::new(reason),
                    };
                    break;
                }
                HookDecision::Proceed
                | HookDecision::Override { .. }
                | HookDecision::Adjust { .. } => {}
            }
        }
        AttemptDecision { admission, notes }
    }

    async fn prepare_result(
        &self,
        context: &HookContext,
        request: PrepareResult,
    ) -> Result<Prepared, PrepareError> {
        if is_synthetic(&request.view) {
            return Ok(Prepared::unchanged());
        }
        let report = self
            .service
            .run(HookRequest {
                point:    HookPoint::AfterAttempt,
                view:     Some(request.view.clone()),
                outcome:  Some(request.outcome.clone()),
                routes:   Vec::new(),
                payload:  Value::Null,
                identity: Some(identity(context)),
            })
            .await;
        let mut prepared = Prepared::unchanged();
        prepared.notes.extend(report.notes());
        if let HookDecision::Adjust { status, reason } = report.decision {
            prepared.adjustment.status = Some(status);
            prepared.adjustment.reason = Some(reason);
        }
        Ok(prepared)
    }

    async fn after_record(&self, context: &HookContext, recorded: Recorded) -> Vec<Note> {
        if is_synthetic(&recorded.view) {
            return Vec::new();
        }
        let report = self
            .service
            .run(HookRequest {
                point:    HookPoint::AfterVisit,
                view:     Some(recorded.view),
                outcome:  Some(recorded.outcome),
                routes:   Vec::new(),
                payload:  Value::Null,
                identity: Some(identity(context)),
            })
            .await;
        report.notes()
    }

    async fn transition(
        &self,
        context: &HookContext,
        transition: Transition,
    ) -> Result<TransitionReport, TransitionError> {
        if is_synthetic(&transition.view) {
            return Ok(TransitionReport::default());
        }
        let report = self
            .service
            .run(HookRequest {
                point:    HookPoint::RouteSelected,
                view:     Some(transition.view),
                outcome:  Some(transition.outcome),
                routes:   transition
                    .groups
                    .iter()
                    .map(|group| (group.group, group.decision.clone()))
                    .collect(),
                payload:  Value::Null,
                identity: Some(identity(context)),
            })
            .await;
        let mut out = TransitionReport {
            problems: report.warnings.clone(),
            ..TransitionReport::default()
        };
        out.notes.extend(report.notes());
        match report.decision {
            HookDecision::Block { reason } => Err(TransitionError::new(reason)),
            HookDecision::Override { group, edge } => {
                out.overrides.push(RouteOverride { group, edge });
                Ok(out)
            }
            _ => Ok(out),
        }
    }

    async fn run_finished(&self, _context: &HookContext, finished: RunFinished) -> Vec<Note> {
        let report = self
            .run_level(HookPoint::RunFinished, RunFinishedPayload {
                status:  finished.status,
                failure: finished.failure,
            })
            .await;
        Self::log_run_level(&report);
        report.notes()
    }

    async fn scope_released(&self, _context: &HookContext, released: ScopeReleased) -> Vec<Note> {
        let report = self
            .run_level(HookPoint::ScopeReleased, ScopeReleasedPayload {
                scope:   released.scope,
                outcome: SmolStr::new(match released.outcome {
                    executor::ScopeOutcome::Succeeded => "succeeded",
                    executor::ScopeOutcome::Failed => "failed",
                }),
            })
            .await;
        Self::log_run_level(&report);
        report.notes()
    }
}

impl HookAdapter {
    /// A point with no firing: the payload is the whole request. No decision
    /// is consumed. The report comes back as the same `hook` note a firing's
    /// report is; the driver hands it to the coordinator, which records it
    /// at run level (`CoordinatorEvent::RunNoteRecorded`), so `replay_run` and
    /// `petri inspect` show it.
    async fn run_level(&self, point: HookPoint, payload: impl Serialize) -> HookReport {
        self.service
            .run(HookRequest {
                point,
                view: None,
                outcome: None,
                routes: Vec::new(),
                payload: serde_json::to_value(payload).unwrap_or(Value::Null),
                identity: None,
            })
            .await
    }

    fn log_run_level(report: &HookReport) {
        if report.is_silent() {
            return;
        }
        tracing::info!(
            point = ?report.point,
            hooks = report.hooks.len(),
            warnings = ?report.warnings,
            "run-level hooks ran"
        );
    }
}
