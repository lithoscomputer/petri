use std::collections::{BTreeMap, BTreeSet};

use engine::{EngineExit, EngineStart, MiddlewareKey};
use ir::{RunStatus, Value};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::host::ForkOrigin;
use crate::{
    AttemptAdmission, CancelReason, CoordinatorEvent, CoordinatorRecord, ExecutionId, GraphDigest,
    InvocationId, InvocationResult, ParentCallKey, SandboxBinding, SandboxLeaseId, SecretBindings,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvocationDeclaration {
    pub id:              InvocationId,
    pub call:            Option<ParentCallKey>,
    pub graph:           GraphDigest,
    pub context:         BTreeMap<SmolStr, Value>,
    pub secret_bindings: SecretBindings,
    pub sandbox:         SandboxBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission:       Option<AttemptAdmission>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvocationState {
    pub declaration:   InvocationDeclaration,
    pub executions:    Vec<ExecutionId>,
    pub result:        Option<InvocationResult>,
    pub cancelled:     bool,
    /// Why the cancel was requested, when the requester said.
    pub cancel_reason: Option<CancelReason>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutionDeclaration {
    pub id:               ExecutionId,
    pub invocation:       InvocationId,
    pub start:            EngineStart,
    pub middleware_state: BTreeMap<MiddlewareKey, (u32, Value)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutionState {
    pub declaration: ExecutionDeclaration,
    pub exit:        Option<EngineExit>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("the coordinator log must start with RunStarted")]
    MissingRunStart,
    #[error("RunStarted appears more than once")]
    DuplicateRunStart,
    #[error("coordinator format {found} is unsupported; expected {expected}")]
    UnsupportedFormat { found: u32, expected: u32 },
    #[error("graph {0} is registered more than once")]
    DuplicateGraph(GraphDigest),
    #[error("invocation {0} is declared more than once")]
    DuplicateInvocation(InvocationId),
    #[error("execution {0} is declared more than once")]
    DuplicateExecution(ExecutionId),
    #[error("unknown graph {0}")]
    UnknownGraph(GraphDigest),
    #[error("unknown invocation {0}")]
    UnknownInvocation(InvocationId),
    #[error("unknown execution {0}")]
    UnknownExecution(ExecutionId),
    #[error("call key is already bound to invocation {0}")]
    DuplicateCall(InvocationId),
    #[error("the root invocation must be invocation 0 with no parent call")]
    InvalidRootInvocation,
    #[error("execution {execution} has an invalid predecessor chain")]
    InvalidPredecessor { execution: ExecutionId },
    #[error("execution {0} is already finished")]
    DuplicateExecutionFinish(ExecutionId),
    #[error("invocation {0} is already finished")]
    DuplicateInvocationFinish(InvocationId),
    #[error("invocation {invocation} cannot finish from execution {execution}")]
    InvalidFinalExecution {
        invocation: InvocationId,
        execution:  ExecutionId,
    },
    #[error("the run is already finished")]
    DuplicateRunFinish,
    #[error("the run cannot finish before its root invocation")]
    RootNotFinished,
    #[error("RunFinished status differs from the root invocation result")]
    RunStatusMismatch,
    #[error("coordinator record at index {index} has sequence {found}")]
    Sequence { index: usize, found: u64 },
    #[error("the configured middleware chain differs from the recorded chain")]
    MiddlewareChain,
}

/// The replayed invocation tree.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorState {
    pub root:             Option<InvocationId>,
    pub middleware_chain: Vec<MiddlewareKey>,
    /// Where the run was forked from, when it was seeded from another run's
    /// records (`FORK.md`). Additive in format version 7.
    #[serde(default)]
    pub forked_from:      Option<ForkOrigin>,
    pub graphs:           BTreeSet<GraphDigest>,
    pub invocations:      BTreeMap<InvocationId, InvocationState>,
    pub executions:       BTreeMap<ExecutionId, ExecutionState>,
    pub calls:            BTreeMap<ParentCallKey, InvocationId>,
    pub run_status:       Option<RunStatus>,
    /// Whether the last recorded run control was a pause. A resume starts
    /// with admission held when it is.
    #[serde(default)]
    pub paused:           bool,
    /// Every run-level note, in record order: the reports of hook points
    /// that belong to no firing.
    #[serde(default)]
    pub run_notes:        Vec<RunNote>,
    /// The leases whose release is recorded (`scope.released`).
    #[serde(default)]
    pub released:         BTreeSet<SandboxLeaseId>,
}

/// A run-level note as recorded: a hook report from a point with no firing
/// (`run_finished`, `scope_released`), and the execution whose driver ran it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunNote {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionId>,
    pub kind:      SmolStr,
    #[serde(default)]
    pub payload:   Value,
}

impl CoordinatorState {
    pub fn replay(records: &[CoordinatorRecord]) -> Result<Self, StateError> {
        let mut state = Self::default();
        for (index, record) in records.iter().enumerate() {
            if record.seq != index as u64 {
                return Err(StateError::Sequence {
                    index,
                    found: record.seq,
                });
            }
            state.apply(&record.body)?;
        }
        if state.root.is_none() {
            return Err(StateError::MissingRunStart);
        }
        Ok(state)
    }

    /// The next free invocation id, derived from the declared tree. Ids only
    /// ever come from here, so the map's high key is the allocation state.
    /// Zero is the root's reserved id.
    pub fn next_invocation_id(&self) -> InvocationId {
        let next = self
            .invocations
            .keys()
            .next_back()
            .map_or(0, |id| {
                id.raw()
                    .checked_add(1)
                    .expect("one run cannot declare 2^64 invocations")
            })
            .max(1);
        InvocationId::new(next)
    }

    /// The next free execution id, on the same rule.
    pub fn next_execution_id(&self) -> ExecutionId {
        ExecutionId::new(self.executions.keys().next_back().map_or(0, |id| {
            id.raw()
                .checked_add(1)
                .expect("one run cannot declare 2^64 executions")
        }))
    }

    /// The execution declared after this one in its invocation, if any.
    pub fn successor_of(&self, execution: ExecutionId) -> Option<ExecutionId> {
        let invocation =
            &self.invocations[&self.executions.get(&execution)?.declaration.invocation];
        let index = invocation
            .executions
            .iter()
            .position(|candidate| *candidate == execution)?;
        invocation.executions.get(index + 1).copied()
    }

    /// Validate `event` against this state without changing it: every
    /// rejection `apply` can make, and nothing else. A store checks an event
    /// here before it writes the record, then applies it in place once the
    /// record is durable, so no append copies the whole state.
    pub fn check(&self, event: &CoordinatorEvent) -> Result<(), StateError> {
        if self.root.is_none() && !matches!(event, CoordinatorEvent::RunStarted { .. }) {
            return Err(StateError::MissingRunStart);
        }
        if self.run_status.is_some() && !matches!(event, CoordinatorEvent::RunStarted { .. }) {
            return Err(StateError::DuplicateRunFinish);
        }
        match event {
            CoordinatorEvent::RunStarted {
                format_version,
                key: _,
                root,
                middleware_chain: _,
                forked_from: _,
            } => {
                if self.root.is_some() {
                    return Err(StateError::DuplicateRunStart);
                }
                if *format_version != crate::COORDINATOR_FORMAT_VERSION {
                    return Err(StateError::UnsupportedFormat {
                        found:    *format_version,
                        expected: crate::COORDINATOR_FORMAT_VERSION,
                    });
                }
                if *root != InvocationId::ROOT {
                    return Err(StateError::InvalidRootInvocation);
                }
            }
            CoordinatorEvent::GraphRegistered { digest } => {
                if self.graphs.contains(digest) {
                    return Err(StateError::DuplicateGraph(*digest));
                }
            }
            CoordinatorEvent::InvocationDeclared {
                invocation,
                call,
                graph,
                ..
            } => {
                if !self.graphs.contains(graph) {
                    return Err(StateError::UnknownGraph(*graph));
                }
                if self.invocations.contains_key(invocation) {
                    return Err(StateError::DuplicateInvocation(*invocation));
                }
                if (*invocation == InvocationId::ROOT) != call.is_none() {
                    return Err(StateError::InvalidRootInvocation);
                }
                if let Some(call) = call {
                    if !self.executions.contains_key(&call.parent) {
                        return Err(StateError::UnknownExecution(call.parent));
                    }
                    if let Some(existing) = self.calls.get(call) {
                        return Err(StateError::DuplicateCall(*existing));
                    }
                }
            }
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                predecessor,
                start,
                middleware_state: _,
            } => {
                if self.executions.contains_key(execution) {
                    return Err(StateError::DuplicateExecution(*execution));
                }
                let invocation_state = self
                    .invocations
                    .get(invocation)
                    .ok_or(StateError::UnknownInvocation(*invocation))?;
                let expected_predecessor = invocation_state.executions.last().copied();
                if *predecessor != expected_predecessor
                    || usize::try_from(start.execution_index).ok()
                        != Some(invocation_state.executions.len())
                {
                    return Err(StateError::InvalidPredecessor {
                        execution: *execution,
                    });
                }
                if let Some(predecessor) = predecessor {
                    // The predecessor is the invocation's last execution — the
                    // check above pinned that — so it can have no successor
                    // yet; only its exit needs to justify one.
                    let prior = self
                        .executions
                        .get(predecessor)
                        .ok_or(StateError::UnknownExecution(*predecessor))?;
                    if !matches!(prior.exit, Some(EngineExit::Restart { .. })) {
                        return Err(StateError::InvalidPredecessor {
                            execution: *execution,
                        });
                    }
                }
            }
            CoordinatorEvent::ExecutionFinished { execution, exit: _ } => {
                let state = self
                    .executions
                    .get(execution)
                    .ok_or(StateError::UnknownExecution(*execution))?;
                if state.exit.is_some() {
                    return Err(StateError::DuplicateExecutionFinish(*execution));
                }
            }
            CoordinatorEvent::InvocationFinished { invocation, result } => {
                let state = self
                    .invocations
                    .get(invocation)
                    .ok_or(StateError::UnknownInvocation(*invocation))?;
                if state.result.is_some() {
                    return Err(StateError::DuplicateInvocationFinish(*invocation));
                }
                let final_execution = self
                    .executions
                    .get(&result.final_execution)
                    .ok_or(StateError::UnknownExecution(result.final_execution))?;
                // The final execution must be the invocation's last one — a
                // non-last execution already has a successor.
                if final_execution.declaration.invocation != *invocation
                    || state.executions.last() != Some(&result.final_execution)
                    || !matches!(final_execution.exit, Some(EngineExit::Terminal { .. }))
                {
                    return Err(StateError::InvalidFinalExecution {
                        invocation: *invocation,
                        execution:  result.final_execution,
                    });
                }
            }
            CoordinatorEvent::InvocationCancelRequested {
                invocation,
                reason: _,
            } => {
                if !self.invocations.contains_key(invocation) {
                    return Err(StateError::UnknownInvocation(*invocation));
                }
            }
            // A repeated pause or unpause is accepted and changes nothing:
            // the coordinator skips the redundant record, and a log that
            // carries one still replays. A run-level note and a released
            // scope constrain nothing: the resource log is the authority on
            // what the run holds.
            CoordinatorEvent::RunPaused
            | CoordinatorEvent::RunUnpaused
            | CoordinatorEvent::RunNoteRecorded { .. }
            | CoordinatorEvent::ScopeReleased { .. } => {}
            CoordinatorEvent::RunFinished { status } => {
                if self.run_status.is_some() {
                    return Err(StateError::DuplicateRunFinish);
                }
                let root = self.root.ok_or(StateError::MissingRunStart)?;
                let result = self
                    .invocations
                    .get(&root)
                    .and_then(|invocation| invocation.result.as_ref())
                    .ok_or(StateError::RootNotFinished)?;
                if result.status != *status {
                    return Err(StateError::RunStatusMismatch);
                }
            }
        }
        Ok(())
    }

    /// Validate `event` and, when it is accepted, apply it. A rejected event
    /// leaves the state unchanged.
    pub fn apply(&mut self, event: &CoordinatorEvent) -> Result<(), StateError> {
        self.check(event)?;
        self.apply_checked(event);
        Ok(())
    }

    /// Apply an event [`check`](Self::check) has accepted against this same
    /// state. Only the mutations; the lookups `check` made are invariants
    /// here, so calling this with an unchecked event is a bug.
    pub(crate) fn apply_checked(&mut self, event: &CoordinatorEvent) {
        match event {
            CoordinatorEvent::RunStarted {
                format_version: _,
                key: _,
                root,
                middleware_chain,
                forked_from,
            } => {
                self.root = Some(*root);
                self.middleware_chain.clone_from(middleware_chain);
                self.forked_from.clone_from(forked_from);
            }
            CoordinatorEvent::GraphRegistered { digest } => {
                self.graphs.insert(*digest);
            }
            CoordinatorEvent::InvocationDeclared {
                invocation,
                call,
                graph,
                context,
                secret_bindings,
                sandbox,
                admission,
            } => {
                if let Some(call) = call {
                    self.calls.insert(call.clone(), *invocation);
                }
                self.invocations.insert(*invocation, InvocationState {
                    declaration:   InvocationDeclaration {
                        id:              *invocation,
                        call:            call.clone(),
                        graph:           *graph,
                        context:         context.clone(),
                        secret_bindings: secret_bindings.clone(),
                        sandbox:         *sandbox,
                        admission:       admission.clone(),
                    },
                    executions:    Vec::new(),
                    result:        None,
                    cancelled:     false,
                    cancel_reason: None,
                });
            }
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                predecessor: _,
                start,
                middleware_state,
            } => {
                self.executions.insert(*execution, ExecutionState {
                    declaration: ExecutionDeclaration {
                        id:               *execution,
                        invocation:       *invocation,
                        start:            start.clone(),
                        middleware_state: middleware_state.clone(),
                    },
                    exit:        None,
                });
                self.invocations
                    .get_mut(invocation)
                    .expect("`check` found the invocation")
                    .executions
                    .push(*execution);
            }
            CoordinatorEvent::ExecutionFinished { execution, exit } => {
                self.executions
                    .get_mut(execution)
                    .expect("`check` found the execution")
                    .exit = Some(exit.clone());
            }
            CoordinatorEvent::InvocationFinished { invocation, result } => {
                self.invocations
                    .get_mut(invocation)
                    .expect("`check` found the invocation")
                    .result = Some(result.clone());
            }
            CoordinatorEvent::InvocationCancelRequested { invocation, reason } => {
                let state = self
                    .invocations
                    .get_mut(invocation)
                    .expect("`check` found the invocation");
                state.cancelled = true;
                if state.cancel_reason.is_none() {
                    state.cancel_reason.clone_from(reason);
                }
            }
            CoordinatorEvent::RunPaused => self.paused = true,
            CoordinatorEvent::RunUnpaused => self.paused = false,
            CoordinatorEvent::RunNoteRecorded {
                execution,
                kind,
                payload,
            } => self.run_notes.push(RunNote {
                execution: *execution,
                kind:      kind.clone(),
                payload:   payload.clone(),
            }),
            CoordinatorEvent::ScopeReleased { lease, .. } => {
                self.released.insert(*lease);
            }
            CoordinatorEvent::RunFinished { status } => {
                self.run_status = Some(*status);
            }
        }
    }
}
