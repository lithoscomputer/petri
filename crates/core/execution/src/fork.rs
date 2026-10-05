//! Fork: seed a new run from a stored run's records up to a position.
//!
//! The design is in `FORK.md`. In short: the position's execution keeps a
//! prefix of its engine log, cut after the position firing's routing (or,
//! with `rerun_last`, before the firing's first record); every other kept
//! execution's log is copied whole; the fork's coordinator log is written
//! fresh from the source's records for the kept part of the invocation tree,
//! under a run declaration that names the source and the position; graphs
//! are copied by digest; nothing of the source's sandbox leases is carried
//! over. The host continues the new run with `host::resume_configured`.

use std::collections::BTreeSet;
use std::fmt;

use engine::{DecisionId, Event, EventLog, EventOrigin};
use ir::FiringId;
use runtime::Runtime;
use serde::{Deserialize, Serialize};
use store::{Access, LogId, OwnerId, Record, RunKey, RunLogs};

use crate::host::HostError;
use crate::store::{decode_graph, encode_record, graph_bytes, read_coordinator_log};
use crate::{
    COORDINATOR_FORMAT_VERSION, CoordinatorError, CoordinatorEvent, CoordinatorRecord,
    CoordinatorState, EngineLogError, ExecutionId, GraphDigest, InvocationId, StateError,
    StoreError, decode_engine_records,
};

/// A position in a run: a firing of one of the root invocation's executions,
/// named by the run's own ids. As a fork position it is the last firing
/// whose finish and routing the fork keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkPosition {
    pub execution: ExecutionId,
    pub firing:    FiringId,
}

impl fmt::Display for ForkPosition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "execution {} firing {}",
            self.execution, self.firing
        )
    }
}

/// How a fork treats the position's firing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForkOptions {
    /// Run the position's firing again instead of keeping its finish: the
    /// engine log is cut before the firing's first record, so the fork's
    /// resume admits it and runs it from its first attempt. What Fabro's
    /// retry asks for.
    pub rerun_last: bool,
}

/// Where a forked run came from, as its run declaration records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkOrigin {
    /// The source run's key.
    pub source:     RunKey,
    /// The position the source's records were kept up to.
    #[serde(flatten)]
    pub position:   ForkPosition,
    /// Whether the position's firing was left to run again.
    pub rerun_last: bool,
}

/// A seeded fork: the new run in the runtime's store, not yet resumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkedRun {
    /// The new run's key: `RunOptions::run_key` when the host gave one,
    /// else the one Petri minted.
    pub key:    RunKey,
    /// The source and position, as recorded on the new run's declaration.
    pub origin: ForkOrigin,
    /// The root graph's digest, the same as the source's.
    pub graph:  GraphDigest,
}

/// Why a position cannot be forked.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ForkError {
    #[error("the source run has no execution {0}")]
    UnknownExecution(ExecutionId),
    #[error(
        "execution {execution} belongs to invocation {invocation}, not the root: a position \
         inside a child invocation cannot be forked"
    )]
    PositionInChild {
        execution:  ExecutionId,
        invocation: InvocationId,
    },
    #[error(
        "execution {execution} records no routing for firing {firing}: a fork position is a \
         firing whose finish was routed"
    )]
    UnroutedFiring {
        execution: ExecutionId,
        firing:    FiringId,
    },
}

/// Seed a new run in the runtime's store from `source`'s records up to
/// `position`. The new run lives under the runtime's `run_dir` with the key
/// `RunOptions::run_key` names (else a fresh one) and must not exist yet.
/// It is left unfinished at the position, with no sandbox lease, so a
/// [`crate::host::resume_configured`] over it acquires the position
/// execution's scopes fresh and continues from there. See `FORK.md`.
pub async fn fork_from(
    rt: &Runtime,
    source: &dyn RunLogs,
    position: ForkPosition,
    options: ForkOptions,
) -> Result<ForkedRun, HostError> {
    let plan = plan_fork(source, position, options).await?;
    let run_dir = rt.run_options().run_dir.clone();
    let key = rt.run_key_for(&run_dir);
    let logs = rt
        .store_for(&run_dir)
        .open(&key, Access::Create {
            owner: OwnerId::mint(),
        })
        .await?;
    let required_finalization = rt
        .installed_hooks()
        .is_some_and(|hooks| hooks.requires_run_finalization());
    write_fork(&*logs, source, &plan, key, required_finalization).await
}

/// Everything the fork copies, decided before the new run is created.
struct ForkPlan {
    origin:           ForkOrigin,
    middleware_chain: Vec<engine::MiddlewareKey>,
    root_graph:       GraphDigest,
    /// The source's coordinator records the fork keeps, in order, without
    /// the source's own declaration.
    records:          Vec<CoordinatorRecord>,
    /// Every kept execution, in id order.
    executions:       BTreeSet<ExecutionId>,
    /// The position execution's stored records up to the cut.
    prefix:           Vec<Record>,
}

