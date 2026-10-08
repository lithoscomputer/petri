//! Read-only reconstruction of a run directory: the `petri inspect` document.
//!
//! The source of truth is the set of durable files the coordinator wrote:
//! `run.json`, `coordinator.jsonl`, every `graphs/<digest>.json`, and one
//! `events.jsonl` per execution. [`inspect_run`] decodes them under the same
//! rules the coordinator resumes under (the strict torn-line rule, the
//! registered graph's digest, the coordinator state machine) and replays each
//! engine log through [`engine::replay`] to derive its run context. It never
//! starts a step, acquires a sandbox, contacts a provider, takes the run
//! lease, or writes under the run directory. Run context is derived on every
//! call: there is no second checkpoint that can drift from the log.
//!
//! The document is [`RunInspection`], versioned by
//! [`INSPECT_FORMAT_VERSION`]. The field-level contract is in
//! `crates/core/execution/INSPECT.md`. Two rules shape it:
//!
//! - Nothing incomplete is presented as a final snapshot. `complete` is true
//!   only when the run recorded its finish, every log decoded whole, and every
//!   execution's replay reproduced its log byte for byte. Anything less is
//!   listed under `incomplete`, and the document still says what the logs do
//!   support.
//! - A record that decodes wrongly, a graph whose bytes do not match their
//!   digest, an unsupported format version, or a log that diverges from replay
//!   is an [`InspectError`], never a document.
//!
//! Values come out of the logs exactly as the driver appended them, after
//! masking: a secret reference stays a `{"$secret": name}` reference and a
//! masked value stays `***`. Nothing here resolves a secret.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use engine::{
    EngineExit, EngineState, EntryPoint, Event, EventLog, MiddlewareKey, ReplayMismatch,
    RouteApplied,
};
use ir::{Attempt, Control, FailureInfo, FiringId, Graph, NodeId, Value};
use serde::Serialize;
use smol_str::SmolStr;
use store::{Access, RunLogs};

use crate::host::ForkOrigin;
use crate::interview::{InterviewReceipt, RECEIPT_FILE};
use crate::state::RunNote;
use crate::store::load_graph_registry;
use crate::{
    CancelReason, CoordinatorEvent, CoordinatorState, EngineLogError, ExecutionId, ExecutionState,
    GraphDigest, InvocationId, InvocationState, SandboxBinding, SecretBinding, SecretBindings,
    StateError, StoreError, open_run_dir, read_coordinator_log, read_execution_log,
};

/// The version of the [`RunInspection`] document. Bumped whenever a field
/// changes shape or meaning; additions that keep every existing field intact
/// do not bump it. Version 2 spells every enum the document carries from a
/// record (`Admission`, `RouteDecision`, `EngineExit`, `EntryPoint`,
/// `Status`, `RunStatus`) with snake-case tags, as the logs do. Version 3
/// reads the run through its store: `run_dir` became `locator`, `run_key`
/// names the run, and a log's `path` and `torn` are gone (a torn tail is the
/// run directory's own business, dropped before the run is read).
pub const INSPECT_FORMAT_VERSION: u32 = 3;

