//! Durable sandbox leases: one append-only log of lease records, the
//! crash-safe authority on what the run holds on which provider.
//!
//! Every transition of a lease is one record in the run's resource log; the
//! latest record per lease is its current state, and the run's single
//! writer keeps that map in memory, rebuilt from the log on open. A record
//! is reserved before anything exists on a provider, in
//! [`LeaseState::Allocating`]; it becomes `live` with the provider's own
//! resource id only after the provider confirms the create (or recovery
//! finds the one match). Every stop and delete is written down as a
//! [`PendingIntent`] before the provider is asked and confirmed after, so a
//! crash between the two leaves a repeatable intent, never a lie. A deleted
//! lease stays as a tombstone while the run exists, so a historical
//! inherited invocation still resolves during replay while new work on the
//! lease is refused.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use driver::RecordingClock;
use executor::WorkspaceId;
use executor_sandbox::{LeaseLedger, LeaseRecord, LedgerError};
pub use executor_sandbox::{LeaseState, PendingIntent};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use store::{LogId, Record, RunLogs};
use tokio::sync::Mutex;

use crate::observe::StoreFailure;
use crate::{ExecutionId, SandboxAllocationKey, SandboxLeaseId};

/// The provider kind a host-process scope's lease records: its workspace
/// is a directory under the run dir, governed by the run's retention.
pub const HOST_PROVIDER: &str = "host";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SandboxResourceRecord {
    pub lease:         SandboxLeaseId,
    pub allocation:    SandboxAllocationKey,
    /// The provider kind: [`HOST_PROVIDER`], or the sandbox plugin kind the
    /// lease manager recorded at allocation.
    pub provider:      SmolStr,
    /// The provider's own id for the resource, once it exists.
    #[serde(default)]
    pub resource_id:   Option<SmolStr>,
    pub workspace:     WorkspaceId,
    /// The declaration must match on every acquisition, including restarts.
    pub runtime:       ir::RuntimeSpec,
    /// The execution whose durable log first introduced a dynamic scope.
    /// Provenance for recovery validation; never part of allocation identity.
    pub introduced_by: Option<ExecutionId>,
    #[serde(default)]
    pub state:         LeaseState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending:       Option<PendingIntent>,
    /// The non-secret fingerprint of the backend the resource lives on: the
    /// daemon or endpoint, and the account or target. Never credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint:   Option<SmolStr>,
}

impl SandboxResourceRecord {
    /// Whether this lease still needs release. Host leases can own action
    /// hosts. A stopped sandbox with no pending intent already had its
    /// retention applied by its owning invocation.
    pub fn needs_release(&self) -> bool {
        self.state != LeaseState::Deleted
            && (self.pending.is_some()
                || self.state == LeaseState::Live
                || (self.state == LeaseState::Allocating && self.fingerprint.is_some()))
    }
}

/// One line of the resource log: a lease's whole record at one transition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResourceLogRecord {
    pub seq:         u64,
    /// Milliseconds since the Unix epoch when the transition was recorded.
    pub recorded_at: u64,
    pub body:        SandboxResourceRecord,
}

#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error("the resource log holds a record at seq {seq} this build cannot decode: {source}")]
    Decode {
        seq:    u64,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not encode a sandbox resource record: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("sandbox allocation {0:?} is recorded under more than one lease")]
    DuplicateAllocation(SandboxAllocationKey),
    #[error("sandbox allocation {0:?} does not match its recorded resource")]
    AllocationMismatch(SandboxAllocationKey),
    #[error("unknown sandbox lease {0}")]
    UnknownLease(SandboxLeaseId),
    #[error("sandbox lease {0} was deleted; its workspace is gone")]
    DeletedLease(SandboxLeaseId),
}

/// Provider-neutral durable sandbox leases of one run: the latest record
/// per lease, over the run's resource log.
///
/// The store holds the run's handle weakly: the coordinator (or the
/// command) that opened the run owns the handle and the lease with it, and
/// a write after that owner is gone is refused rather than made under a
/// lease nobody holds.
pub struct ResourceStore {
    logs:          Weak<dyn RunLogs>,
    locator:       String,
    by_lease:      BTreeMap<SandboxLeaseId, SandboxResourceRecord>,
    by_allocation: BTreeMap<SandboxAllocationKey, SandboxLeaseId>,
    next_lease:    u64,
    next_seq:      u64,
    /// What each record's `recorded_at` reads.
    clock:         RecordingClock,
    /// The run's store failure, shared with its other writers. Once any
    /// write has failed, every write here is refused: memory may no longer
    /// match the log (a write that landed but reported a failure), and the
    /// lifetime is ending.
    failure:       Arc<StoreFailure>,
}

