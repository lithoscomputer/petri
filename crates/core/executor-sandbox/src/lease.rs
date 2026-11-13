//! Sandboxes by lease: one live handle per durable lease, shared by every
//! holder, fenced only during crash recovery.
//!
//! A container sandbox lives as long as the lease that names its workspace.
//! The coordinator allocates the lease and hands it to each execution that
//! runs in the sandbox; this manager keeps the one live handle and a holder
//! count. A second acquire on a live lease — a restarted execution, an
//! inherited nested invocation, concurrent scopes over one workspace —
//! reuses the handle and never stops the sandbox. Per-execution release only
//! drops a holder. The sandbox is stopped when its invocation releases the
//! lease and kept or deleted by retention; `petri sandbox prune` deletes
//! kept ones later.
//!
//! # Recovery
//!
//! When no live holder exists (a fresh process over a run dir), acquire
//! reconciles: it attaches the lease's recorded reference, or finds the one
//! sandbox under its workspace label on the entry it was admitted to, stops
//! it once (ending whatever a dead execution left running), and starts it
//! once before any holder resumes. More than one match is an error, never an
//! arbitrary choice; a confirmed record whose resource is missing is an
//! error, never a silent replacement, because its workspace was lost. A
//! provider whose sandboxes last only as long as their owner process
//! (`Durability::Process`, built-in Host) is the exception: its sandbox is
//! created again from the admitted spec, over the same workspace.
//!
//! A lease recorded before the shared sandbox API names a provider kind and
//! the provider's id, with no reference. Its sandbox is found by the run's
//! label across every entry and matched on that id; nothing about its
//! namespace is inferred from today's configuration.
//!
//! # The ledger
//!
//! Every transition is written to a [`LeaseLedger`] before the provider call
//! that makes it true (`allocating`, with the admission, before create;
//! `pending: stop` before stop, `pending: delete` before delete) and confirmed
//! only after the provider succeeds. Recovery repeats a pending idempotent
//! operation. The coordinator's resource store is the durable ledger; a
//! standalone driver uses an in-memory one and releases its sandbox at scope
//! release.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use executor::{EnvError, ReleaseReport, Retention, SandboxLeaseId, ScopeOutcome};
use ir::SandboxReference;
use sandbox_driver::SandboxId;
use sandbox_driver::contract::{
    Descriptor, Durability, Error as DriverError, LabelFilter, NamespaceId, ProviderName, Sandbox,
    SandboxBuilder, SandboxRef, SandboxSpec, Sandboxes, Source,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio::task::JoinSet;

use crate::catalog::{self, Credentials};
use crate::run::{LEASE_LABEL, RUN_LABEL, RunIdentity};
use crate::{BACKEND, LostSandbox, acquire_failed};

/// Where a lease's sandbox stands, durably.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// Reserved; the provider may or may not have created the resource.
    #[default]
    Allocating,
    /// The resource exists and was last known running.
    Live,
    /// The resource exists, stopped; its workspace is kept.
    Stopped,
    /// The resource was deleted. The record stays as a tombstone.
    Deleted,
}

/// An operation written down before it is asked of the provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingIntent {
    Stop,
    Delete,
}

/// The durable facts about one lease the manager reads and writes.
#[derive(Clone, Debug, PartialEq)]
pub struct LeaseRecord {
    pub state:       LeaseState,
    pub pending:     Option<PendingIntent>,
    /// The provider entry, once known; a provider kind on a lease recorded
    /// before the shared sandbox API.
    pub provider:    Option<SmolStr>,
    /// The provider's resource id, once known.
    pub resource_id: Option<SmolStr>,
    /// The non-secret provider fingerprint a lease allocated before the
    /// shared sandbox API recorded. Read, never written.
    pub fingerprint: Option<SmolStr>,
    /// Where the lease's sandbox is admitted, written before the create.
    pub admission:   Option<SandboxAdmission>,
    /// The sandbox's durable reference, once it exists.
    pub reference:   Option<SandboxReference>,
}

/// What a lease's sandbox was admitted as, recorded before the provider is
/// asked to create it: the provider entry and the namespace it reported, the
/// catalog environment the template came from, and the resolved spec. A
/// recovery uses these, never the catalog's current state, so an edited
/// environment cannot redirect a lease.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SandboxAdmission {
    pub provider:    SmolStr,
    pub namespace:   SmolStr,
    pub environment: SmolStr,
    /// The resolved `SandboxSpec`, as its JSON encoding.
    pub spec:        serde_json::Value,
}

/// Why a ledger operation failed.
#[derive(Debug, thiserror::Error)]
#[error("sandbox lease ledger: {0}")]
pub struct LedgerError(pub String);

/// The durable record of every lease, as the manager needs it. Every write
/// resolves only once its record is durable: the manager awaits `allocating`
/// before `provider.create`, and `pending` before a stop or a delete, so no
/// provider mutation starts before its intent is stored. A ledger that
/// queues the write and returns breaks crash recovery.
#[async_trait::async_trait]
pub trait LeaseLedger: Send + Sync {
    async fn lookup(&self, lease: SandboxLeaseId) -> Result<Option<LeaseRecord>, LedgerError>;

    /// Records that the provider is about to be asked to create the
    /// resource, and what it is admitted as: entry, namespace, environment
    /// and resolved spec.
    async fn allocating(
        &self,
        lease: SandboxLeaseId,
        admission: &SandboxAdmission,
    ) -> Result<(), LedgerError>;

    /// Records the resource as created (or found) and running, under its
    /// durable reference.
    async fn live(
        &self,
        lease: SandboxLeaseId,
        reference: &SandboxReference,
    ) -> Result<(), LedgerError>;

    /// Records an intent before the provider call. The intent stays until
    /// the matching confirmation.
    async fn pending(
        &self,
        lease: SandboxLeaseId,
        intent: PendingIntent,
    ) -> Result<(), LedgerError>;

    async fn stopped(&self, lease: SandboxLeaseId) -> Result<(), LedgerError>;