/// Why a run directory could not be reconstructed.
///
/// Every variant means the files do not support a trustworthy document. An
/// interrupted run is not an error: it comes back as a [`RunInspection`] with
/// `complete: false`.
#[derive(Debug, thiserror::Error)]
pub enum InspectError {
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{path}` is not run metadata: {source}")]
    BadMetadata {
        path:   PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// `interviews.json` exists but is not an interview receipt this build
    /// reads.
    #[error("`{path}` is not an interview receipt: {source}")]
    BadReceipt {
        path:   PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("run format {found} is unsupported; this build reads format {expected}")]
    UnsupportedFormat { found: u32, expected: u32 },
    /// The coordinator log or a registered graph failed the store's own
    /// decode rules: an undecodable record, a missing graph, a digest that
    /// does not match its bytes, a graph that fails validation.
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the coordinator log does not replay: {0}")]
    State(#[from] StateError),
    #[error(transparent)]
    EngineLog(#[from] EngineLogError),
    #[error("execution {execution} log diverges from replay: {source}")]
    ReplayDiverged {
        execution: ExecutionId,
        #[source]
        source:    ReplayMismatch,
    },
    #[error("execution {execution} names unregistered graph {graph}")]
    UnknownGraph {
        execution: ExecutionId,
        graph:     GraphDigest,
    },
    #[error("could not encode an event record for comparison: {0}")]
    Encode(#[source] serde_json::Error),
}

/// The whole document. See the module docs for the rules it follows.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunInspection {
    /// [`INSPECT_FORMAT_VERSION`].
    pub inspect_format_version: u32,
    /// The run directory's own format, from `run.json`.
    pub coordinator_format_version: u32,
    /// Where the run lives, as its store names it: a run directory's path,
    /// or a database and an id.
    pub locator: String,
    /// The run's identity in its store and on its sandbox providers.
    pub run_key: store::RunKey,
    /// Where the run was forked from, when it was seeded from another run's
    /// records: the source key, the position and whether the position's
    /// firing runs again (`FORK.md`). `null` on a run that started fresh.
    /// Additive in format version 3.
    pub forked_from: Option<ForkOrigin>,
    /// True only when `incomplete` is empty: the run recorded its finish and
    /// every execution's log decoded whole and replayed byte-identically.
    pub complete: bool,
    /// The recorded run status: `success`, `failed` or `cancelled`. Absent
    /// until the run has recorded its finish.
    pub status: Option<String>,
    /// Required host work declared for this run.
    pub required_finalization: bool,
    /// The committed failure of that work. Execution evidence stays in the
    /// root invocation and its engine log.
    pub finalization_failure: Option<ir::FinalizationFailure>,
    /// Every reason `complete` is false, in the order found.
    pub incomplete: Vec<String>,
    /// Whether the last recorded run control was a pause: a resume starts
    /// with admission held. Additive in format version 1.
    pub paused: bool,
    /// Every run-level note, in record order: a `hook` note is a run-level
    /// hook report (`run_finished`, `scope_released`), with the execution
    /// whose driver ran the point. Additive in format version 1.
    #[serde(default)]
    pub notes: Vec<RunNote>,
    pub root: RootInspection,
    pub middleware_chain: Vec<MiddlewareKey>,
    /// Every registered graph digest, in digest order.
    pub graphs: Vec<GraphDigest>,
    /// Every invocation, in id order. The root is first.
    pub invocations: Vec<InvocationInspection>,
    /// Every execution, in id order, which is declaration order across the
    /// whole run.
    pub executions: Vec<ExecutionInspection>,
    /// The interview receipt the host wrote beside the run
    /// (`interviews.json`), or `null` when the run had no interviewer.
    /// Sensitive answers appear only as `$secret` references.
    pub interviews: Option<InterviewReceipt>,
}

/// Which execution speaks for the root invocation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RootInspection {
    pub invocation:       InvocationId,
    /// The execution the root's recorded result names. Absent until the root
    /// invocation has finished; then it is always the root's last execution.
    pub final_execution:  Option<ExecutionId>,
    /// The root's most recently declared execution, finished or not.
    pub latest_execution: Option<ExecutionId>,
}

/// One invocation: its declaration, its place in the tree, and its result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct InvocationInspection {
    pub invocation:       InvocationId,
    /// `declared` (no execution yet), `running` (no result yet) or
    /// `finished`.
    pub status:           &'static str,
    /// The call that declared this invocation; absent on the root.
    pub parent:           Option<ParentCall>,
    /// Invocations whose call site lies in one of this invocation's
    /// executions, in id order.
    pub children:         Vec<InvocationId>,
    pub graph:            GraphDigest,
    /// The context the invocation was declared with.
    pub context:          BTreeMap<SmolStr, Value>,
    pub secrets:          SecretsInspection,
    /// `isolated` or `inherited`.
    pub sandbox:          &'static str,
    pub cancel_requested: bool,
    /// Why, when the requester said: `interrupt`, `control`, or
    /// `stall_timeout` with its budget and idle time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_reason:    Option<CancelReason>,
    /// Every execution of this invocation in order; each after the first
    /// followed a `loop_restart`.
    pub executions:       Vec<ExecutionId>,
    /// The execution the result names. Absent until finished.
    pub final_execution:  Option<ExecutionId>,
    pub result:           Option<ResultInspection>,
}

/// The durable call key that declared a nested invocation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ParentCall {
    pub invocation: InvocationId,
    pub execution:  ExecutionId,
    pub firing:     u64,
    pub attempt:    u32,
    pub slot:       SmolStr,
}

/// A child's name-only secret bindings. No value is representable here.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SecretsInspection {
    /// `none`, `inherit` or `explicit`.
    pub mode:  &'static str,
    /// The names an `explicit` binding exposes to the child.
    pub names: Vec<SmolStr>,
}

/// An invocation's one durable result.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResultInspection {
    /// `success`, `failed` or `cancelled`.
    pub status:          String,
    pub failure:         Option<FailureInfo>,
    pub final_execution: ExecutionId,
    pub output:          Value,
    /// The final execution's `kv`, as the coordinator recorded it.
    pub context:         BTreeMap<SmolStr, Value>,
}

/// One execution: its declaration, its log, and the state replayed from it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExecutionInspection {
    pub execution:       ExecutionId,
    pub invocation:      InvocationId,
    /// Position within the invocation, from 0.
    pub execution_index: u32,
    pub predecessor:     Option<ExecutionId>,
    pub successor:       Option<ExecutionId>,
    /// `finished` (a terminal exit is recorded), `restarted` (a restart exit
    /// is recorded and a successor follows) or `incomplete`.
    pub status:          &'static str,
    /// The node a restart successor starts at. Absent for the graph's own
    /// entries.
    pub entry_node:      Option<SmolStr>,
    /// The context the execution started with.
    pub start_context:   BTreeMap<SmolStr, Value>,
    /// The exit the coordinator recorded for this execution.
    pub exit:            Option<ExitInspection>,
    pub log:             LogInspection,
    /// The state replayed from the log. Absent when there is no log.
    pub engine:          Option<EngineInspection>,
    /// Invocations whose call site is a firing of this execution.
    pub children:        Vec<InvocationId>,
}

/// How an execution ended.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExitInspection {
    Terminal {
        /// `success`, `failed` or `cancelled`.
        status: String,
    },
    Restart {
        edge:          u32,
        target:        u32,
        target_name:   Option<SmolStr>,
        source_firing: u64,
    },
}

/// The execution's engine log and how far replay trusted it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogInspection {
    /// Records stored.
    pub records: usize,
    /// `verified` (replay reproduced the log exactly), `prefix` (the log is
    /// a byte-prefix of what replay regenerates: a crash landed between an
    /// external append and the flush of its derived records) or `missing`
    /// (nothing stored for the execution).
    pub replay:  &'static str,
}

/// What replaying one execution's log yields.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EngineInspection {
    pub started:       bool,
    pub finished:      bool,
    /// The root cancel scope was cancelled.
    pub cancelled:     bool,
    /// The exit replay derived. Compare with the coordinator's `exit`.
    pub exit:          Option<ExitInspection>,
    /// The run status folded from node outcomes as the state stands:
    /// `success`, `failed` or `cancelled`.
    pub folded_status: String,
    /// Engine errors, rendered.
    pub errors:        Vec<String>,
    pub context:       ContextInspection,
    /// Every final firing record in completion order: the routing-visible
    /// history. One entry per firing.
    pub history:       Vec<HistoryInspection>,
    /// Every `StepFinished` record, final or not, in log order. Retries show
    /// up here and nowhere else.
    pub attempts:      Vec<AttemptInspection>,
    /// Every applied route, in log order.
    pub routes:        Vec<RouteInspection>,
    /// Every control the host delivered into a firing, in log order: a human
    /// gate's answer, a supervisor's steering, a cancel. Payloads are as
    /// logged, so a sensitive answer is its `$secret` reference.
    pub deliveries:    Vec<DeliveryInspection>,
    /// Firings still live when the log ends. Empty for a finished execution.
    pub live:          Vec<LiveInspection>,
}

/// The run context expressions saw: `kv.*` and `nodes.*`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextInspection {
    pub kv:    BTreeMap<SmolStr, Value>,
    /// Keyed by node instance name (`build`, `build#2`).
    pub nodes: BTreeMap<SmolStr, NodeInspection>,
}

