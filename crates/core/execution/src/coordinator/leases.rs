//! Sandbox leases: where an execution's scopes run, how a finished
//! invocation's leases are released and recorded, and what a resume checks.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use driver::{SandboxAssignment, ScopeLease, ScopeLeaseAllocator, ScopeLeases};
use executor_sandbox::{CONTAINER_KIND, RecordedLease, RoutingExecutor, SIMULATED_KIND};
use ir::{RunStatus, RuntimeTarget, ScopeId};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinSet;

use super::start::invoke_error;
use super::{Coordinator, CoordinatorError};
use crate::{
    CoordinatorEvent, CoordinatorStore, ExecutionId, ExecutionLogWriter, GraphDigest,
    HOST_PROVIDER, InvocationId, InvokeError, ParentCallKey, ResourceError, ResourceStore,
    SandboxAllocationKey, SandboxBinding, read_execution_log,
};

/// One lease's release: what the run asked for and what the executor did.
pub(super) struct LeaseRelease {
    lease:   crate::SandboxLeaseId,
    outcome: executor::ScopeOutcome,
    report:  executor::ReleaseReport,
}

/// A finished invocation's leases, released.
pub(super) struct InvocationReleased {
    pub(super) invocation: InvocationId,
    pub(super) releases:   Vec<LeaseRelease>,
}

/// The scope outcome an invocation's status maps to, which decides the
/// retention of its sandboxes.
fn scope_outcome(status: RunStatus) -> executor::ScopeOutcome {
    if status == RunStatus::Success {
        executor::ScopeOutcome::Succeeded
    } else {
        executor::ScopeOutcome::Failed
    }
}

fn log_release_problems(lease: crate::SandboxLeaseId, report: &executor::ReleaseReport) {
    for problem in &report.problems {
        tracing::warn!(
            lease = lease.raw(),
            problem,
            "sandbox lease release problem"
        );
    }
}

pub(super) type ExecutionLeases = Arc<Mutex<BTreeMap<ScopeId, crate::SandboxLeaseId>>>;

/// The coordinator delegates reservation to each execution's acquire tasks.
/// The shared resource store still serializes lease ID allocation and writes.
struct InvocationLeaseAllocator {
    invocation: InvocationId,
    execution:  ExecutionId,
    resources:  Arc<AsyncMutex<ResourceStore>>,
    acquired:   ExecutionLeases,
    router:     Option<Arc<RoutingExecutor>>,
    writer:     Arc<ExecutionLogWriter>,
}

impl fmt::Debug for InvocationLeaseAllocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InvocationLeaseAllocator")
            .field("invocation", &self.invocation)
            .field("execution", &self.execution)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl ScopeLeaseAllocator for InvocationLeaseAllocator {
    async fn reserve(
        &self,
        identity: engine::ScopeIdentity,
        spec: &executor::ScopeSpec,
    ) -> Result<ScopeLease, executor::EnvError> {
        let error = |error: &dyn fmt::Display| {
            executor::EnvError::backend("coordinator", "reserve lease", error.to_string())
        };
        // A write the store failed is the run's failure, not the scope's.
        let store_failed = |error: &dyn fmt::Display| executor::EnvError::Store {
            message: error.to_string(),
        };
        let introduced_by =
            matches!(identity, engine::ScopeIdentity::Spliced(_)).then_some(self.execution);
        // The scope's introducing outcome must be durable before its lease.
        // This queues a marker behind all records already observed by the
        // driver.
        if introduced_by.is_some() {
            self.writer
                .flush()
                .await
                .map_err(|source| store_failed(&source))?;
        }
        let provider = self
            .router
            .as_ref()
            .map_or_else(
                || match spec.runtime.target {
                    RuntimeTarget::HostProcess => HOST_PROVIDER,
                    RuntimeTarget::Container { .. } => CONTAINER_KIND,
                },
                |router| router.provider_kind_for(&spec.runtime),
            )
            .to_owned();
        let allocation = SandboxAllocationKey {
            invocation: self.invocation,
            scope:      identity,
        };
        let runtime = spec.runtime.clone();
        let assignment = {
            let mut resources = self.resources.lock().await;
            let record = resources
                .reserve_scope(allocation, &provider, runtime, introduced_by)
                .await
                .map_err(|source| match source {
                    ResourceError::Store(_) => store_failed(&source),
                    other => error(&other),
                })?;
            if record.state == crate::LeaseState::Deleted {
                return Err(error(&ResourceError::DeletedLease(record.lease)));
            }
            ScopeLease {
                lease:     record.lease,
                workspace: record.workspace.clone(),
                provider:  record.provider.clone(),
            }
        };
        self.acquired
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(spec.id, assignment.lease);
        Ok(assignment)
    }
}

