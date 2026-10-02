//! The ledger-first rule and the takeover reconciliation, against a scripted
//! provider: no provider mutation starts before its intent is stored, a
//! late create by a stale owner never becomes live, and the next
//! reconciliation removes what no record names.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use executor::{Retention, SandboxLeaseId, ScopeOutcome};
use sandbox_driver::{
    Capabilities, Error, EventContext, Exec, Filesystem, Isolation, PlatformInfo, ProviderKind,
    ResourceKind, Sandbox, SandboxFilter, SandboxId, SandboxProvider, SandboxSpec, SandboxState,
    SandboxStatus,
};
use testkit::RunDir;
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};

use crate::lease::{LeaseRequest, RecordedLease};
use crate::{
    FixedProvider, LeaseLedger, LeaseRecord, LeaseState, LedgerError, LostSandbox, MemoryLedger,
    PendingIntent, RunIdentity, SandboxLeaseManager,
};

const WAIT: Duration = Duration::from_secs(5);
const LEASE: SandboxLeaseId = SandboxLeaseId::new(3);
const WORKSPACE: &str = "scope-0";

/// One sandbox the scripted provider holds, with the labels it was created
/// under and the calls it saw.
struct Box_ {
    id:     SandboxId,
    labels: BTreeMap<String, String>,
    stops:  AtomicUsize,
    starts: AtomicUsize,
}

#[async_trait]
impl Sandbox for Box_ {
    fn id(&self) -> &SandboxId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        static CAPABILITIES: OnceLock<Capabilities> = OnceLock::new();
        CAPABILITIES.get_or_init(|| Capabilities::minimal(Isolation::Container))
    }

    async fn describe(&self) -> sandbox_driver::Result<SandboxStatus> {
        let mut status = SandboxStatus::new(self.id.clone(), SandboxState::Running);
        status.labels.clone_from(&self.labels);
        Ok(status)
    }

    fn working_directory(&self) -> &str {
        crate::CONTAINER_WORKSPACE
    }

    async fn environment(&self) -> sandbox_driver::Result<BTreeMap<String, String>> {
        Ok(BTreeMap::new())
    }

    async fn platform_info(&self) -> sandbox_driver::Result<PlatformInfo> {
        panic!("reconciliation does not probe the platform")
    }

    async fn start(&self) -> sandbox_driver::Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&self) -> sandbox_driver::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn delete(&self) -> sandbox_driver::Result<()> {
        Ok(())
    }

    fn exec(&self) -> &dyn Exec {
        panic!("reconciliation does not execute a step")
    }

    fn fs(&self) -> &dyn Filesystem {
        panic!("reconciliation does not read workspace files")
    }
}

/// A provider that keeps every sandbox it created, by label, and can hold a
/// create until told to finish it.
struct ScriptedProvider {
    kind:           ProviderKind,
    boxes:          Mutex<BTreeMap<SandboxId, Arc<Box_>>>,
    next:           AtomicUsize,
    creates:        AtomicUsize,
    hold_create:    AtomicBool,
    create_started: Notify,
    allow_create:   Notify,
}

impl ScriptedProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            kind:           ProviderKind::try_new("scripted").expect("a valid kind"),
            boxes:          Mutex::new(BTreeMap::new()),
            next:           AtomicUsize::new(1),
            creates:        AtomicUsize::new(0),
            hold_create:    AtomicBool::new(false),
            create_started: Notify::new(),
            allow_create:   Notify::new(),
        })
    }

    fn boxes(&self) -> Vec<Arc<Box_>> {
        self.boxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    fn ids(&self) -> Vec<SandboxId> {
        self.boxes()
            .iter()
            .map(|sandbox| sandbox.id.clone())
            .collect()
    }
}

#[async_trait]
impl SandboxProvider for ScriptedProvider {
    fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    fn capabilities(&self) -> &Capabilities {
        static CAPABILITIES: OnceLock<Capabilities> = OnceLock::new();
        CAPABILITIES.get_or_init(|| Capabilities::minimal(Isolation::Container))
    }