/// The engine's node-instance record: what one instance left behind.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NodeInspection {
    /// The final attempt's status tag.
    pub status:       &'static str,
    pub success_like: bool,
    pub failure:      Option<FailureInfo>,
    pub output:       Value,
    /// The latest generation to complete.
    pub generation:   u32,
    /// How many attempts the final firing took.
    pub attempts:     u32,
}

/// One final firing record.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HistoryInspection {
    pub firing:          u64,
    pub node:            SmolStr,
    pub node_id:         u32,
    pub generation:      u32,
    pub attempt:         u32,
    pub status:          &'static str,
    pub failure:         Option<FailureInfo>,
    pub output:          Value,
    pub context_updates: BTreeMap<SmolStr, Value>,
}

/// One `StepFinished` record, whichever attempt it was.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AttemptInspection {
    /// The record's position in the execution's log.
    pub seq:        u64,
    pub firing:     u64,
    pub node:       Option<SmolStr>,
    pub generation: Option<u32>,
    pub attempt:    u32,
    pub status:     &'static str,
    pub failure:    Option<FailureInfo>,
    /// True when this outcome is the firing's routing-visible one.
    #[serde(rename = "final")]
    pub is_final:   bool,
}

/// One applied route.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RouteInspection {
    pub seq:    u64,
    pub firing: u64,
    pub node:   Option<SmolStr>,
    /// `edge` or `none`.
    pub kind:   &'static str,
    pub group:  u32,
    pub edge:   Option<u32>,
    /// The node the route led to.
    pub target: Option<SmolStr>,
}