impl Coordinator {
    /// The takeover step of a resume: before any create, adopt what a lost
    /// create left on a provider and remove what no record names. Runs
    /// with the ledger attached, so an adopted lease is recorded live.
    /// Problems are logged, not returned: a lease it cannot settle keeps its
    /// record for the run's own release or a later prune.
    pub(super) async fn reconcile_leases(&self) {
        // A simulated lease (a dry run's) was never on a provider: there is
        // nothing to adopt and nothing to sweep.
        let (host, container): (Vec<_>, Vec<_>) = self
            .resources()
            .await
            .records()
            .filter(|record| {
                record.state != crate::LeaseState::Deleted && record.provider != SIMULATED_KIND
            })
            .map(|record| {
                (record.provider == HOST_PROVIDER, RecordedLease {
                    lease:        record.lease,
                    workspace_id: record.workspace.as_str().to_owned(),
                })
            })
            .partition::<Vec<_>, _>(|(host, _)| *host);
        let host: Vec<RecordedLease> = host.into_iter().map(|(_, lease)| lease).collect();
        let container: Vec<RecordedLease> = container.into_iter().map(|(_, lease)| lease).collect();
        if host.is_empty() && container.is_empty() {
            return;
        }
        let report = self.runtime.reconcile_leases(&host, &container).await;
        for (lease, sandbox) in &report.adopted {
            tracing::info!(lease = lease.raw(), sandbox = %sandbox, "adopted a lost create");
        }
        for sandbox in &report.removed {
            tracing::warn!(sandbox = %sandbox, "removed a sandbox no lease record names");
        }
        for problem in &report.problems {
            tracing::warn!(problem, "sandbox lease reconciliation problem");
        }
    }

    /// Stop a lease's sandbox, then keep or delete it by retention for the
    /// outcome `status` maps to. Release is best effort; a problem is
    /// logged, and the record keeps its pending intent for the next attempt
    /// (`finish`, or `petri sandbox prune`).
    pub(super) async fn release_lease(
        &self,
        lease: crate::SandboxLeaseId,
        status: RunStatus,
    ) -> LeaseRelease {
        let outcome = scope_outcome(status);
        let report = self.runtime.release_lease(lease, outcome).await;
        log_release_problems(lease, &report);
        LeaseRelease {
            lease,
            outcome,
            report,
        }
    }

    /// Record a lease's release as the run's `scope.released`: the lease's
    /// record after the release says whether its sandbox is still there.
    pub(super) async fn append_scope_released(
        &mut self,
        invocation: InvocationId,
        release: LeaseRelease,
    ) -> Result<(), CoordinatorError> {
        let LeaseRelease {
            lease,
            outcome,
            report,
        } = release;
        let record = {
            let resources = self.resources().await;
            let Ok(record) = resources.resolve(lease) else {
                return Ok(());
            };
            record.clone()
        };
        self.append(CoordinatorEvent::ScopeReleased {
            invocation,
            lease,
            scope: record.allocation.scope,
            workspace: record.workspace,
            provider: record.provider,
            instance: record.resource_id,
            outcome,
            retained: record.state != crate::LeaseState::Deleted,
            problems: report.problems,
        })
        .await?;
        Ok(())
    }