    async fn create(
        &self,
        spec: &SandboxSpec,
        _events: Option<EventContext>,
    ) -> sandbox_driver::Result<Arc<dyn Sandbox>> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        if self.hold_create.load(Ordering::SeqCst) {
            self.create_started.notify_one();
            self.allow_create.notified().await;
        }
        let id = SandboxId::try_new(format!("box-{}", self.next.fetch_add(1, Ordering::SeqCst)))
            .expect("a valid id");
        let sandbox = Arc::new(Box_ {
            id:     id.clone(),
            labels: spec.labels.clone(),
            stops:  AtomicUsize::new(0),
            starts: AtomicUsize::new(0),
        });
        self.boxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, sandbox.clone());
        Ok(sandbox)
    }

    async fn attach(
        &self,
        id: &SandboxId,
        _events: Option<EventContext>,
    ) -> sandbox_driver::Result<Arc<dyn Sandbox>> {
        self.boxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .map(|sandbox| sandbox.clone() as Arc<dyn Sandbox>)
            .ok_or_else(|| Error::NotFound {
                resource: ResourceKind::Sandbox,
                id:       id.to_string(),
            })
    }

    async fn delete(
        &self,
        id: &SandboxId,
        _events: Option<EventContext>,
    ) -> sandbox_driver::Result<()> {
        self.boxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        Ok(())
    }

    async fn list(&self, filter: &SandboxFilter) -> sandbox_driver::Result<Vec<SandboxStatus>> {
        let mut statuses = Vec::new();
        for sandbox in self.boxes() {
            if filter
                .labels
                .iter()
                .all(|(key, value)| sandbox.labels.get(key) == Some(value))
            {
                statuses.push(sandbox.describe().await?);
            }
        }
        Ok(statuses)
    }
}

/// A ledger whose writes can be held or refused: a stalled store, or a
/// store that moved the lease to another owner.
struct ScriptedLedger {
    inner:          Arc<MemoryLedger>,
    stale:          AtomicBool,
    hold_writes:    AtomicBool,
    write_started:  Notify,
    allow_write:    Notify,
    refuse_pending: AtomicBool,
}

impl ScriptedLedger {
    fn new(inner: Arc<MemoryLedger>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            stale: AtomicBool::new(false),
            hold_writes: AtomicBool::new(false),
            write_started: Notify::new(),
            allow_write: Notify::new(),
            refuse_pending: AtomicBool::new(false),
        })
    }

    async fn gate(&self) -> Result<(), LedgerError> {
        if self.stale.load(Ordering::SeqCst) {
            return Err(LedgerError("the run's lease moved to another owner".into()));
        }
        if self.hold_writes.load(Ordering::SeqCst) {
            self.write_started.notify_one();
            self.allow_write.notified().await;
        }
        Ok(())
    }
}

#[async_trait]
impl LeaseLedger for ScriptedLedger {
    async fn lookup(&self, lease: SandboxLeaseId) -> Result<Option<LeaseRecord>, LedgerError> {
        self.inner.lookup(lease).await
    }

    async fn allocating(
        &self,
        lease: SandboxLeaseId,
        provider: &str,
        fingerprint: &str,
    ) -> Result<(), LedgerError> {
        self.gate().await?;
        self.inner.allocating(lease, provider, fingerprint).await
    }

    async fn live(&self, lease: SandboxLeaseId, resource_id: &str) -> Result<(), LedgerError> {
        self.gate().await?;
        self.inner.live(lease, resource_id).await
    }

    async fn pending(
        &self,
        lease: SandboxLeaseId,
        intent: PendingIntent,
    ) -> Result<(), LedgerError> {
        if self.refuse_pending.load(Ordering::SeqCst) {
            return Err(LedgerError("the intent could not be stored".into()));
        }
        self.gate().await?;
        self.inner.pending(lease, intent).await
    }

    async fn stopped(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.gate().await?;
        self.inner.stopped(lease).await
    }

    async fn deleted(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.gate().await?;
        self.inner.deleted(lease).await
    }
}

fn build_manager(
    dir: &RunDir,
    provider: &Arc<ScriptedProvider>,
    ledger: Arc<dyn LeaseLedger>,
) -> Arc<SandboxLeaseManager> {
    Arc::new(SandboxLeaseManager::new(
        Arc::new(FixedProvider::new(provider.clone())),
        ledger,
        Arc::new(RunIdentity::new(dir.path().to_path_buf(), dir.run_id())),
    ))
}

async fn acquire(manager: &Arc<SandboxLeaseManager>) -> Result<SandboxId, String> {
    let request = LeaseRequest {
        lease:        LEASE,
        workspace_id: WORKSPACE,
        standalone:   false,
    };
    manager
        .acquire(request, |labels, _provider| async move {
            let mut spec = SandboxSpec::new(sandbox_driver::SandboxSource::Image {
                reference: "test".into(),
            });
            spec.labels.extend(labels);
            Ok(spec)
        })
        .await
        .map(|acquired| acquired.into_sandbox().id().clone())
        .map_err(|error| error.to_string())
}