/// One `ControlRequested` record.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeliveryInspection {
    pub seq:     u64,
    pub firing:  u64,
    pub node:    Option<SmolStr>,
    /// `deliver`, `cancel`, `kill`, or `other` for a control this build does
    /// not name.
    pub kind:    &'static str,
    /// The delivered value, for `deliver`. A sensitive value crosses as
    /// `{"$secret": "<name>"}` and is shown as that reference.
    pub payload: Option<Value>,
}

/// A firing the log leaves live.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LiveInspection {
    pub firing:         u64,
    pub node:           Option<SmolStr>,
    pub generation:     u32,
    pub attempt:        u32,
    pub started:        bool,
    pub awaiting_retry: bool,
    pub cancelling:     bool,
}

/// Reconstruct a run from its store, through a handle opened for reading.
///
/// # Errors
///
/// Fails when the records do not support a trustworthy reconstruction: a
/// run with no declaration, an unsupported format version, a record that
/// does not decode, a coordinator log that does not replay, a registered
/// graph that is missing or does not match its digest, an engine log with
/// an undecodable record, or an engine log that diverges from its replay.
/// An interrupted run is not an error; see [`RunInspection::incomplete`].
/// The interview receipt is a file beside a run directory: see
/// [`inspect_run_dir`] and [`RunInspection::with_interviews`].
pub async fn inspect_run(logs: &dyn RunLogs) -> Result<RunInspection, InspectError> {
    let records = read_coordinator_log(logs)
        .await
        .map_err(|error| match error {
            StoreError::UnsupportedFormat { found, expected } => {
                InspectError::UnsupportedFormat { found, expected }
            }
            other => InspectError::Store(other),
        })?;
    let (format_version, run_key, forked_from) = match records.first().map(|record| &record.body) {
        Some(CoordinatorEvent::RunStarted {
            format_version,
            key,
            forked_from,
            ..
        }) => (*format_version, key.clone(), forked_from.clone()),
        _ => return Err(InspectError::State(StateError::MissingRunStart)),
    };
    let state = CoordinatorState::replay(&records)?;
    let graphs = load_graph_registry(logs, &state).await?;

    let mut incomplete = Vec::new();
    if state.run_status.is_none() {
        incomplete.push("the run has not recorded its finish".to_owned());
    }

    let children = children_by_execution(&state);
    let mut executions = Vec::with_capacity(state.executions.len());
    for execution in state.executions.values() {
        executions.push(
            inspect_execution(logs, &state, &graphs, execution, &children, &mut incomplete).await?,
        );
    }
    let invocations = state
        .invocations
        .values()
        .map(|invocation| inspect_invocation(&state, invocation, &mut incomplete))
        .collect();

    let root_id = state.root.ok_or(StateError::MissingRunStart)?;
    let root = state
        .invocations
        .get(&root_id)
        .ok_or(StateError::UnknownInvocation(root_id))?;
    Ok(RunInspection {
        inspect_format_version: INSPECT_FORMAT_VERSION,
        coordinator_format_version: format_version,
        locator: logs.locator(),
        run_key,
        forked_from,
        complete: incomplete.is_empty(),
        status: state.run_status.map(|status| status.to_string()),
        required_finalization: state.required_finalization,
        finalization_failure: state.finalization_failure.clone(),
        incomplete,
        paused: state.paused,
        notes: state.run_notes.clone(),
        root: RootInspection {
            invocation:       root_id,
            final_execution:  root.result.as_ref().map(|result| result.final_execution),
            latest_execution: root.executions.last().copied(),
        },
        middleware_chain: state.middleware_chain.clone(),
        graphs: state.graphs.iter().copied().collect(),
        invocations,
        executions,
        interviews: None,
    })
}