    /// Record the releases a crash cut off: a lease its release settled
    /// (stopped and kept, or deleted) whose `scope.released` the crash beat
    /// to the log. Its record says how the release ended; the outcome is
    /// its owner's, or `status` for an owner with none.
    pub(super) async fn record_cut_off_releases(&mut self, status: RunStatus) {
        let unrecorded: Vec<_> = self
            .resources()
            .await
            .records()
            .filter(|record| {
                matches!(
                    record.state,
                    crate::LeaseState::Stopped | crate::LeaseState::Deleted
                ) && !self.store.state().released.contains(&record.lease)
            })
            .map(|record| {
                let owner_status = self
                    .store
                    .state()
                    .invocations
                    .get(&record.allocation.invocation)
                    .and_then(|invocation| invocation.result.as_ref())
                    .map_or(status, |result| result.status);
                (record.lease, record.allocation.invocation, owner_status)
            })
            .collect();
        for (lease, invocation, owner_status) in unrecorded {
            let release = LeaseRelease {
                lease,
                outcome: scope_outcome(owner_status),
                report: executor::ReleaseReport::default(),
            };
            if let Err(error) = self.append_scope_released(invocation, release).await {
                tracing::warn!(%error, lease = lease.raw(), "the scope's release was not recorded");
            }
        }
    }

    /// Release the leases `invocation` allocated, now that it has finished.
    /// An inherited invocation allocated none: its caller's lease outlives
    /// it.
    pub(super) async fn release_invocation_leases(
        &mut self,
        invocation: InvocationId,
        status: RunStatus,
        releasing: &mut JoinSet<InvocationReleased>,
    ) {
        let owned: Vec<crate::SandboxLeaseId> = self
            .resources()
            .await
            .records()
            .filter(|record| record.allocation.invocation == invocation && record.needs_release())
            .map(|record| record.lease)
            .collect();
        let router = self.runtime.sandbox_router().cloned();
        #[cfg(test)]
        let gate = self.release_gate.take();
        releasing.spawn(async move {
            #[cfg(test)]
            if let Some(gate) = gate {
                gate.started.notify_one();
                gate.complete.notified().await;
            }
            let mut releases = Vec::new();
            if let Some(router) = router {
                let outcome = scope_outcome(status);
                for lease in owned {
                    let report = router.release_lease(lease, outcome).await;
                    log_release_problems(lease, &report);
                    releases.push(LeaseRelease {
                        lease,
                        outcome,
                        report,
                    });
                }
            }
            InvocationReleased {
                invocation,
                releases,
            }
        });
    }

    /// Where an execution's scopes run. An isolated invocation owns one
    /// lease per stable scope identity, reserved before any executor sees
    /// it; an inherited one runs every scope in its caller's sandbox — the
    /// caller's workspace, the caller's runtime target, one shared lease.
    /// The isolated invocation's acquired leases come back beside the
    /// assignment, for a child that inherits one of them.
    pub(super) async fn prepare_sandbox(
        &mut self,
        invocation: InvocationId,
        execution: ExecutionId,
        writer: Arc<ExecutionLogWriter>,
    ) -> Result<(SandboxAssignment, Option<ExecutionLeases>), CoordinatorError> {
        match self.store.state().invocations[&invocation]
            .declaration
            .sandbox
        {
            SandboxBinding::Inherited { lease } => {
                let (workspace, runtime) = {
                    let resources = self.resources().await;
                    let record = resources
                        .resolve_usable(lease)
                        .map_err(|error| match error {
                            ResourceError::DeletedLease(lease) => {
                                CoordinatorError::InvalidResource { lease }
                            }
                            other => other.into(),
                        })?;
                    (record.workspace.clone(), record.runtime.clone())
                };
                let assignment = SandboxAssignment {
                    workspace_override: Some(workspace),
                    runtime_override:   Some(runtime),
                    leases:             ScopeLeases::Shared(lease),
                };
                Ok((assignment, None))
            }
            SandboxBinding::Isolated => {
                let acquired: ExecutionLeases = Arc::new(Mutex::new(BTreeMap::new()));
                let allocator = InvocationLeaseAllocator {
                    invocation,
                    execution,
                    resources: self.resources.clone(),
                    acquired: acquired.clone(),
                    router: self.runtime.sandbox_router().cloned(),
                    writer,
                };
                let assignment = SandboxAssignment {
                    workspace_override: None,
                    runtime_override:   None,
                    leases:             ScopeLeases::Owned(Arc::new(allocator)),
                };
                Ok((assignment, Some(acquired)))
            }
        }
    }