    async fn deleted(&self, lease: SandboxLeaseId) -> Result<(), LedgerError>;
}

/// A ledger that forgets everything when the process ends: for a driver
/// with no coordinator, whose sandboxes end with their scopes.
#[derive(Default)]
pub struct MemoryLedger {
    records: Mutex<HashMap<SandboxLeaseId, LeaseRecord>>,
}

impl MemoryLedger {
    fn update(&self, lease: SandboxLeaseId, update: impl FnOnce(&mut LeaseRecord)) {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let record = records.entry(lease).or_insert_with(|| LeaseRecord {
            state:       LeaseState::Allocating,
            pending:     None,
            provider:    None,
            resource_id: None,
            fingerprint: None,
            admission:   None,
            reference:   None,
        });
        update(record);
    }
}

#[async_trait::async_trait]
impl LeaseLedger for MemoryLedger {
    async fn lookup(&self, lease: SandboxLeaseId) -> Result<Option<LeaseRecord>, LedgerError> {
        Ok(self
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&lease)
            .cloned())
    }

    async fn allocating(
        &self,
        lease: SandboxLeaseId,
        admission: &SandboxAdmission,
    ) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Allocating;
            record.provider = Some(admission.provider.clone());
            record.admission = Some(admission.clone());
        });
        Ok(())
    }

    async fn live(
        &self,
        lease: SandboxLeaseId,
        reference: &SandboxReference,
    ) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Live;
            record.pending = None;
            record.provider = Some(reference.provider.clone());
            record.resource_id = Some(reference.id.clone());
            record.reference = Some(reference.clone());
        });
        Ok(())
    }

    async fn pending(
        &self,
        lease: SandboxLeaseId,
        intent: PendingIntent,
    ) -> Result<(), LedgerError> {
        self.update(lease, |record| record.pending = Some(intent));
        Ok(())
    }

    async fn stopped(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Stopped;
            record.pending = None;
        });
        Ok(())
    }

    async fn deleted(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.update(lease, |record| {
            record.state = LeaseState::Deleted;
            record.pending = None;
        });
        Ok(())
    }
}

/// One lease as the run's records name it, for reconciliation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedLease {
    pub lease:        SandboxLeaseId,
    /// The workspace the lease names: the reconcile label.
    pub workspace_id: String,
}

/// What a reconciliation did to the run's resources on one provider.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Leases whose resource a lost create had produced: attached, fenced
    /// and recorded live.
    pub adopted:  Vec<(SandboxLeaseId, SandboxId)>,
    /// Resources under the run's labels that no record names: removed.
    pub removed:  Vec<SandboxId>,
    /// What could not be reconciled, with why. A problem leaves the record
    /// as it was; the next acquire or prune tries again.
    pub problems: Vec<String>,
}

impl ReconcileReport {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    pub fn merge(&mut self, other: Self) {
        self.adopted.extend(other.adopted);
        self.removed.extend(other.removed);
        self.problems.extend(other.problems);
    }
}

/// A lease recorded before the shared sandbox API on the Host plugin. The
/// built-in Host cannot reach another process's sandboxes; the workspace
/// it left is a directory under the run dir.
const LEGACY_HOST: &str = "host";

/// Per-lease state, serialized by one lock so allocation and recovery for
/// one lease never race.
#[derive(Default)]
struct LeaseSlot {
    live:    Option<Arc<dyn Sandbox>>,
    holders: usize,
}

/// What an acquire needs to know beyond the lease.
pub(crate) struct LeaseRequest<'a> {
    pub lease:        SandboxLeaseId,
    /// The workspace the lease names, for the reconcile label.
    pub workspace_id: &'a str,
    /// A standalone acquisition has no coordinator to release its lease.
    pub standalone:   bool,
}

/// A sandbox ready to be created: the environment its template came from,
/// the entry that environment names, and the builder with the resolved
/// spec.
pub(crate) struct Prepared {
    pub(crate) environment: String,
    pub(crate) provider:    ProviderName,
    pub(crate) builder:     SandboxBuilder,
    /// Run credentials registered for this create, dropped once it settles.
    pub(crate) credentials: Vec<String>,
}

/// Where a lease's sandbox is on its provider.
enum Located {
    Attached(Arc<dyn Sandbox>),
    /// Not on the provider: never created, deleted, or, on a provider whose
    /// sandboxes last as long as their owner process, gone with it.
    Gone,
}

/// One live handle and a holder count per lease, over the run's catalog.
pub struct SandboxLeaseManager {
    sandboxes:    Sandboxes,
    credentials:  Arc<Credentials>,
    ledger:       Arc<dyn LeaseLedger>,
    identity:     Arc<RunIdentity>,
    lost_sandbox: LostSandbox,
    leases:       Mutex<HashMap<SandboxLeaseId, Arc<AsyncMutex<LeaseSlot>>>>,
    cleanup:      Mutex<JoinSet<()>>,
}

impl SandboxLeaseManager {
    pub(crate) fn new(
        sandboxes: Sandboxes,
        credentials: Arc<Credentials>,
        ledger: Arc<dyn LeaseLedger>,
        identity: Arc<RunIdentity>,
    ) -> Self {
        Self {
            sandboxes,
            credentials,
            ledger,
            identity,
            lost_sandbox: LostSandbox::Refuse,
            leases: Mutex::new(HashMap::new()),
            cleanup: Mutex::new(JoinSet::new()),
        }
    }

    /// What acquire does when a recorded sandbox is gone from the provider;
    /// [`LostSandbox::Refuse`] until set.
    #[must_use]
    pub fn with_lost_sandbox(mut self, policy: LostSandbox) -> Self {
        self.lost_sandbox = policy;
        self
    }

    pub fn sandboxes(&self) -> &Sandboxes {
        &self.sandboxes
    }

    fn slot(&self, lease: SandboxLeaseId) -> Arc<AsyncMutex<LeaseSlot>> {
        Arc::clone(
            self.leases
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(lease)
                .or_default(),
        )
    }