/// [`inspect_run`] over the run directory at `run_dir`, with the interview
/// receipt the host wrote beside it (`interviews.json`), when there is one.
pub async fn inspect_run_dir(run_dir: &Path) -> Result<RunInspection, InspectError> {
    let logs = open_run_dir(run_dir, Access::Read).await?;
    let inspection = inspect_run(&*logs).await?;
    Ok(inspection.with_interviews(read_receipt(run_dir)?))
}

impl RunInspection {
    /// Attach the interview receipt a host kept beside the run.
    #[must_use]
    pub fn with_interviews(mut self, interviews: Option<InterviewReceipt>) -> Self {
        self.interviews = interviews;
        self
    }
}

/// The interview receipt beside a run directory, when the host wrote one.
/// A run without an interviewer writes none, so a missing file is not an
/// error; a file that is not a receipt is.
pub fn read_receipt(run_dir: &Path) -> Result<Option<InterviewReceipt>, InspectError> {
    let path = run_dir.join(RECEIPT_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(InspectError::Io {
                action: "read",
                path,
                source,
            });
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| InspectError::BadReceipt { path, source })
}

/// Nested invocations keyed by the execution their call site lies in.
fn children_by_execution(state: &CoordinatorState) -> BTreeMap<ExecutionId, Vec<InvocationId>> {
    let mut children: BTreeMap<ExecutionId, Vec<InvocationId>> = BTreeMap::new();
    for (id, invocation) in &state.invocations {
        if let Some(call) = &invocation.declaration.call {
            children.entry(call.parent).or_default().push(*id);
        }
    }
    children
}