async fn plan_fork(
    source: &dyn RunLogs,
    position: ForkPosition,
    options: ForkOptions,
) -> Result<ForkPlan, HostError> {
    let records = read_coordinator_log(source)
        .await
        .map_err(CoordinatorError::from)?;
    let state = CoordinatorState::replay(&records).map_err(state_error)?;
    let source_key = match records.first().map(|record| &record.body) {
        Some(CoordinatorEvent::RunStarted { key, .. }) => key.clone(),
        _ => return Err(state_error(StateError::MissingRunStart)),
    };
    let execution = state
        .executions
        .get(&position.execution)
        .ok_or(ForkError::UnknownExecution(position.execution))?;
    if execution.declaration.invocation != InvocationId::ROOT {
        return Err(ForkError::PositionInChild {
            execution:  position.execution,
            invocation: execution.declaration.invocation,
        }
        .into());
    }
    let root = state
        .invocations
        .get(&InvocationId::ROOT)
        .ok_or_else(|| state_error(StateError::UnknownInvocation(InvocationId::ROOT)))?;
    let root_graph = root.declaration.graph;
    let graph = decode_graph(
        root_graph,
        &graph_bytes(source, root_graph)
            .await
            .map_err(CoordinatorError::from)?,
    )
    .map_err(CoordinatorError::from)?;

    // The position execution's log, cut at the position.
    let stored = source.read(&LogId::Execution(position.execution)).await?;
    let decoded = decode_engine_records(&stored).map_err(|error| {
        CoordinatorError::from(EngineLogError::Decode {
            execution: position.execution,
            source:    error,
        })
    })?;
    let cut = cut_index(&decoded.log, position, options)?;
    let prefix_log = decoded.log.prefix(cut);
    // The cut lands on an apply boundary, so the prefix replays exactly.
    let replayed = engine::verify_replay(graph, &prefix_log)?;
    let finished: BTreeSet<FiringId> = replayed
        .history()
        .iter()
        .map(|record| record.firing)
        .collect();
    let mut prefix = stored;
    prefix.truncate(cut);

    // The kept tree: the root up to the position's execution, and every
    // finished child called from a kept execution by a kept finish. A
    // child's id is above its parent invocation's, so one pass in id order
    // sees each parent before its children.
    let chain_end = root
        .executions
        .iter()
        .position(|candidate| *candidate == position.execution)
        .ok_or_else(|| state_error(StateError::UnknownExecution(position.execution)))?;
    let mut kept_invocations = BTreeSet::from([InvocationId::ROOT]);
    let mut executions: BTreeSet<ExecutionId> =
        root.executions[..=chain_end].iter().copied().collect();
    for (id, invocation) in &state.invocations {
        let Some(call) = &invocation.declaration.call else {
            continue;
        };
        let Some(parent) = state.executions.get(&call.parent) else {
            continue;
        };
        let kept = invocation.result.is_some()
            && kept_invocations.contains(&parent.declaration.invocation)
            && executions.contains(&call.parent)
            && (call.parent != position.execution || finished.contains(&call.firing));
        if kept {
            kept_invocations.insert(*id);
            executions.extend(invocation.executions.iter().copied());
        }
    }

    let records = records
        .into_iter()
        .filter(|record| {
            keeps(
                &record.body,
                &kept_invocations,
                &executions,
                position.execution,
            )
        })
        .collect();
    Ok(ForkPlan {
        origin: ForkOrigin {
            source: source_key,
            position,
            rerun_last: options.rerun_last,
        },
        middleware_chain: state.middleware_chain,
        root_graph,
        records,
        executions,
        prefix,
    })
}

/// Whether a source coordinator record belongs in the fork's log.
fn keeps(
    event: &CoordinatorEvent,
    invocations: &BTreeSet<InvocationId>,
    executions: &BTreeSet<ExecutionId>,
    position: ExecutionId,
) -> bool {
    match event {
        CoordinatorEvent::GraphRegistered { .. } => true,
        CoordinatorEvent::InvocationDeclared { invocation, .. } => invocations.contains(invocation),
        CoordinatorEvent::ExecutionDeclared { execution, .. } => executions.contains(execution),
        // The position's execution is the one the fork continues.
        CoordinatorEvent::ExecutionFinished { execution, .. } => {
            executions.contains(execution) && *execution != position
        }
        CoordinatorEvent::InvocationFinished { invocation, .. }
        | CoordinatorEvent::InvocationCancelRequested { invocation, .. } => {
            invocations.contains(invocation) && *invocation != InvocationId::ROOT
        }
        // The fork writes its own declaration; the source's controls,
        // run-level notes, sandbox releases and end describe the source.
        CoordinatorEvent::RunStarted { .. }
        | CoordinatorEvent::RunPaused
        | CoordinatorEvent::RunUnpaused
        | CoordinatorEvent::RunNoteRecorded { .. }
        | CoordinatorEvent::ScopeReleased { .. }
        | CoordinatorEvent::RunFinished { .. } => false,
    }
}