/// The provider is not asked to create while the `allocating` write is
/// still in flight, and never when the write fails.
#[tokio::test]
async fn no_create_starts_before_its_allocating_record_is_stored() {
    let dir = RunDir::new("reconcile-ledger-first");
    let provider = ScriptedProvider::new();
    let ledger = ScriptedLedger::new(Arc::new(MemoryLedger::default()));
    ledger.hold_writes.store(true, Ordering::SeqCst);
    let manager = build_manager(&dir, &provider, ledger.clone());

    let acquiring = {
        let manager = manager.clone();
        tokio::spawn(async move { acquire(&manager).await })
    };
    timeout(WAIT, ledger.write_started.notified())
        .await
        .expect("the allocating write starts");
    sleep(Duration::from_millis(50)).await;
    assert_eq!(
        provider.creates.load(Ordering::SeqCst),
        0,
        "no create while the intent is not stored"
    );
    ledger.hold_writes.store(false, Ordering::SeqCst);
    ledger.allow_write.notify_one();
    let created = timeout(WAIT, acquiring)
        .await
        .expect("the acquire settles")
        .expect("the task joins")
        .expect("the create succeeds once the intent is stored");
    assert_eq!(provider.creates.load(Ordering::SeqCst), 1);
    assert_eq!(provider.ids(), vec![created]);

    // A refused write: the provider is never asked.
    let dir = RunDir::new("reconcile-ledger-refused");
    let provider = ScriptedProvider::new();
    let ledger = ScriptedLedger::new(Arc::new(MemoryLedger::default()));
    ledger.stale.store(true, Ordering::SeqCst);
    let manager = build_manager(&dir, &provider, ledger);
    let error = acquire(&manager).await.expect_err("the intent is refused");
    assert!(error.contains("lease moved"), "{error}");
    assert_eq!(provider.creates.load(Ordering::SeqCst), 0);
}

/// Neither a stop nor a delete reaches the provider when its pending
/// intent cannot be stored.
#[tokio::test]
async fn no_stop_or_delete_starts_before_its_intent_is_stored() {
    let dir = RunDir::new("reconcile-intent-first");
    let provider = ScriptedProvider::new();
    let ledger = ScriptedLedger::new(Arc::new(MemoryLedger::default()));
    let manager = build_manager(&dir, &provider, ledger.clone());
    let id = acquire(&manager).await.expect("creates");
    let sandbox = provider
        .boxes()
        .into_iter()
        .find(|sandbox| sandbox.id == id)
        .expect("the created sandbox");

    ledger.refuse_pending.store(true, Ordering::SeqCst);
    for retention in [Retention::Always, Retention::Never] {
        let report = manager
            .release_lease(LEASE, retention, ScopeOutcome::Succeeded)
            .await;
        assert!(!report.is_clean(), "{report:?}");
        assert_eq!(sandbox.stops.load(Ordering::SeqCst), 0, "no stop");
        assert_eq!(provider.ids(), vec![id.clone()], "no delete");
        let record = ledger
            .lookup(LEASE)
            .await
            .expect("reads")
            .expect("recorded");
        assert_eq!(record.state, LeaseState::Live);
        assert_eq!(record.pending, None);
    }
}