    /// A lease record the ledger could not write: the run's store failed,
    /// not the provider.
    fn ledger_failed(error: &LedgerError) -> EnvError {
        EnvError::Store {
            message: error.to_string(),
        }
    }

    /// The sandbox for `request.lease`, allocating, attaching, or reusing
    /// as the lease's state requires, and counting the caller as a holder.
    /// `prepare` resolves the create spec, given the labels every Petri
    /// sandbox carries.
    pub(crate) async fn acquire<Fut>(
        self: &Arc<Self>,
        request: LeaseRequest<'_>,
        prepare: impl FnOnce(Vec<(String, String)>) -> Fut + Send,
    ) -> Result<AcquiredSandbox, EnvError>
    where
        Fut: Future<Output = Result<Prepared, EnvError>> + Send,
    {
        // An owned guard: a create hands it to its task, so an acquire that
        // is dropped mid-create still keeps the lease locked until the
        // create has settled one way or the other.
        let mut slot = self.slot(request.lease).lock_owned().await;
        if let Some(live) = &slot.live {
            let sandbox = Arc::clone(live);
            slot.holders += 1;
            return Ok(self.acquired(&request, sandbox));
        }
        let lease = request.lease;
        let record = self
            .ledger
            .lookup(lease)
            .await
            .map_err(|error| Self::ledger_failed(&error))?;
        let labels = self.identity.labels(lease, request.workspace_id);
        let Some(record) = record.filter(attempted) else {
            // Nothing was ever asked of a provider. A create a crash left
            // unrecorded comes back as `AlreadyExists` and is adopted.
            let prepared = match prepare(labels).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    // A refused scope still owes its lease a release, which
                    // finds nothing on any provider and writes the
                    // tombstone: the run records `scope.released` for every
                    // lease it reserved.
                    if let Err(ledger) = self.ledger.pending(lease, PendingIntent::Delete).await {
                        tracing::warn!(lease = lease.raw(), error = %ledger, "the refused lease's release was not recorded");
                    }
                    return Err(error);
                }
            };
            return self.create(&request, prepared, slot).await;
        };
        if record.state == LeaseState::Deleted {
            return Err(deleted_lease(lease));
        }
        let sandbox = match self.locate(&record, lease).await? {
            Located::Attached(sandbox) => self.recover(lease, &record, sandbox).await?,
            Located::Gone => {
                if record.pending == Some(PendingIntent::Delete) {
                    // Delete completed before its confirmation was durable.
                    self.finish_delete(lease, &record).await?;
                    return Err(deleted_lease(lease));
                }
                let process = self.process_durable(&record).await?;
                if record.state != LeaseState::Allocating
                    && !process
                    && self.lost_sandbox == LostSandbox::Refuse
                {
                    return Err(EnvError::backend(
                        BACKEND,
                        "acquire",
                        format!(
                            "sandbox lease {lease} is recorded {:?} but its sandbox is not on the \
                             provider; its workspace was lost and Petri will not replace it \
                             silently",
                            record.state
                        ),
                    ));
                }
                if record.state != LeaseState::Allocating && !process {
                    tracing::warn!(
                        lease = lease.raw(),
                        state = ?record.state,
                        "the lease's recorded sandbox is gone from the provider; creating a \
                         fresh one with an empty workspace"
                    );
                }
                // The admitted spec, never the catalog's current template:
                // a process-durable sandbox comes back over the same
                // workspace, and a lost create is retried as admitted.
                let prepared = match &record.admission {
                    Some(admission) => self.readmit(admission).await?,
                    None => prepare(labels).await?,
                };
                return self.create(&request, prepared, slot).await;
            }
        };
        slot.live = Some(Arc::clone(&sandbox));
        slot.holders += 1;
        Ok(self.acquired(&request, sandbox))
    }

    fn acquired(
        self: &Arc<Self>,
        request: &LeaseRequest<'_>,
        sandbox: Arc<dyn Sandbox>,
    ) -> AcquiredSandbox {
        AcquiredSandbox {
            manager:    self.clone(),
            lease:      request.lease,
            standalone: request.standalone,
            sandbox:    Some(sandbox),
        }
    }

    /// The prepared create of an admitted spec, on the entry it was
    /// admitted to. An environment that now names another entry is
    /// refused: the lease's sandbox lives where it was admitted.
    async fn readmit(&self, admission: &SandboxAdmission) -> Result<Prepared, EnvError> {
        let spec: SandboxSpec =
            serde_json::from_value(admission.spec.clone()).map_err(|error| {
                EnvError::backend(
                    BACKEND,
                    "acquire",
                    format!("the lease's admitted spec could not be read: {error}"),
                )
            })?;
        let environment = self
            .sandboxes
            .environment(&admission.environment)
            .await
            .map_err(|error| acquire_failed(&error))?;
        if environment.provider.as_str() != admission.provider {
            return Err(EnvError::backend(
                BACKEND,
                "acquire",
                format!(
                    "environment `{}` now names provider entry `{}`, but the lease was admitted \
                     to `{}`; restore the environment before the run continues",
                    admission.environment, environment.provider, admission.provider
                ),
            ));
        }
        let builder = self
            .sandboxes
            .builder(&admission.environment)
            .await
            .map_err(|error| acquire_failed(&error))?
            .spec(spec);
        Ok(Prepared {
            environment: admission.environment.to_string(),
            provider: environment.provider,
            builder,
            credentials: Vec::new(),
        })
    }

    /// Creates the sandbox, recording the admission first and the reference
    /// after, so a crash on either side of the create is reconciled.
    ///
    /// The create runs in its own task, which owns the lease's lock: an
    /// acquire nobody waited out (a cancelled scope, a sweep aborting the
    /// run) drops this future, and the provider's create, already sent,
    /// still completes, with the lease locked until it has. The task
    /// returns an acquisition guard. Dropping either the task's unread
    /// result or later environment initialization releases that holder.
    async fn create(
        self: &Arc<Self>,
        request: &LeaseRequest<'_>,
        prepared: Prepared,
        mut slot: OwnedMutexGuard<LeaseSlot>,
    ) -> Result<AcquiredSandbox, EnvError> {
        let lease = request.lease;
        let standalone = request.standalone;
        let Prepared {
            environment,
            provider,
            builder,
            credentials,
        } = prepared;
        let spec = builder.resolved_spec().clone();
        let namespace = self.describe(provider.as_str()).await?.namespace.id;
        let admission = SandboxAdmission {
            provider:    SmolStr::new(provider.as_str()),
            namespace:   SmolStr::new(namespace.as_str()),
            environment: SmolStr::new(&environment),
            spec:        serde_json::to_value(&spec).map_err(|error| {
                EnvError::backend(BACKEND, "acquire", format!("encoding the spec: {error}"))
            })?,
        };
        if let Some(directory) = self.designated(&spec) {
            fs::create_dir_all(&directory)
                .await
                .map_err(|error| EnvError::workspace("create", directory.display(), error))?;
        }
        self.ledger
            .allocating(lease, &admission)
            .await
            .map_err(|error| Self::ledger_failed(&error))?;
        let manager = self.clone();
        let created = tokio::spawn(async move {
            let created = builder.create().await;
            manager.credentials.remove(&credentials);
            // A create a crash left unrecorded under the same name: the
            // lease's own sandbox, adopted and fenced like a recovery.
            let adopted = match created {
                Ok(sandbox) => Ok(Arc::from(sandbox)),
                Err(DriverError::AlreadyExists { id, .. }) => Err(Ok(SandboxRef {
                    provider,
                    namespace,
                    id: SandboxId::try_new(id).map_err(|error| {
                        EnvError::backend(BACKEND, "acquire", error.to_string())
                    })?,
                })),
                // TODO(sandbox-driver): the Docker provider reports a name in
                // use as `InvalidSpec` at `name`, not the contract's
                // `AlreadyExists` with the holder's id. Until it does, the
                // holder is the lease's one sandbox on the entry, by label.
                Err(DriverError::InvalidSpec { path, message }) if path == "name" => {
                    match manager
                        .find_on(&provider, manager.lease_filter(lease))
                        .await?
                        .as_slice()
                    {
                        [one] => Err(Ok(one.clone())),
                        _ => Err(Err(DriverError::InvalidSpec { path, message })),
                    }
                }
                Err(error) => Err(Err(error)),
            };
            let sandbox: Arc<dyn Sandbox> = match adopted {
                Ok(sandbox) => sandbox,
                Err(Ok(reference)) => {
                    tracing::info!(lease = lease.raw(), sandbox = %reference.id, "adopting an unrecorded create");
                    let record = manager.record(lease).await?;
                    match manager.attach(&reference, false).await? {
                        Located::Attached(sandbox) => {
                            manager.recover(lease, &record, sandbox).await?
                        }
                        Located::Gone => {
                            return Err(EnvError::backend(
                                BACKEND,
                                "acquire",
                                format!(
                                    "the provider reported sandbox {} under this lease's name but \
                                     it could not be attached",
                                    reference.id
                                ),
                            ));
                        }
                    }
                }
                Err(Err(error)) => {
                    if standalone {
                        let report = manager
                            .release_locked(lease, Retention::Never, ScopeOutcome::Failed, slot)
                            .await;
                        for problem in report.problems {
                            tracing::warn!(
                                lease = lease.raw(),
                                problem,
                                "failed sandbox create cleanup"
                            );
                        }
                    }
                    return Err(acquire_failed(&error));
                }
            };
            let acquired = AcquiredSandbox {
                manager: manager.clone(),
                lease,
                standalone,
                sandbox: Some(sandbox.clone()),
            };
            slot.live = Some(sandbox.clone());
            slot.holders += 1;
            manager
                .ledger
                .live(lease, &to_reference(&sandbox.reference()))
                .await
                .map_err(|error| Self::ledger_failed(&error))?;
            drop(slot);
            Ok::<_, EnvError>(acquired)
        });
        created.await.map_err(|error| {
            EnvError::backend(
                BACKEND,
                "acquire",
                format!("the sandbox create task failed: {error}"),
            )
        })?
    }

    async fn record(&self, lease: SandboxLeaseId) -> Result<LeaseRecord, EnvError> {
        self.ledger
            .lookup(lease)
            .await
            .map_err(|error| Self::ledger_failed(&error))?
            .ok_or_else(|| {
                EnvError::backend(BACKEND, "acquire", format!("lease {lease} has no record"))
            })
    }

    async fn describe(&self, entry: &str) -> Result<Descriptor, EnvError> {
        self.sandboxes
            .provider(entry)
            .map_err(|error| acquire_failed(&error))?
            .describe()
            .await
            .map_err(|error| acquire_failed(&error))
    }

    /// Whether the lease's sandbox lasts only as long as its owner process,
    /// so that a missing one is expected after a restart.
    async fn process_durable(&self, record: &LeaseRecord) -> Result<bool, EnvError> {
        match &record.admission {
            Some(admission) => {
                Ok(self.describe(&admission.provider).await?.durability == Durability::Process)
            }
            None => Ok(record.provider.as_deref() == Some(LEGACY_HOST)),
        }
    }

    /// Finds the lease's sandbox. A lease admitted through the shared API
    /// is looked for only on its entry, after that entry's namespace is
    /// checked against the admitted one; an older lease is found by the
    /// run's label and its recorded id.
    async fn locate(
        &self,
        record: &LeaseRecord,
        lease: SandboxLeaseId,
    ) -> Result<Located, EnvError> {
        let Some(admission) = &record.admission else {
            return self.locate_legacy(record, lease).await;
        };
        let descriptor = self.check_namespace(admission).await?;
        let process = descriptor.durability == Durability::Process;
        let reference = if let Some(reference) = &record.reference {
            sandbox_ref(reference)?
        } else {
            let entry = entry_name(&admission.provider)?;
            let found = self.find_on(&entry, self.lease_filter(lease)).await?;
            match found.as_slice() {
                [] => return Ok(Located::Gone),
                [one] => one.clone(),
                many => {
                    return Err(EnvError::backend(
                        BACKEND,
                        "acquire",
                        format!(
                            "{} sandboxes belong to lease {lease}; Petri does not choose one \
                             arbitrarily",
                            many.len()
                        ),
                    ));
                }
            }
        };
        self.attach(&reference, process).await
    }

    /// The run-label lookup of a lease recorded before the shared sandbox
    /// API: exactly one sandbox under `petri.run` with the recorded id.
    async fn locate_legacy(
        &self,
        record: &LeaseRecord,
        lease: SandboxLeaseId,
    ) -> Result<Located, EnvError> {
        if record.provider.as_deref() == Some(LEGACY_HOST) {
            return Ok(Located::Gone);
        }
        let found = catalog::find(
            &self.sandboxes,
            LabelFilter::eq(RUN_LABEL, self.identity.run_id()),
        )
        .await
        .map_err(|error| acquire_failed(&error))?;
        let lease_value = lease.raw().to_string();
        let matches: Vec<_> = found
            .sandboxes
            .iter()
            .filter(|summary| match &record.resource_id {
                Some(id) => summary.reference.id.as_str() == id.as_str(),
                None => summary.labels.get(LEASE_LABEL) == Some(&lease_value),
            })
            .collect();
        let unsearched: Vec<String> = found
            .failures
            .iter()
            .map(|failure| format!("{}: {}", failure.entry, failure.error))
            .collect();
        match matches.as_slice() {
            [one] => self.attach(&one.reference, false).await,
            [] if record.resource_id.is_none() && unsearched.is_empty() => Ok(Located::Gone),
            [] => Err(EnvError::backend(
                BACKEND,
                "acquire",
                format!(
                    "sandbox {} of lease {lease} (recorded on {} before the shared sandbox API) \
                     was not found under {RUN_LABEL}={} on any provider entry{}; it is gone, or \
                     its provider does not list sandboxes the legacy API created",
                    record.resource_id.as_deref().unwrap_or("<unrecorded>"),
                    record.provider.as_deref().unwrap_or("an unknown provider"),
                    self.identity.run_id(),
                    if unsearched.is_empty() {
                        String::new()
                    } else {
                        format!(" (not searched: {})", unsearched.join("; "))
                    }
                ),
            )),
            many => Err(EnvError::backend(
                BACKEND,
                "acquire",
                format!(
                    "{} sandboxes match lease {lease} under {RUN_LABEL}; Petri does not choose \
                     one arbitrarily",
                    many.len()
                ),
            )),
        }
    }

    async fn attach(&self, reference: &SandboxRef, process: bool) -> Result<Located, EnvError> {
        match self.sandboxes.attach(reference).await {
            Ok(sandbox) => Ok(Located::Attached(Arc::from(sandbox))),
            Err(DriverError::NotFound { .. }) => Ok(Located::Gone),
            // A process-durable sandbox is its owner's; another process's
            // is gone with that process.
            Err(DriverError::Unavailable { .. }) if process => Ok(Located::Gone),
            Err(error) => Err(acquire_failed(&error)),
        }
    }

    /// The entry's current descriptor, refusing a namespace other than the
    /// admitted one: the entry now reaches a different backend.
    async fn check_namespace(&self, admission: &SandboxAdmission) -> Result<Descriptor, EnvError> {
        let descriptor = self.describe(&admission.provider).await?;
        if descriptor.namespace.id.as_str() != admission.namespace {
            return Err(EnvError::backend(
                BACKEND,
                "acquire",
                format!(
                    "provider entry `{}` now reaches namespace `{}`, but the lease was admitted \
                     on `{}`; a changed DOCKER_HOST, Daytona organization or entry must be \
                     restored before the run continues",
                    admission.provider, descriptor.namespace.id, admission.namespace
                ),
            ));
        }
        Ok(descriptor)
    }

    fn lease_filter(&self, lease: SandboxLeaseId) -> LabelFilter {
        let mut filter = LabelFilter::eq(RUN_LABEL, self.identity.run_id());
        filter
            .0
            .insert(LEASE_LABEL.to_owned(), lease.raw().to_string());
        filter
    }

    /// The matches on one entry. A failure to search that entry is an
    /// error: it cannot vouch that nothing is there.
    async fn find_on(
        &self,
        entry: &ProviderName,
        filter: LabelFilter,
    ) -> Result<Vec<SandboxRef>, EnvError> {
        let found = catalog::find(&self.sandboxes, filter)
            .await
            .map_err(|error| acquire_failed(&error))?;
        if let Some(failure) = found
            .failures
            .iter()
            .find(|failure| failure.entry == *entry)
        {
            return Err(acquire_failed(&failure.error));
        }
        Ok(found
            .sandboxes
            .into_iter()
            .filter(|summary| summary.reference.provider == *entry)
            .map(|summary| summary.reference)
            .collect())
    }

    /// Fences an attached sandbox: one stop, then one start.
    async fn recover(
        &self,
        lease: SandboxLeaseId,
        record: &LeaseRecord,
        sandbox: Arc<dyn Sandbox>,
    ) -> Result<Arc<dyn Sandbox>, EnvError> {
        let reference = sandbox.reference();
        if record.pending == Some(PendingIntent::Delete) {
            // The invocation meant to delete it and died before the
            // provider confirmed; finish that, and the lease is gone.
            self.destroy(&reference)
                .await
                .map_err(|error| acquire_failed(&error))?;
            self.finish_delete(lease, record).await?;
            return Err(EnvError::backend(
                BACKEND,
                "acquire",
                format!("sandbox lease {lease} was being deleted; its workspace is gone"),
            ));
        }
        tracing::info!(lease = lease.raw(), sandbox = %reference.id, "fencing a recorded sandbox");
        self.ledger
            .pending(lease, PendingIntent::Stop)
            .await
            .map_err(|error| Self::ledger_failed(&error))?;
        sandbox
            .stop()
            .await
            .map_err(|error| acquire_failed(&error))?;
        sandbox
            .start()
            .await
            .map_err(|error| acquire_failed(&error))?;
        self.ledger
            .live(lease, &to_reference(&reference))
            .await
            .map_err(|error| Self::ledger_failed(&error))?;
        Ok(sandbox)
    }

    /// Deletion is complete when the resource is already absent.
    async fn destroy(&self, reference: &SandboxRef) -> Result<(), DriverError> {
        match self.sandboxes.destroy(reference).await {
            Ok(()) | Err(DriverError::NotFound { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// The designated host directory an admitted spec names, when it is
    /// this run's: Petri chose it, so Petri makes it and removes it.
    fn designated(&self, spec: &SandboxSpec) -> Option<PathBuf> {
        if spec.source != Source::HostDirectory
            || spec.workspace_ownership != Some(sandbox_driver::WorkspaceOwnership::Designated)
        {
            return None;
        }
        let directory = PathBuf::from(spec.working_directory.as_ref()?);
        directory
            .starts_with(self.identity.run_dir())
            .then_some(directory)
    }

    /// Removes the run's designated workspace a deleted lease named, then
    /// writes the tombstone. Returns whether a workspace was there.
    async fn finish_delete(
        &self,
        lease: SandboxLeaseId,
        record: &LeaseRecord,
    ) -> Result<bool, EnvError> {
        let spec = record.admission.as_ref().and_then(|admission| {
            serde_json::from_value::<SandboxSpec>(admission.spec.clone()).ok()
        });
        let mut removed = false;
        if let Some(directory) = spec.as_ref().and_then(|spec| self.designated(spec)) {
            match fs::remove_dir_all(&directory).await {
                Ok(()) => removed = true,
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(EnvError::workspace("remove", directory.display(), error));
                }
            }
        }
        self.ledger
            .deleted(lease)
            .await
            .map_err(|error| Self::ledger_failed(&error))?;
        Ok(removed)
    }

    /// Drops one holder. Never stops the sandbox: that is the lease
    /// release's job.
    pub async fn release_holder(&self, lease: SandboxLeaseId) -> usize {
        let slot = self.slot(lease);
        let mut slot = slot.lock().await;
        slot.holders = slot.holders.saturating_sub(1);
        slot.holders
    }

    async fn abandon(&self, lease: SandboxLeaseId, standalone: bool) {
        let mut slot = self.slot(lease).lock_owned().await;
        slot.holders = slot.holders.saturating_sub(1);
        if standalone && slot.holders == 0 {
            let report = self
                .release_locked(lease, Retention::Never, ScopeOutcome::Failed, slot)
                .await;
            for problem in report.problems {
                tracing::warn!(
                    lease = lease.raw(),
                    problem,
                    "abandoned sandbox cleanup failed"
                );
            }
        }
    }

    fn abandon_in_background(self: &Arc<Self>, lease: SandboxLeaseId, standalone: bool) {
        let mut cleanup = self.cleanup.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(result) = cleanup.try_join_next() {
            if let Err(error) = result {
                tracing::warn!(%error, "sandbox cleanup task failed");
            }
        }
        let manager = self.clone();
        cleanup.spawn(async move { manager.abandon(lease, standalone).await });
    }

    /// The live handle, if the lease has one in this process.
    pub async fn live(&self, lease: SandboxLeaseId) -> Option<Arc<dyn Sandbox>> {
        let slot = self.slot(lease);
        let slot = slot.lock().await;
        slot.live.clone()
    }

    /// Ends the lease: stops the sandbox, then keeps it stopped or deletes
    /// it as `retention` decides for `outcome`. Each step is written to the
    /// ledger as an intent first and confirmed after.
    pub async fn release_lease(
        &self,
        lease: SandboxLeaseId,
        retention: Retention,
        outcome: ScopeOutcome,
    ) -> ReleaseReport {
        let slot = self.slot(lease).lock_owned().await;
        self.release_locked(lease, retention, outcome, slot).await
    }

    async fn release_locked(
        &self,
        lease: SandboxLeaseId,
        retention: Retention,
        outcome: ScopeOutcome,
        mut slot: OwnedMutexGuard<LeaseSlot>,
    ) -> ReleaseReport {
        let report = ReleaseReport::default();
        let live = slot.live.take();
        slot.holders = 0;
        let record = match self.ledger.lookup(lease).await {
            Ok(Some(record)) if record.state != LeaseState::Deleted => record,
            Ok(_) => return report,
            Err(error) => return report.problem(error.to_string()),
        };
        if live.is_none() && !attempted(&record) {
            // No allocation was attempted, so no provider cleanup is needed.
            // A refused lease, marked for delete, ends as a tombstone.
            if record.pending == Some(PendingIntent::Delete)
                && let Err(error) = self.finish_delete(lease, &record).await
            {
                return report.problem(error.to_string());
            }
            return report;
        }
        let keep = record.pending != Some(PendingIntent::Delete) && retention.keeps(outcome);
        let located = match live {
            Some(sandbox) => Located::Attached(sandbox),
            None => match self.locate(&record, lease).await {
                Ok(located) => located,
                Err(error) => return report.problem(error.to_string()),
            },
        };
        let sandbox = match located {
            Located::Attached(sandbox) => sandbox,
            Located::Gone => return self.release_gone(lease, &record, keep, report).await,
        };
        let reference = sandbox.reference();
        if reference.provider.as_str() != record.provider.as_deref().unwrap_or_default()
            || record.reference.is_none()
        {
            // A create that reached the provider but never its record:
            // name the sandbox first, so a kept lease names what it keeps.
            if let Err(error) = self.ledger.live(lease, &to_reference(&reference)).await {
                return report.problem(error.to_string());
            }
        }
        let id = &reference.id;
        let intent = if keep {
            PendingIntent::Stop
        } else {
            PendingIntent::Delete
        };
        if let Err(error) = self.ledger.pending(lease, intent).await {
            return report.problem(error.to_string());
        }
        if keep {
            match sandbox.stop().await {
                Ok(()) => {
                    if let Err(error) = self.ledger.stopped(lease).await {
                        return report.problem(error.to_string());
                    }
                    report.kept(format!("sandbox {id} (stopped, lease {lease})"))
                }
                // Someone outside the run deleted the sandbox: there is
                // nothing left to keep. The lease ends as a delete that
                // finds nothing ends, and the release says what was lost.
                Err(DriverError::NotFound { .. }) => match self.ledger.deleted(lease).await {
                    Ok(()) => report.problem(format!(
                        "sandbox {id} was gone before it could be kept; lease {lease} is deleted"
                    )),
                    Err(error) => report.problem(error.to_string()),
                },
                Err(error) => report.problem(format!("sandbox {id} stop failed: {error}")),
            }
        } else {
            match self.destroy(&reference).await {
                Ok(()) => match self.finish_delete(lease, &record).await {
                    Ok(_) => report.released(format!("sandbox {id} (lease {lease})")),
                    Err(error) => report.problem(error.to_string()),
                },
                Err(error) => report.problem(format!("sandbox {id} delete failed: {error}")),
            }
        }
    }

    /// The release of a lease whose sandbox is not on its provider.
    async fn release_gone(
        &self,
        lease: SandboxLeaseId,
        record: &LeaseRecord,
        keep: bool,
        report: ReleaseReport,
    ) -> ReleaseReport {
        let process = match self.process_durable(record).await {
            Ok(process) => process,
            Err(error) => return report.problem(error.to_string()),
        };
        // A process-durable sandbox went with its owner; its workspace, a
        // directory of the run's, is what retention keeps or removes.
        if process && keep {
            return match self.ledger.stopped(lease).await {
                Ok(()) => report.kept(format!(
                    "workspace of lease {lease} (its sandbox ended with its owner process)"
                )),
                Err(error) => report.problem(error.to_string()),
            };
        }
        let lost = record.state != LeaseState::Allocating && !process && keep;
        match self.finish_delete(lease, record).await {
            Ok(_) if lost => report.problem(format!(
                "the sandbox of lease {lease} was gone before it could be kept; the lease is \
                 deleted"
            )),
            Ok(_) => report,
            Err(error) => report.problem(error.to_string()),
        }
    }

    /// Deletes a recorded sandbox without a live handle: the prune path.
    /// The namespace must match, the intent is written first, and the
    /// record becomes a tombstone only after the provider confirms. The ids
    /// deleted come back; an empty list means nothing was on the provider.
    pub async fn delete_recorded(&self, lease: SandboxLeaseId) -> Result<Vec<SandboxId>, EnvError> {
        let slot = self.slot(lease);
        let mut slot = slot.lock().await;
        slot.live = None;
        slot.holders = 0;
        let Some(record) = self
            .ledger
            .lookup(lease)
            .await
            .map_err(|error| Self::ledger_failed(&error))?
        else {
            return Ok(Vec::new());
        };
        if record.state == LeaseState::Deleted {
            return Ok(Vec::new());
        }
        let located = if attempted(&record) {
            self.locate(&record, lease).await?
        } else {
            Located::Gone
        };
        self.ledger
            .pending(lease, PendingIntent::Delete)
            .await
            .map_err(|error| Self::ledger_failed(&error))?;
        let ids = match located {
            Located::Attached(sandbox) => {
                let reference = sandbox.reference();
                self.destroy(&reference)
                    .await
                    .map_err(|error| EnvError::backend(BACKEND, "prune", error.to_string()))?;
                vec![reference.id]
            }
            Located::Gone => Vec::new(),
        };
        let removed = self.finish_delete(lease, &record).await?;
        if ids.is_empty() && removed {
            // A process-durable sandbox ends with its owner, but the run's
            // designated workspace outlives it: removing that is the delete.
            return Ok(record
                .reference
                .as_ref()
                .map(|reference| reference.id.clone())
                .or(record.resource_id.clone())
                .and_then(|id| SandboxId::try_new(id).ok())
                .into_iter()
                .collect());
        }
        Ok(ids)
    }

    /// Reconcile the run's leases with their providers before any create:
    /// the takeover step of a `Write` open. First every admitted lease's
    /// namespace is checked against its entry's: the run's records name
    /// the namespace their resources live in, and an entry that now reports
    /// another one is the wrong place to adopt or remove anything, so a
    /// mismatch is reported and nothing below runs. Then each lease
    /// recorded `allocating` with a create attempted has its lost create
    /// adopted, when there is one (attached, fenced with one stop and one
    /// start, recorded live). Then every sandbox under the run's label that
    /// no record names is garbage (a stale owner's late create) and is
    /// removed. Invariant afterwards: a resource under the run's labels is
    /// named by a live or stopped record.
    pub async fn reconcile(&self, leases: &[RecordedLease]) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        // The entries the run's records name: only their inventory failures
        // matter to the sweep. `find` searches every entry in the catalog,
        // and one the run never used (Daytona with no credential, a Docker
        // daemon a host-only run never reached) cannot hold its sandboxes.
        let mut named = BTreeSet::new();
        for recorded in leases {
            match self.ledger.lookup(recorded.lease).await {
                Ok(Some(record)) => {
                    named.extend(record.provider.iter().map(ToString::to_string));
                    named.extend(record.admission.iter().map(|a| a.provider.to_string()));
                    named.extend(record.reference.iter().map(|r| r.provider.to_string()));
                    if let Some(admission) = &record.admission
                        && let Err(error) = self.check_namespace(admission).await
                    {
                        report.problems.push(error.to_string());
                    }
                }
                Ok(None) => {}
                Err(error) => report.problems.push(error.to_string()),
            }
        }
        if !report.problems.is_empty() {
            return report;
        }
        for recorded in leases {
            let slot = self.slot(recorded.lease);
            let mut slot = slot.lock().await;
            let record = match self.ledger.lookup(recorded.lease).await {
                Ok(Some(record)) => record,
                Ok(None) => continue,
                Err(error) => {
                    report.problems.push(error.to_string());
                    continue;
                }
            };
            if record.state != LeaseState::Allocating
                || !attempted(&record)
                || record.pending.is_some()
                || slot.live.is_some()
            {
                continue;
            }
            match self.locate(&record, recorded.lease).await {
                Ok(Located::Attached(sandbox)) => {
                    match self.recover(recorded.lease, &record, sandbox).await {
                        Ok(sandbox) => {
                            report
                                .adopted
                                .push((recorded.lease, sandbox.reference().id));
                            slot.live = Some(sandbox);
                        }
                        Err(error) => report.problems.push(error.to_string()),
                    }
                }
                Ok(Located::Gone) => {}
                Err(error) => report.problems.push(error.to_string()),
            }
        }
        report.merge(self.sweep_unrecorded(&named).await);
        report
    }

    /// Remove every sandbox under the run's label that no record names.
    /// Action hosts carry no lease label and are left to their own sweep.
    async fn sweep_unrecorded(&self, named: &BTreeSet<String>) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        let found = match catalog::find(
            &self.sandboxes,
            LabelFilter::eq(RUN_LABEL, self.identity.run_id()),
        )
        .await
        {
            Ok(found) => found,
            Err(error) => return report.with_problem(acquire_failed(&error).to_string()),
        };
        for failure in found
            .failures
            .iter()
            .filter(|failure| named.contains(failure.entry.as_str()))
        {
            report.problems.push(format!(
                "provider entry `{}` could not be searched for the run's sandboxes: {}",
                failure.entry, failure.error
            ));
        }
        for summary in found.sandboxes {
            let Some(lease) = summary
                .labels
                .get(LEASE_LABEL)
                .and_then(|raw| raw.parse::<u64>().ok())
                .map(SandboxLeaseId::new)
            else {
                continue;
            };
            let id = &summary.reference.id;
            let named = match self.ledger.lookup(lease).await {
                Ok(record) => record.is_some_and(|record| {
                    record.state != LeaseState::Deleted
                        && (record.resource_id.as_deref() == Some(id.as_str())
                            || record
                                .reference
                                .as_ref()
                                .is_some_and(|reference| reference.id == id.as_str()))
                }),
                Err(error) => {
                    report.problems.push(error.to_string());
                    continue;
                }
            };
            if named {
                continue;
            }
            tracing::warn!(lease = lease.raw(), sandbox = %id, "removing a sandbox no lease record names");
            match self.destroy(&summary.reference).await {
                Ok(()) => report.removed.push(id.clone()),
                Err(error) => report
                    .problems
                    .push(format!("sandbox {id} delete failed: {error}")),
            }
        }
        report
    }
}