fn inspect_invocation(
    state: &CoordinatorState,
    invocation: &InvocationState,
    incomplete: &mut Vec<String>,
) -> InvocationInspection {
    let id = invocation.declaration.id;
    let parent = invocation.declaration.call.as_ref().map(|call| ParentCall {
        invocation: state
            .executions
            .get(&call.parent)
            .map_or(InvocationId::ROOT, |execution| {
                execution.declaration.invocation
            }),
        execution:  call.parent,
        firing:     call.firing.raw(),
        attempt:    call.attempt.raw(),
        slot:       call.slot.clone(),
    });
    let children = state
        .invocations
        .values()
        .filter(|candidate| {
            candidate.declaration.call.as_ref().is_some_and(|call| {
                state
                    .executions
                    .get(&call.parent)
                    .is_some_and(|execution| execution.declaration.invocation == id)
            })
        })
        .map(|candidate| candidate.declaration.id)
        .collect();
    let status = match (&invocation.result, invocation.executions.is_empty()) {
        (Some(_), _) => "finished",
        (None, false) => {
            incomplete.push(format!("invocation {id} has not finished"));
            "running"
        }
        (None, true) => {
            incomplete.push(format!("invocation {id} is declared but never ran"));
            "declared"
        }
    };
    let secrets = match &invocation.declaration.secret_bindings {
        SecretBindings::None => SecretsInspection {
            mode:  "none",
            names: Vec::new(),
        },
        SecretBindings::Inherit => SecretsInspection {
            mode:  "inherit",
            names: Vec::new(),
        },
        SecretBindings::Explicit(bindings) => SecretsInspection {
            mode:  "explicit",
            names: bindings
                .iter()
                .map(|(name, binding)| match binding {
                    SecretBinding::Parent(_) | SecretBinding::Empty => name.clone(),
                })
                .collect(),
        },
    };
    InvocationInspection {
        invocation: id,
        status,
        parent,
        children,
        graph: invocation.declaration.graph,
        context: invocation.declaration.context.clone(),
        secrets,
        sandbox: match invocation.declaration.sandbox {
            SandboxBinding::Isolated => "isolated",
            SandboxBinding::Inherited { .. } => "inherited",
        },
        cancel_requested: invocation.cancelled,
        cancel_reason: invocation.cancel_reason.clone(),
        executions: invocation.executions.clone(),
        final_execution: invocation
            .result
            .as_ref()
            .map(|result| result.final_execution),
        result: invocation.result.as_ref().map(|result| ResultInspection {
            status:          result.status.to_string(),
            failure:         result.failure.clone(),
            final_execution: result.final_execution,
            output:          result.output.clone(),
            context:         result.context.clone(),
        }),
    }
}

async fn inspect_execution(
    logs: &dyn RunLogs,
    state: &CoordinatorState,
    graphs: &BTreeMap<GraphDigest, Arc<Graph>>,
    execution: &ExecutionState,
    children: &BTreeMap<ExecutionId, Vec<InvocationId>>,
    incomplete: &mut Vec<String>,
) -> Result<ExecutionInspection, InspectError> {
    let id = execution.declaration.id;
    let invocation_id = execution.declaration.invocation;
    let invocation = state
        .invocations
        .get(&invocation_id)
        .ok_or(StateError::UnknownInvocation(invocation_id))?;
    let digest = invocation.declaration.graph;
    let graph = graphs.get(&digest).ok_or(InspectError::UnknownGraph {
        execution: id,
        graph:     digest,
    })?;
    let position = invocation
        .executions
        .iter()
        .position(|candidate| *candidate == id);
    let predecessor = position
        .and_then(|index| index.checked_sub(1))
        .and_then(|index| invocation.executions.get(index).copied());
    let successor = position.and_then(|index| invocation.executions.get(index + 1).copied());

    let decoded = read_execution_log(logs, id).await?;
    let (log, engine) = if decoded.log.is_empty() {
        incomplete.push(format!("execution {id}: no engine log was stored"));
        (
            LogInspection {
                records: 0,
                replay:  "missing",
            },
            None,
        )
    } else {
        let (engine_state, coverage) = replay_log(graph, &decoded.log, id)?;
        if coverage == Coverage::Prefix {
            incomplete.push(format!(
                "execution {id}: the log ends before the core's derived records"
            ));
        }
        (
            LogInspection {
                records: decoded.log.len(),
                replay:  match coverage {
                    Coverage::Verified => "verified",
                    Coverage::Prefix => "prefix",
                },
            },
            Some(inspect_engine(&engine_state, &decoded.log)),
        )
    };

    let status = match &execution.exit {
        Some(EngineExit::Terminal { .. }) => "finished",
        Some(EngineExit::Restart { .. }) => "restarted",
        None => {
            incomplete.push(format!("execution {id}: no exit is recorded"));
            "incomplete"
        }
    };
    if let (Some(recorded), Some(replayed)) = (
        &execution.exit,
        engine.as_ref().and_then(|engine| engine.exit.as_ref()),
    ) && &exit_inspection(recorded, graph) != replayed
    {
        incomplete.push(format!(
            "execution {id}: the recorded exit disagrees with the replayed log"
        ));
    }

    Ok(ExecutionInspection {
        execution: id,
        invocation: invocation_id,
        execution_index: execution.declaration.start.execution_index,
        predecessor,
        successor,
        status,
        entry_node: match execution.declaration.start.entry {
            EntryPoint::GraphEntries => None,
            EntryPoint::Node(node) => Some(
                node_name(graph, node).unwrap_or_else(|| SmolStr::new(format!("#{}", node.raw()))),
            ),
        },
        start_context: execution.declaration.start.context.clone(),
        exit: execution
            .exit
            .as_ref()
            .map(|exit| exit_inspection(exit, graph)),
        log,
        engine,
        children: children.get(&id).cloned().unwrap_or_default(),
    })
}