/// A create the old owner started completes after the lease moved: the
/// resource never becomes live (the old owner's `live` write is refused),
/// the new owner's own sandbox stays, and the next reconciliation removes
/// the late one.
#[tokio::test]
async fn a_late_create_by_a_stale_owner_is_never_live_and_is_removed_by_reconciliation() {
    let dir = RunDir::new("reconcile-takeover");
    let provider = ScriptedProvider::new();
    let records = Arc::new(MemoryLedger::default());
    let old_ledger = ScriptedLedger::new(records.clone());
    let old = build_manager(&dir, &provider, old_ledger.clone());

    // The old owner reserves the lease and its create hangs at the provider.
    provider.hold_create.store(true, Ordering::SeqCst);
    let old_acquire = {
        let old = old.clone();
        tokio::spawn(async move { acquire(&old).await })
    };
    timeout(WAIT, provider.create_started.notified())
        .await
        .expect("the old owner's create starts");
    provider.hold_create.store(false, Ordering::SeqCst);
    let record = records
        .lookup(LEASE)
        .await
        .expect("reads")
        .expect("recorded");
    assert_eq!(record.state, LeaseState::Allocating);
    assert!(record.fingerprint.is_some(), "a create was attempted");

    // The lease moves: the old owner's later writes are refused.
    old_ledger.stale.store(true, Ordering::SeqCst);
    let new = build_manager(&dir, &provider, records.clone());
    let recorded = [RecordedLease {
        lease:        LEASE,
        workspace_id: WORKSPACE.to_owned(),
    }];
    let report = new.reconcile(&recorded).await;
    assert!(report.is_clean(), "{report:?}");
    assert!(report.adopted.is_empty(), "nothing exists yet to adopt");
    assert!(report.removed.is_empty());

    // The new owner creates its own sandbox and records it live.
    let mine = acquire(&new).await.expect("the new owner creates");
    let record = records
        .lookup(LEASE)
        .await
        .expect("reads")
        .expect("recorded");
    assert_eq!(record.state, LeaseState::Live);
    assert_eq!(record.resource_id.as_deref(), Some(mine.as_str()));

    // The old create completes: a second resource under the same labels,
    // whose `live` write is refused.
    provider.allow_create.notify_one();
    let error = timeout(WAIT, old_acquire)
        .await
        .expect("the old acquire settles")
        .expect("the task joins")
        .expect_err("the stale owner cannot record its resource live");
    assert!(error.contains("lease moved"), "{error}");
    assert_eq!(provider.ids().len(), 2, "the late resource exists");
    let record = records
        .lookup(LEASE)
        .await
        .expect("reads")
        .expect("recorded");
    assert_eq!(
        record.resource_id.as_deref(),
        Some(mine.as_str()),
        "the record still names the new owner's sandbox"
    );

    // The next reconciliation removes what no record names.
    let report = new.reconcile(&recorded).await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.removed.len(), 1);
    assert_ne!(report.removed[0], mine);
    assert_eq!(provider.ids(), vec![mine.clone()]);
    assert!(new.live(LEASE).await.is_some());
}

/// A create that reached the provider but not the ledger is adopted at
/// reconciliation: attached, fenced with one stop and one start, and
/// recorded live, so the workspace it holds is never replaced.
#[tokio::test]
async fn a_lost_create_is_adopted_and_fenced_at_reconciliation() {
    let dir = RunDir::new("reconcile-adopt");
    let provider = ScriptedProvider::new();
    let records = Arc::new(MemoryLedger::default());
    let crashed_ledger = ScriptedLedger::new(records.clone());
    let crashed = build_manager(&dir, &provider, crashed_ledger.clone());
    provider.hold_create.store(true, Ordering::SeqCst);
    let crashed_acquire = {
        let crashed = crashed.clone();
        tokio::spawn(async move { acquire(&crashed).await })
    };
    timeout(WAIT, provider.create_started.notified())
        .await
        .expect("the create starts");
    provider.hold_create.store(false, Ordering::SeqCst);
    // The crash: the `live` write never lands, but the resource exists.
    crashed_ledger.stale.store(true, Ordering::SeqCst);
    provider.allow_create.notify_one();
    let _ = timeout(WAIT, crashed_acquire).await.expect("settles");
    let lost = provider.ids();
    assert_eq!(lost.len(), 1);
    let record = records
        .lookup(LEASE)
        .await
        .expect("reads")
        .expect("recorded");
    assert_eq!(record.state, LeaseState::Allocating);

    let resumed = build_manager(&dir, &provider, records.clone());
    let report = resumed
        .reconcile(&[RecordedLease {
            lease:        LEASE,
            workspace_id: WORKSPACE.to_owned(),
        }])
        .await;
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.adopted, vec![(LEASE, lost[0].clone())]);
    assert!(report.removed.is_empty());
    let sandbox = provider.boxes().remove(0);
    assert_eq!(sandbox.stops.load(Ordering::SeqCst), 1, "fenced once");
    assert_eq!(sandbox.starts.load(Ordering::SeqCst), 1, "started once");
    let record = records
        .lookup(LEASE)
        .await
        .expect("reads")
        .expect("recorded");
    assert_eq!(record.state, LeaseState::Live);
    assert_eq!(record.resource_id.as_deref(), Some(lost[0].as_str()));
    // The adopted sandbox serves the next acquire without a create.
    let again = acquire(&resumed)
        .await
        .expect("acquires the adopted sandbox");
    assert_eq!(again, lost[0]);
    assert_eq!(provider.creates.load(Ordering::SeqCst), 1);
}