/// Where the position execution's log is cut: after the core records the
/// position firing's routing produced, or before the firing's first record
/// when it runs again. Both are apply boundaries.
fn cut_index(
    log: &EventLog,
    position: ForkPosition,
    options: ForkOptions,
) -> Result<usize, ForkError> {
    let records = log.records();
    let routed = records
        .iter()
        .rposition(|record| {
            record.origin == EventOrigin::External
                && matches!(
                    &record.event,
                    Event::RoutingResolved {
                        decision_id: DecisionId::Route { firing, .. },
                        ..
                    } if *firing == position.firing
                )
        })
        .ok_or(ForkError::UnroutedFiring {
            execution: position.execution,
            firing:    position.firing,
        })?;
    if options.rerun_last {
        return Ok(records
            .iter()
            .position(|record| names_firing(&record.event, position.firing))
            .expect("the routing record names the firing"));
    }
    let mut cut = routed + 1;
    while records
        .get(cut)
        .is_some_and(|record| record.origin == EventOrigin::Core)
    {
        cut += 1;
    }
    Ok(cut)
}

/// Whether an engine record is one of `firing`'s own. A token the firing
/// emitted is not: it follows the firing's routing, never precedes its first
/// record, and a seed token names firing 0.
fn names_firing(event: &Event, firing: FiringId) -> bool {
    match event {
        Event::StepStarted { firing: own, .. }
        | Event::StepProgressRecorded { firing: own, .. }
        | Event::StepFinished { firing: own, .. }
        | Event::RetryElapsed { firing: own, .. }
        | Event::ControlRequested { firing: own, .. } => *own == firing,
        Event::AdmissionDecided { decision_id, .. }
        | Event::RoutingResolved { decision_id, .. } => match decision_id {
            DecisionId::AttemptStart { firing: own, .. }
            | DecisionId::Route { firing: own, .. } => *own == firing,
            DecisionId::ExecutionStart => false,
        },
        Event::RouteApplied { applied } => applied.firing() == firing,
        Event::ExecutionStarted { .. }
        | Event::TokenEmitted { .. }
        | Event::NodeExpanded { .. }
        | Event::CancelRequested { .. }
        | Event::KillRequested { .. }
        | Event::ScopeAcquired { .. }
        | Event::ScopeFailed { .. } => false,
    }
}

/// Write the planned fork into the new run: the graphs first, so no
/// registration names a missing blob; then the coordinator log; then each
/// kept execution's engine log.
async fn write_fork(
    logs: &dyn RunLogs,
    source: &dyn RunLogs,
    plan: &ForkPlan,
    key: RunKey,
    required_finalization: bool,
) -> Result<ForkedRun, HostError> {
    let mut records = vec![CoordinatorRecord::external(
        0,
        driver::recorded_now(),
        CoordinatorEvent::RunStarted {
            format_version: COORDINATOR_FORMAT_VERSION,
            key: key.clone(),
            root: InvocationId::ROOT,
            middleware_chain: plan.middleware_chain.clone(),
            required_finalization,
            forked_from: Some(plan.origin.clone()),
        },
    )];
    records.extend(
        plan.records
            .iter()
            .zip(1_u64..)
            .map(|(record, seq)| CoordinatorRecord {
                seq,
                origin: record.origin,
                recorded_at: record.recorded_at,
                body: record.body.clone(),
            }),
    );
    // The kept records must make a run of their own: a fork that broke the
    // state machine would be a bug here, not a run that fails to resume.
    CoordinatorState::replay(&records).map_err(state_error)?;

    for record in &records {
        if let CoordinatorEvent::GraphRegistered { digest } = record.body {
            let bytes = graph_bytes(source, digest)
                .await
                .map_err(CoordinatorError::from)?;
            let stored = logs.put_blob(&bytes).await?;
            if stored != digest {
                return Err(CoordinatorError::from(StoreError::GraphDigest {
                    expected: digest,
                    found:    stored,
                })
                .into());
            }
        }
    }
    let encoded = records
        .iter()
        .map(encode_record)
        .collect::<Result<Vec<_>, _>>()
        .map_err(CoordinatorError::from)?;
    logs.append(&LogId::Coordinator, &encoded).await?;

    for execution in &plan.executions {
        let stored = if *execution == plan.origin.position.execution {
            plan.prefix.clone()
        } else {
            source.read(&LogId::Execution(*execution)).await?
        };
        if !stored.is_empty() {
            logs.append(&LogId::Execution(*execution), &stored).await?;
        }
    }
    Ok(ForkedRun {
        key,
        origin: plan.origin.clone(),
        graph: plan.root_graph,
    })
}

fn state_error(error: StateError) -> HostError {
    CoordinatorError::from(StoreError::State(error)).into()
}