/// How much of a log its replay stood behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coverage {
    /// Replay regenerated exactly the log's records.
    Verified,
    /// The log is a byte-prefix of the regenerated one.
    Prefix,
}

/// Replay the log and hold it to the engine's own rule: the loaded log must
/// be a byte-prefix of the regenerated one. Anything else is divergence.
fn replay_log(
    graph: &Graph,
    log: &EventLog,
    execution: ExecutionId,
) -> Result<(EngineState, Coverage), InspectError> {
    let state = engine::replay(graph.clone(), log);
    let rebuilt = &state.log;
    let mismatch = |first_divergence| InspectError::ReplayDiverged {
        execution,
        source: ReplayMismatch {
            original_records: log.len(),
            replayed_records: rebuilt.len(),
            first_divergence,
        },
    };
    for (original, regenerated) in log.records().iter().zip(rebuilt.records()) {
        let original_bytes = serde_json::to_vec(original).map_err(InspectError::Encode)?;
        let regenerated_bytes = serde_json::to_vec(regenerated).map_err(InspectError::Encode)?;
        if original_bytes != regenerated_bytes {
            return Err(mismatch(Some(original.seq)));
        }
    }
    if rebuilt.len() < log.len() {
        return Err(mismatch(None));
    }
    let coverage = if rebuilt.len() == log.len() {
        Coverage::Verified
    } else {
        Coverage::Prefix
    };
    Ok((state, coverage))
}

