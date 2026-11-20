//! Interrupt ledger transitions and reopen the durable store over the same
//! Docker resources. No test reconstructs the expected record by hand.

use std::path::Path;
use std::sync::{Arc, Mutex};

use execution::{
    Access, InvocationId, OwnerId, ResourceLedger, ResourceStore, RunDirStore, RunKey, RunLogs,
    RunStore as _, SandboxAllocationKey, SandboxResourceRecord,
};
use executor::{AcquireContext, Executor, Retention, SandboxLeaseId, ScopeOutcome, ScopeSpec};
use executor_sandbox::{
    LeaseLedger, LeaseRecord, LeaseState, LedgerError, PendingIntent, RoutingExecutor,
    SandboxAdmission,
};
use ir::{RuntimeSpec, ScopeId};
use testkit::{RunDir, container_id, container_write, is_docker_ready};
use tokio::sync::Mutex as AsyncMutex;

const LEASE: SandboxLeaseId = SandboxLeaseId::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crash {
    Reserved,
    Created,
    StopIntent,
    Stopped,
    DeleteIntent,
    Deleted,
}

struct InterruptedLedger {
    inner: ResourceLedger,
    crash: Mutex<Option<Crash>>,
}

impl InterruptedLedger {
    fn interrupt(&self, at: Crash) -> Result<(), LedgerError> {
        let mut crash = self
            .crash
            .lock()
            .expect("the test crash control is not poisoned");
        if *crash == Some(at) {
            *crash = None;
            return Err(LedgerError(format!("simulated crash at {at:?}")));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl LeaseLedger for InterruptedLedger {
    async fn lookup(&self, lease: SandboxLeaseId) -> Result<Option<LeaseRecord>, LedgerError> {
        self.inner.lookup(lease).await
    }

    async fn allocating(
        &self,
        lease: SandboxLeaseId,
        admission: &SandboxAdmission,
    ) -> Result<(), LedgerError> {
        self.inner.allocating(lease, admission).await?;
        self.interrupt(Crash::Reserved)
    }

    async fn live(
        &self,
        lease: SandboxLeaseId,
        reference: &ir::SandboxReference,
    ) -> Result<(), LedgerError> {
        self.interrupt(Crash::Created)?;
        self.inner.live(lease, reference).await
    }

    async fn pending(
        &self,
        lease: SandboxLeaseId,
        intent: PendingIntent,
    ) -> Result<(), LedgerError> {
        self.inner.pending(lease, intent).await?;
        self.interrupt(match intent {
            PendingIntent::Stop => Crash::StopIntent,
            PendingIntent::Delete => Crash::DeleteIntent,
        })
    }

    async fn stopped(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.interrupt(Crash::Stopped)?;
        self.inner.stopped(lease).await
    }

    async fn deleted(&self, lease: SandboxLeaseId) -> Result<(), LedgerError> {
        self.interrupt(Crash::Deleted)?;
        self.inner.deleted(lease).await
    }
}

struct Fixture {
    directory: RunDir,
    scope:     ScopeSpec,
    /// The fixture's own writer handle: it holds the run's lease for the
    /// fixture's life, and every resource store view writes through it.
    logs:      Arc<dyn RunLogs>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = RunDir::new("sandbox-ledger-recovery");
        let scope = ScopeSpec::new(ScopeId::new(0), "scope-0")
            .with_runtime(RuntimeSpec::container("alpine:3.20"));
        let logs = RunDirStore::new(directory.path())
            .open(&RunKey::new(directory.run_id()), Access::Create {
                owner: OwnerId::mint(),
            })
            .await
            .expect("the run is created");
        ResourceStore::load(&logs)
            .await
            .expect("the test resource store opens")
            .ensure_record(
                SandboxAllocationKey {
                    invocation: InvocationId::ROOT,
                    scope:      engine::ScopeIdentity::Declared(scope.id),
                },
                "docker",
                scope.workspace_id.clone(),
                scope.runtime.clone(),
                None,
            )
            .await
            .expect("the initial lease is reserved");
        Self {
            directory,
            scope,
            logs,
        }
    }

    /// A fresh view of the resource log, over the fixture's own handle.
    async fn store(&self) -> ResourceStore {
        ResourceStore::load(&self.logs)
            .await
            .expect("the persisted resource store reopens")
    }

    async fn router(&self, retention: Retention, crash: Option<Crash>) -> RoutingExecutor {
        let ledger = ResourceLedger::new(Arc::new(AsyncMutex::new(self.store().await)));
        let router = RoutingExecutor::local(self.directory.path(), retention)
            .with_run_id(self.directory.run_id());
        router.set_ledger(Arc::new(InterruptedLedger {
            inner: ledger,
            crash: Mutex::new(crash),
        }));
        router
    }

    async fn record(&self) -> SandboxResourceRecord {
        self.store()
            .await
            .resolve(LEASE)
            .expect("the fixture lease is recorded")
            .clone()
    }

    async fn cleanup(&self, router: &RoutingExecutor) {
        router
            .delete_recorded(LEASE, self.scope.workspace_id.as_str())
            .await
            .expect("the test sandbox is deleted");
        assert_eq!(self.record().await.state, LeaseState::Deleted);
        assert_eq!(self.record().await.pending, None);
        assert!(
            container_id(&self.directory.sandbox_name(LEASE.raw()))
                .await
                .is_none()
        );
    }
}

#[tokio::test]
async fn allocation_crashes_recover_without_replacing_the_workspace() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::Reserved, Crash::Created] {
        let fixture = Fixture::new().await;
        let router = fixture.router(Retention::Never, Some(crash)).await;
        let ctx = AcquireContext::bare().with_lease(LEASE);
        assert!(router.acquire(&fixture.scope, &ctx).await.is_err());
        assert_eq!(fixture.record().await.state, LeaseState::Allocating);
        let name = fixture.directory.sandbox_name(LEASE.raw());
        let created = container_id(&name).await;
        if crash == Crash::Created {
            assert!(created.is_some());
            assert!(container_write(&name, "/workspace/before", "preserved").await);
        } else {
            assert!(created.is_none());
        }
        drop(router);

        let resumed = fixture.router(Retention::Never, None).await;
        let handle = resumed.acquire(&fixture.scope, &ctx).await.unwrap();
        if let Some(created) = created {
            assert_eq!(container_id(&name).await, Some(created));
            assert_eq!(
                handle
                    .exec()
                    .read_file(Path::new("before"))
                    .await
                    .unwrap()
                    .unwrap(),
                b"preserved"
            );
        }
        assert_eq!(fixture.record().await.state, LeaseState::Live);
        assert!(
            resumed
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        fixture.cleanup(&resumed).await;
    }
}

#[tokio::test]
async fn interrupted_stop_resumes_the_same_sandbox_and_workspace() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::StopIntent, Crash::Stopped] {
        let fixture = Fixture::new().await;
        let router = fixture.router(Retention::Always, Some(crash)).await;
        let ctx = AcquireContext::bare().with_lease(LEASE);
        let handle = router.acquire(&fixture.scope, &ctx).await.unwrap();
        handle
            .exec()
            .write_file(Path::new("before"), b"preserved")
            .await
            .unwrap();
        let created = fixture.record().await.resource_id;
        assert!(
            router
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert!(
            !router
                .release_lease(LEASE, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert_eq!(fixture.record().await.pending, Some(PendingIntent::Stop));
        drop(router);

        let resumed = fixture.router(Retention::Never, None).await;
        let handle = resumed.acquire(&fixture.scope, &ctx).await.unwrap();
        assert_eq!(fixture.record().await.resource_id, created);
        assert_eq!(fixture.record().await.pending, None);
        assert_eq!(
            handle
                .exec()
                .read_file(Path::new("before"))
                .await
                .unwrap()
                .unwrap(),
            b"preserved"
        );
        assert!(
            resumed
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        fixture.cleanup(&resumed).await;
    }
}

#[tokio::test]
async fn interrupted_delete_finishes_as_a_tombstone() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::DeleteIntent, Crash::Deleted] {
        let fixture = Fixture::new().await;
        let router = fixture.router(Retention::Never, Some(crash)).await;
        let ctx = AcquireContext::bare().with_lease(LEASE);
        let handle = router.acquire(&fixture.scope, &ctx).await.unwrap();
        assert!(
            router
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert!(
            !router
                .release_lease(LEASE, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert_eq!(fixture.record().await.pending, Some(PendingIntent::Delete));
        drop(router);

        let resumed = fixture.router(Retention::Never, None).await;
        assert!(resumed.acquire(&fixture.scope, &ctx).await.is_err());
        assert_eq!(
            fixture.record().await.state,
            LeaseState::Deleted,
            "{crash:?}"
        );
        assert_eq!(fixture.record().await.pending, None);
        fixture.cleanup(&resumed).await;
    }
}

#[tokio::test]
async fn recovery_refuses_a_changed_namespace_or_a_lost_workspace() {
    if !is_docker_ready().await {
        return;
    }
    let fixture = Fixture::new().await;
    let router = fixture.router(Retention::Never, None).await;
    let ctx = AcquireContext::bare().with_lease(LEASE);
    let handle = router.acquire(&fixture.scope, &ctx).await.unwrap();
    assert!(
        router
            .release(handle, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    let original = fixture.record().await;
    drop(router);
    fixture
        .store()
        .await
        .update(LEASE, |record| {
            let admission = record.admission.as_mut().expect("the lease was admitted");
            admission.namespace = "another-daemon".into();
        })
        .await
        .unwrap();
    let resumed = fixture.router(Retention::Never, None).await;
    let error = resumed.acquire(&fixture.scope, &ctx).await.unwrap_err();
    assert!(error.to_string().contains("another-daemon"), "{error}");
    drop(resumed);

    fixture
        .store()
        .await
        .update(LEASE, |record| {
            record.admission.clone_from(&original.admission);
        })
        .await
        .unwrap();
    let resumed = fixture.router(Retention::Never, None).await;
    fixture.cleanup(&resumed).await;
    drop(resumed);
    // A resource lost outside Petri leaves its last confirmed live record.
    fixture
        .store()
        .await
        .update(LEASE, |record| *record = original)
        .await
        .unwrap();
    let resumed = fixture.router(Retention::Never, None).await;
    let error = resumed.acquire(&fixture.scope, &ctx).await.unwrap_err();
    assert!(error.to_string().contains("workspace was lost"), "{error}");
    fixture.cleanup(&resumed).await;
}

#[tokio::test]
async fn release_reconciles_an_allocation_without_a_recorded_resource_id() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::Reserved, Crash::Created] {
        for retention in [Retention::Never, Retention::Always] {
            let fixture = Fixture::new().await;
            let router = fixture.router(retention, Some(crash)).await;
            let ctx = AcquireContext::bare().with_lease(LEASE);
            assert!(router.acquire(&fixture.scope, &ctx).await.is_err());
            assert!(fixture.record().await.resource_id.is_none());
            router.shutdown().await;
            drop(router);

            let resumed = fixture.router(retention, None).await;
            let report = resumed.release_lease(LEASE, ScopeOutcome::Failed).await;
            assert!(report.is_clean(), "{report:?}");
            let name = fixture.directory.sandbox_name(LEASE.raw());
            if crash == Crash::Created && retention == Retention::Always {
                assert_eq!(fixture.record().await.state, LeaseState::Stopped);
                assert!(container_id(&name).await.is_some());
                assert!(!testkit::container_is_running(&name).await);
            } else {
                assert_eq!(fixture.record().await.state, LeaseState::Deleted);
                assert!(container_id(&name).await.is_none());
            }
            fixture.cleanup(&resumed).await;
            resumed.shutdown().await;
        }
    }
}

#[tokio::test]
async fn release_finishes_a_pending_delete_even_when_retention_changes() {
    if !is_docker_ready().await {
        return;
    }
    for crash in [Crash::DeleteIntent, Crash::Deleted] {
        let fixture = Fixture::new().await;
        let router = fixture.router(Retention::Never, Some(crash)).await;
        let ctx = AcquireContext::bare().with_lease(LEASE);
        let handle = router.acquire(&fixture.scope, &ctx).await.unwrap();
        assert!(
            router
                .release(handle, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert!(
            !router
                .release_lease(LEASE, ScopeOutcome::Succeeded)
                .await
                .is_clean()
        );
        assert_eq!(fixture.record().await.pending, Some(PendingIntent::Delete));
        router.shutdown().await;
        drop(router);

        let resumed = fixture.router(Retention::Always, None).await;
        let report = resumed.release_lease(LEASE, ScopeOutcome::Failed).await;
        assert!(report.is_clean(), "{report:?}");
        fixture.cleanup(&resumed).await;
        resumed.shutdown().await;
    }
}