impl ResourceStore {
    /// Rebuild the current state of every lease from the resource log. The
    /// caller keeps `logs` for as long as it writes through the store.
    pub async fn load(logs: &Arc<dyn RunLogs>) -> Result<Self, ResourceError> {
        let stored = logs.read(&LogId::Resources).await?;
        let mut by_lease = BTreeMap::new();
        let mut by_allocation = BTreeMap::new();
        let mut next_lease = 0_u64;
        for line in &stored {
            let record: ResourceLogRecord =
                line.decode().map_err(|source| ResourceError::Decode {
                    seq: line.seq,
                    source,
                })?;
            let record = record.body;
            if let Some(existing) = by_allocation.insert(record.allocation.clone(), record.lease)
                && existing != record.lease
            {
                return Err(ResourceError::DuplicateAllocation(record.allocation));
            }
            next_lease = next_lease.max(record.lease.raw().saturating_add(1));
            by_lease.insert(record.lease, record);
        }
        Ok(Self {
            logs: Arc::downgrade(logs),
            locator: logs.locator(),
            by_lease,
            by_allocation,
            next_lease,
            next_seq: stored.len() as u64,
            clock: RecordingClock::default(),
            failure: Arc::new(StoreFailure::default()),
        })
    }

    /// The run's first failed write, once there is one: memory may no
    /// longer match the log.
    pub(crate) fn failed(&self) -> Option<&str> {
        self.failure.get()
    }

    /// Share the run's store failure with its other writers.
    #[must_use]
    pub(crate) fn with_failure(mut self, failure: Arc<StoreFailure>) -> Self {
        self.failure = failure;
        self
    }

    /// Stamp every record appended from now on with `clock`.
    #[must_use]
    pub fn with_clock(mut self, clock: RecordingClock) -> Self {
        self.clock = clock;
        self
    }

    pub fn records(&self) -> impl Iterator<Item = &SandboxResourceRecord> {
        self.by_lease.values()
    }

    /// Reserve a stable scope identity. A dynamic scope's workspace follows
    /// its lease, since its live ScopeId can change after an execution restart.
    pub(crate) async fn reserve_scope(
        &mut self,
        allocation: SandboxAllocationKey,
        provider: &str,
        runtime: ir::RuntimeSpec,
        introduced_by: Option<ExecutionId>,
    ) -> Result<&SandboxResourceRecord, ResourceError> {
        let workspace = if let Some(lease) = self.by_allocation.get(&allocation) {
            self.resolve(*lease)?.workspace.clone()
        } else {
            match &allocation.scope {
                engine::ScopeIdentity::Declared(scope) => {
                    WorkspaceId::scoped(Some(&allocation.invocation.workspace_prefix()), *scope)
                }
                engine::ScopeIdentity::Spliced(_) => WorkspaceId::new(format!(
                    "invocation-{}-lease-{}",
                    allocation.invocation, self.next_lease,
                )),
            }
        };
        self.ensure_record(allocation, provider, workspace, runtime, introduced_by)
            .await
    }

    /// The record of `lease`, tombstones included: replay of a historical
    /// inherited invocation resolves through here.
    pub fn resolve(&self, lease: SandboxLeaseId) -> Result<&SandboxResourceRecord, ResourceError> {
        self.by_lease
            .get(&lease)
            .ok_or(ResourceError::UnknownLease(lease))
    }

    /// The record of `lease` for new work: a tombstone is refused, because
    /// the workspace it named is gone.
    pub fn resolve_usable(
        &self,
        lease: SandboxLeaseId,
    ) -> Result<&SandboxResourceRecord, ResourceError> {
        let record = self.resolve(lease)?;
        if record.state == LeaseState::Deleted {
            return Err(ResourceError::DeletedLease(lease));
        }
        Ok(record)
    }

    /// The lease of `allocation`, reserved on first use. `provider` is the
    /// kind the scope's runtime target names; the sandbox lease manager
    /// records the real one, with its fingerprint, when it allocates.
    pub async fn ensure_record(
        &mut self,
        allocation: SandboxAllocationKey,
        provider: impl Into<SmolStr>,
        workspace: WorkspaceId,
        runtime: ir::RuntimeSpec,
        introduced_by: Option<ExecutionId>,
    ) -> Result<&SandboxResourceRecord, ResourceError> {
        let provider = provider.into();
        if let Some(lease) = self.by_allocation.get(&allocation).copied() {
            let record = self
                .by_lease
                .get(&lease)
                .expect("both resource indexes are written together");
            if record.workspace != workspace || record.runtime != runtime {
                return Err(ResourceError::AllocationMismatch(allocation));
            }
            return Ok(record);
        }
        let lease = SandboxLeaseId::new(self.next_lease);
        let record = SandboxResourceRecord {
            lease,
            allocation: allocation.clone(),
            provider,
            resource_id: None,
            workspace,
            runtime,
            introduced_by,
            state: LeaseState::Allocating,
            pending: None,
            fingerprint: None,
        };
        self.write(&record).await?;
        self.next_lease = self.next_lease.saturating_add(1);
        self.by_allocation.insert(allocation, lease);
        self.by_lease.insert(lease, record);
        Ok(self
            .by_lease
            .get(&lease)
            .expect("the new resource was inserted"))
    }