impl ReconcileReport {
    fn with_problem(mut self, problem: String) -> Self {
        self.problems.push(problem);
        self
    }
}

/// Whether anything about the lease was ever asked of a provider.
fn attempted(record: &LeaseRecord) -> bool {
    record.admission.is_some() || record.fingerprint.is_some() || record.reference.is_some()
}

fn deleted_lease(lease: SandboxLeaseId) -> EnvError {
    EnvError::backend(
        BACKEND,
        "acquire",
        format!("sandbox lease {lease} was deleted; its workspace is gone"),
    )
}

/// The reference as Petri's records keep it.
pub(crate) fn to_reference(reference: &SandboxRef) -> SandboxReference {
    SandboxReference {
        provider:  SmolStr::new(reference.provider.as_str()),
        namespace: SmolStr::new(reference.namespace.as_str()),
        id:        SmolStr::new(reference.id.as_str()),
    }
}

/// A recorded reference, back as the driver's.
pub(crate) fn sandbox_ref(reference: &SandboxReference) -> Result<SandboxRef, EnvError> {
    let invalid = |error: &dyn fmt::Display| {
        EnvError::backend(
            BACKEND,
            "acquire",
            format!("the recorded sandbox reference is invalid: {error}"),
        )
    };
    Ok(SandboxRef {
        provider:  entry_name(&reference.provider)?,
        namespace: NamespaceId::try_new(reference.namespace.as_str()).map_err(|e| invalid(&e))?,
        id:        SandboxId::try_new(reference.id.as_str()).map_err(|e| invalid(&e))?,
    })
}