/// A release that keeps its sandbox, finding it gone from the provider,
/// ends the lease as a tombstone and says what was lost: there is nothing
/// left to keep, and no retry could keep it.
#[tokio::test]
async fn a_kept_release_of_a_lost_sandbox_tombstones_its_lease() {
    let dir = RunDir::new("reconcile-lost-kept");
    let provider = ScriptedProvider::new();
    let ledger: Arc<MemoryLedger> = Arc::new(MemoryLedger::default());
    let manager = build_manager(&dir, &provider, ledger.clone());
    let id = acquire(&manager).await.expect("creates");
    manager.release_holder(LEASE).await;
    drop(manager);
    provider
        .delete(&id, None)
        .await
        .expect("the provider forgets the sandbox");

    // A resumed run's manager finds the sandbox by its record.
    let manager = build_manager(&dir, &provider, ledger.clone());
    let report = manager
        .release_lease(LEASE, Retention::Always, ScopeOutcome::Failed)
        .await;
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("was gone before it could be kept")),
        "{report:?}"
    );
    let record = ledger
        .lookup(LEASE)
        .await
        .expect("the ledger reads")
        .expect("the lease is recorded");
    assert_eq!(record.state, LeaseState::Deleted);
    assert_eq!(record.pending, None);
    let again = manager
        .release_lease(LEASE, Retention::Always, ScopeOutcome::Failed)
        .await;
    assert!(again.is_clean(), "a tombstone needs no release: {again:?}");
}

/// A sandbox whose create never reached its record: the release finds it by
/// its label, and a lease that keeps it names it, so prune and the next
/// release find it by the record.
#[tokio::test]
async fn a_kept_release_names_a_sandbox_its_create_never_recorded() {
    let dir = RunDir::new("reconcile-unnamed-kept");
    let provider = ScriptedProvider::new();
    let created: Arc<MemoryLedger> = Arc::new(MemoryLedger::default());
    let manager = build_manager(&dir, &provider, created.clone());
    let id = acquire(&manager).await.expect("creates");
    manager.release_holder(LEASE).await;
    drop(manager);
    let allocated = created
        .lookup(LEASE)
        .await
        .expect("the ledger reads")
        .expect("the lease is recorded");

    // The resumed run's ledger: the allocation, and no sandbox named.
    let ledger: Arc<MemoryLedger> = Arc::new(MemoryLedger::default());
    ledger
        .allocating(
            LEASE,
            allocated.provider.as_deref().expect("a provider"),
            allocated.fingerprint.as_deref().expect("a fingerprint"),
        )
        .await
        .expect("records the allocation");
    let manager = build_manager(&dir, &provider, ledger.clone());
    let report = manager
        .release_lease(LEASE, Retention::Always, ScopeOutcome::Failed)
        .await;
    assert!(report.is_clean(), "{report:?}");
    let record = ledger
        .lookup(LEASE)
        .await
        .expect("the ledger reads")
        .expect("the lease is recorded");
    assert_eq!(record.state, LeaseState::Stopped);
    assert_eq!(record.resource_id.as_deref(), Some(id.as_str()));
}

/// A recorded sandbox that is gone from the provider fails the acquire by
/// default; under `LostSandbox::Replace` a fresh one is created under the
/// lease and recorded live in its place.
#[tokio::test]
async fn a_lost_sandbox_is_refused_by_default_and_replaced_on_request() {
    let dir = RunDir::new("reconcile-lost-sandbox");
    let provider = ScriptedProvider::new();
    let ledger: Arc<MemoryLedger> = Arc::new(MemoryLedger::default());
    let manager = build_manager(&dir, &provider, ledger.clone());
    let first = acquire(&manager).await.expect("the first acquire creates");
    manager.release_holder(LEASE).await;
    drop(manager);

    // The sandbox disappears behind Petri's back: a prune, a daemon reset.
    provider
        .delete(&first, None)
        .await
        .expect("the provider forgets the sandbox");
    assert!(provider.ids().is_empty());

    let refusing = build_manager(&dir, &provider, ledger.clone());
    let error = acquire(&refusing)
        .await
        .expect_err("a lost workspace is not replaced silently");
    assert!(error.contains("will not replace it silently"), "{error}");
    assert_eq!(provider.creates.load(Ordering::SeqCst), 1);
    drop(refusing);

    let replacing = Arc::new(
        SandboxLeaseManager::new(
            Arc::new(FixedProvider::new(provider.clone())),
            ledger.clone(),
            Arc::new(RunIdentity::new(dir.path().to_path_buf(), dir.run_id())),
        )
        .with_lost_sandbox(LostSandbox::Replace),
    );
    let second = acquire(&replacing)
        .await
        .expect("a fresh sandbox is created");
    assert_ne!(second, first);
    assert_eq!(provider.creates.load(Ordering::SeqCst), 2);
    assert_eq!(provider.ids(), vec![second.clone()]);
    let record = ledger
        .lookup(LEASE)
        .await
        .expect("the ledger reads")
        .expect("the lease is recorded");
    assert_eq!(record.state, LeaseState::Live);
    assert_eq!(record.resource_id.as_deref(), Some(second.as_str()));
}