    /// Change one record, durably: its new state is appended to the log
    /// before the map is updated, so a crash leaves the log and the memory
    /// in agreement.
    pub async fn update(
        &mut self,
        lease: SandboxLeaseId,
        update: impl FnOnce(&mut SandboxResourceRecord),
    ) -> Result<(), ResourceError> {
        let mut record = self.resolve(lease)?.clone();
        update(&mut record);
        self.write(&record).await?;
        self.by_lease.insert(lease, record);
        Ok(())
    }

    async fn write(&mut self, record: &SandboxResourceRecord) -> Result<(), ResourceError> {
        let line = ResourceLogRecord {
            seq:         self.next_seq,
            recorded_at: self.clock.now(),
            body:        record.clone(),
        };
        let stored = Record::encode(&line).map_err(|error| match error {
            store::EncodeError::Encode(source) => ResourceError::Encode(source),
            store::EncodeError::Shape(shape) => {
                ResourceError::Encode(serde_json::Error::custom(shape))
            }
        })?;
        if let Some(first) = self.failure.get() {
            return Err(store::StoreError::backend(
                self.locator.clone(),
                "append",
                format!("an earlier write to the run's store failed: {first}"),
            )
            .into());
        }
        let logs = self.logs.upgrade().ok_or_else(|| {
            store::StoreError::backend(
                self.locator.clone(),
                "append",
                "the run's store handle is gone",
            )
        })?;
        if let Err(error) = logs.append(&LogId::Resources, &[stored]).await {
            self.failure
                .trip(format!("could not append to the resources log: {error}"));
            return Err(error.into());
        }
        self.next_seq += 1;
        Ok(())
    }
}

/// The resource store as the sandbox lease manager's ledger. The
/// coordinator reserves every lease first; an unknown lease here is a
/// caller that acquired without one, and is refused rather than invented.
/// Every write resolves once its record is in the store.
#[derive(Clone)]
pub struct ResourceLedger(Arc<Mutex<ResourceStore>>);

impl ResourceLedger {
    pub fn new(store: Arc<Mutex<ResourceStore>>) -> Self {
        Self(store)
    }

    async fn update(
        &self,
        lease: SandboxLeaseId,
        update: impl FnOnce(&mut SandboxResourceRecord) + Send,
    ) -> Result<(), LedgerError> {
        self.0
            .lock()
            .await
            .update(lease, update)
            .await
            .map_err(|error| LedgerError(error.to_string()))
    }
}

#[async_trait::async_trait]
impl LeaseLedger for ResourceLedger {
    /// Refused once a write to the run's store failed: a record that
    /// landed but reported a failure is not in memory, so no provider call
    /// may act on what memory says (the sweep of unrecorded sandboxes would
    /// delete the one the log names).
    async fn lookup(&self, lease: SandboxLeaseId) -> Result<Option<LeaseRecord>, LedgerError> {
        let store = self.0.lock().await;
        if let Some(first) = store.failed() {
            return Err(LedgerError(format!(
                "an earlier write to the run's store failed: {first}"
            )));
        }
        let Ok(record) = store.resolve(lease) else {
            return Ok(None);
        };
        Ok(Some(LeaseRecord {
            state:       record.state,
            pending:     record.pending,
            provider:    Some(record.provider.clone()),
            resource_id: record.resource_id.clone(),
            fingerprint: record.fingerprint.clone(),
        }))
    }

    async fn allocating(
        &self,
        lease: SandboxLeaseId,
        provider: &str,
        fingerprint: &str,
    ) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Allocating;
            record.pending = None;
            record.provider = SmolStr::new(provider);
            record.fingerprint = Some(SmolStr::new(fingerprint));
        })
        .await
    }

    async fn live(&self, lease: SandboxLeaseId, resource_id: &str) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Live;
            record.pending = None;
            record.resource_id = Some(SmolStr::new(resource_id));
        })
        .await
    }

    async fn pending(
        &self,
        lease: SandboxLeaseId,
        intent: PendingIntent,
    ) -> Result<(), LedgerError> {
        self.update(lease, |record| record.pending = Some(intent))
            .await
    }

    async fn stopped(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Stopped;
            record.pending = None;
        })
        .await
    }

    async fn deleted(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Deleted;
            record.pending = None;
        })
        .await
    }
}