fn inspect_engine(state: &EngineState, log: &EventLog) -> EngineInspection {
    let graph = state.graph();
    let mut firings: BTreeMap<FiringId, (NodeId, u32)> = BTreeMap::new();
    for record in state.history() {
        firings.insert(record.firing, (record.node, record.generation.raw()));
    }
    for firing in state.live_firings() {
        firings
            .entry(firing.id)
            .or_insert((firing.node, firing.generation.raw()));
    }
    let final_attempts: BTreeSet<(FiringId, Attempt)> = state
        .history()
        .iter()
        .map(|record| (record.firing, record.attempt))
        .collect();
    let node_of = |firing: FiringId| {
        firings
            .get(&firing)
            .and_then(|(node, _)| node_name(graph, *node))
    };

    let mut attempts = Vec::new();
    let mut routes = Vec::new();
    let mut deliveries = Vec::new();
    for record in log.records() {
        match &record.event {
            Event::ControlRequested { firing, ctl } => {
                let (kind, payload) = match ctl {
                    Control::Deliver(value) => ("deliver", Some(value.clone())),
                    Control::Cancel => ("cancel", None),
                    Control::Kill => ("kill", None),
                    _ => ("other", None),
                };
                deliveries.push(DeliveryInspection {
                    seq: record.seq,
                    firing: firing.raw(),
                    node: node_of(*firing),
                    kind,
                    payload,
                });
            }
            Event::StepFinished {
                firing,
                attempt,
                outcome,
            } => attempts.push(AttemptInspection {
                seq:        record.seq,
                firing:     firing.raw(),
                node:       node_of(*firing),
                generation: firings.get(firing).map(|(_, generation)| *generation),
                attempt:    attempt.raw(),
                status:     outcome.status.tag(),
                failure:    outcome.status.failure_info().cloned(),
                is_final:   final_attempts.contains(&(*firing, *attempt)),
            }),
            Event::RouteApplied { applied } => {
                let firing = applied.firing();
                let node = node_of(firing);
                routes.push(match applied {
                    RouteApplied::Edge { edge, group, .. } => RouteInspection {
                        seq: record.seq,
                        firing: firing.raw(),
                        node,
                        kind: "edge",
                        group: *group,
                        edge: Some(edge.raw()),
                        target: graph.edge(*edge).and_then(|edge| node_name(graph, edge.to)),
                    },
                    RouteApplied::None { group, .. } => RouteInspection {
                        seq: record.seq,
                        firing: firing.raw(),
                        node,
                        kind: "none",
                        group: *group,
                        edge: None,
                        target: None,
                    },
                });
            }
            _ => {}
        }
    }

    let run = state.run_context();
    EngineInspection {
        started: state.is_started(),
        finished: state.is_finished(),
        cancelled: state.is_cancelled(),
        exit: state.exit().map(|exit| exit_inspection(exit, graph)),
        folded_status: state.folded_status().to_string(),
        errors: state.errors().iter().map(ToString::to_string).collect(),
        context: ContextInspection {
            kv:    (*run.kv).clone(),
            nodes: run
                .nodes
                .iter()
                .map(|(name, record)| {
                    (name.clone(), NodeInspection {
                        status:       record.status.tag(),
                        success_like: record.status.is_success_like(),
                        failure:      record.status.failure_info().cloned(),
                        output:       record.output.clone(),
                        generation:   record.generation.raw(),
                        attempts:     record.attempts,
                    })
                })
                .collect(),
        },
        history: state
            .history()
            .iter()
            .map(|record| HistoryInspection {
                firing:          record.firing.raw(),
                node:            record.name.clone(),
                node_id:         record.node.raw(),
                generation:      record.generation.raw(),
                attempt:         record.attempt.raw(),
                status:          record.outcome.status.tag(),
                failure:         record.outcome.status.failure_info().cloned(),
                output:          record.outcome.output.clone(),
                context_updates: record.outcome.context_updates.clone(),
            })
            .collect(),
        attempts,
        routes,
        deliveries,
        live: state
            .live_firings()
            .map(|firing| LiveInspection {
                firing:         firing.id.raw(),
                node:           node_name(graph, firing.node),
                generation:     firing.generation.raw(),
                attempt:        firing.attempt.raw(),
                started:        firing.started,
                awaiting_retry: firing.awaiting_retry,
                cancelling:     firing.cancelling,
            })
            .collect(),
    }
}

fn exit_inspection(exit: &EngineExit, graph: &Graph) -> ExitInspection {
    match exit {
        EngineExit::Terminal { status } => ExitInspection::Terminal {
            status: status.to_string(),
        },
        EngineExit::Restart {
            edge,
            target,
            source,
        } => ExitInspection::Restart {
            edge:          edge.raw(),
            target:        target.raw(),
            target_name:   node_name(graph, *target),
            source_firing: source.raw(),
        },
    }
}

fn node_name(graph: &Graph, node: NodeId) -> Option<SmolStr> {
    graph.node(node).map(|node| node.name.clone())
}