    /// Resolve `SandboxMode::Inherit` for a new child. The call key pins the
    /// parent invocation, and the caller names its own scope, so no engine log
    /// is read. The driver registers every acquired scope, including dynamic
    /// ones, before a step can invoke a child.
    pub(super) async fn inherited_binding(
        &mut self,
        call: &ParentCallKey,
        scope: ir::ScopeId,
    ) -> Result<SandboxBinding, CoordinatorError> {
        let execution = self
            .store
            .state()
            .executions
            .get(&call.parent)
            .ok_or(CoordinatorError::NoInheritableSandbox)?;
        let parent_invocation = execution.declaration.invocation;
        if let SandboxBinding::Inherited { lease } = self.store.state().invocations
            [&parent_invocation]
            .declaration
            .sandbox
        {
            self.resources().await.resolve_usable(lease)?;
            return Ok(SandboxBinding::Inherited { lease });
        }
        let lease = self
            .live
            .leases_of(call.parent)
            .and_then(|leases| {
                leases
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&scope)
                    .copied()
            })
            .ok_or(CoordinatorError::NoInheritableSandbox)?;
        self.resources().await.resolve_usable(lease)?;
        Ok(SandboxBinding::Inherited { lease })
    }

    /// An inherited child runs in its caller's sandbox, so a container it
    /// declares must be the caller's own: absent (the scope takes the
    /// caller's target) or equal. A different image or option set is a
    /// deterministic refusal at declaration; a child that needs its own
    /// image uses an isolated binding.
    pub(super) async fn check_inherited_container(
        &mut self,
        lease: crate::SandboxLeaseId,
        child: GraphDigest,
    ) -> Result<(), InvokeError> {
        let parent = self
            .resources()
            .await
            .resolve(lease)
            .map(|record| record.runtime.clone())
            .map_err(invoke_error)?;
        let graph = self.store.load_graph(child).await.map_err(invoke_error)?;
        for scope in &graph.scopes {
            let declares_container =
                matches!(scope.runtime.target, RuntimeTarget::Container { .. });
            if declares_container && scope.runtime.target != parent.target {
                return Err(InvokeError::InheritedContainerMismatch { scope: scope.id });
            }
        }
        Ok(())
    }
}

pub(super) async fn validate_resources(
    store: &mut CoordinatorStore,
    resources: &ResourceStore,
) -> Result<(), CoordinatorError> {
    let mut replayed = BTreeMap::new();
    for record in resources.records() {
        let invalid = || CoordinatorError::InvalidResource {
            lease: record.lease,
        };
        let Some(invocation) = store.state().invocations.get(&record.allocation.invocation) else {
            return Err(CoordinatorError::InvalidResource {
                lease: record.lease,
            });
        };
        if invocation.declaration.sandbox != SandboxBinding::Isolated {
            return Err(CoordinatorError::InvalidResource {
                lease: record.lease,
            });
        }
        let digest = invocation.declaration.graph;
        let graph = store.load_graph(digest).await?;
        match &record.allocation.scope {
            engine::ScopeIdentity::Declared(scope) => {
                if record.introduced_by.is_some()
                    || graph
                        .scope(*scope)
                        .is_none_or(|scope| scope.runtime != record.runtime)
                {
                    return Err(invalid());
                }
            }
            identity @ engine::ScopeIdentity::Spliced(_) => {
                let execution = record.introduced_by.ok_or_else(invalid)?;
                if store
                    .state()
                    .executions
                    .get(&execution)
                    .is_none_or(|execution| {
                        execution.declaration.invocation != record.allocation.invocation
                    })
                {
                    return Err(invalid());
                }
                if let Entry::Vacant(entry) = replayed.entry(execution) {
                    let log = read_execution_log(&**store.logs(), execution).await?;
                    let point =
                        engine::resume((*graph).clone(), &log.log).map_err(|_| invalid())?;
                    entry.insert(point.state);
                }
                let state = &replayed[&execution];
                if !state.graph().scopes.iter().any(|scope| {
                    state.scope_identity(scope.id).as_ref() == Some(identity)
                        && scope.runtime == record.runtime
                }) {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}