fn entry_name(name: &str) -> Result<ProviderName, EnvError> {
    ProviderName::try_new(name).map_err(|error| {
        EnvError::backend(
            BACKEND,
            "acquire",
            format!("invalid provider entry name `{name}`: {error}"),
        )
    })
}

/// Owns a holder until environment initialization hands it to an EnvHandle.
/// An unread create result and an interrupted environment read both drop
/// this guard. The manager owns any asynchronous cleanup that drop starts.
pub(crate) struct AcquiredSandbox {
    manager:    Arc<SandboxLeaseManager>,
    lease:      SandboxLeaseId,
    standalone: bool,
    sandbox:    Option<Arc<dyn Sandbox>>,
}

impl AcquiredSandbox {
    pub(crate) fn sandbox(&self) -> &Arc<dyn Sandbox> {
        self.sandbox
            .as_ref()
            .expect("an acquisition owns its sandbox")
    }

    pub(crate) fn into_sandbox(mut self) -> Arc<dyn Sandbox> {
        self.sandbox
            .take()
            .expect("an acquisition owns its sandbox")
    }

    pub(crate) async fn release(mut self) {
        self.manager.abandon(self.lease, self.standalone).await;
        self.sandbox = None;
    }
}

impl Drop for AcquiredSandbox {
    fn drop(&mut self) {
        if self.sandbox.is_some() {
            self.manager
                .abandon_in_background(self.lease, self.standalone);
        }
    }
}
