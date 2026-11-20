//! An [`Executor`] over one run's sandbox catalog: each scope goes to the
//! catalog environment its runtime names, through one [`SandboxExecutor`],
//! with recovery for action hosts left by earlier runs. Host container actions
//! are refused until sandbox-driver supports portable host-workspace mounts.
//! It is the composition the runtime registers for a run.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, SandboxLeaseId,
    ScopeOutcome, ScopeSpec, WorkspaceId,
};
use sandbox_driver::{SandboxId, Sandboxes, Source};

use crate::actions::{ActionHost, ActionHostRunner, remove_recorded};
use crate::catalog::Credentials;
use crate::gate::{DRAIN_BUDGET, RunGate};
use crate::lease::{LeaseLedger, MemoryLedger, ReconcileReport, RecordedLease};
use crate::run::RunIdentity;
use crate::simulated::SIMULATED_KIND;
use crate::{
    BACKEND, SandboxCatalog, SandboxExecutor, SandboxOptions, SandboxTeardown, acquire_failed,
};

/// The environment container scopes go to when the options name no other.
pub const CONTAINER_KIND: &str = "docker";

/// What ends a host scope's action host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ActionOwner {
    Lease(SandboxLeaseId),
    Scope(ir::ScopeId),
}

/// Routes scopes to the environments of one sandbox catalog.
///
/// Every scope goes through one [`SandboxExecutor`] over the run's catalog;
/// which provider an environment reaches is the catalog's. The catalog is
/// opened on the first scope that needs it, after the coordinator sets the
/// ledger, and a provider is activated on the first sandbox it serves, so a
/// host-only run never touches a daemon. A lease recorded earlier goes back
/// to the entry its reference names, whatever environment the run is
/// configured with now.
///
/// A [`RoutingExecutor::simulated`] router is a dry run's: every scope,
/// whatever its target and whatever backend the options name, is acquired
/// on the simulated provider. A lease another provider holds (a real run
/// resumed as a dry run) is left where it is and reported, never touched.
pub struct RoutingExecutor {
    catalog:      Result<SandboxCatalog, String>,
    /// The run's open catalog and executor, built on first use. A
    /// configuration error is remembered: every scope then fails routably
    /// with it.
    open:         OnceLock<Result<Arc<SandboxExecutor>, String>>,
    /// The action hosts of host scopes, each keyed by what ends it: the
    /// lease that owns it, or the scope itself when no coordinator named one.
    action_hosts: Mutex<BTreeMap<(ActionOwner, WorkspaceId), Arc<ActionHost>>>,
    ledger:       OnceLock<Arc<dyn LeaseLedger>>,
    identity:     Arc<RunIdentity>,
    retention:    Retention,
    options:      SandboxOptions,
    /// Every scope goes to the simulated provider: a dry run.
    simulated:    bool,
    /// Admission to every environment this router hands out; closed at
    /// shutdown, so a retained environment cannot outlive the run.
    gate:         RunGate,
}

impl RoutingExecutor {
    /// The catalog the default options describe: Host for process scopes,
    /// Docker for container scopes. The standard runtime's default.
    pub fn local(run_dir: impl Into<PathBuf>, retention: Retention) -> Self {
        Self::with_options(run_dir, retention, SandboxOptions::default())
    }

    /// The catalog `options` name: its catalog file, or the built-in
    /// environments its backend describes.
    pub fn with_options(
        run_dir: impl Into<PathBuf>,
        retention: Retention,
        options: SandboxOptions,
    ) -> Self {
        let catalog = match &options.catalog {
            Some(path) => SandboxCatalog::from_file(path),
            None => SandboxCatalog::from_options(&options),
        };
        Self::over(catalog, run_dir, retention, options)
    }

    /// A router over an application's own catalog: its entries,
    /// environments, providers, and credential store.
    pub fn with_catalog(
        catalog: SandboxCatalog,
        run_dir: impl Into<PathBuf>,
        retention: Retention,
        options: SandboxOptions,
    ) -> Self {
        Self::over(Ok(catalog), run_dir, retention, options)
    }

    /// A dry run's router: every scope is acquired on the simulated
    /// provider. The sandbox options a run would place scopes with do not
    /// apply, and neither does its retention: there is nothing to place and
    /// nothing to keep, so every lease is recorded deleted when it is
    /// released.
    pub fn simulated(run_dir: impl Into<PathBuf>) -> Self {
        let mut router = Self::over(
            Ok(SandboxCatalog::simulated()),
            run_dir,
            Retention::Never,
            SandboxOptions::default(),
        );
        router.simulated = true;
        router
    }

    fn over(
        catalog: Result<SandboxCatalog, String>,
        run_dir: impl Into<PathBuf>,
        retention: Retention,
        options: SandboxOptions,
    ) -> Self {
        Self {
            catalog,
            open: OnceLock::new(),
            action_hosts: Mutex::new(BTreeMap::new()),
            ledger: OnceLock::new(),
            identity: Arc::new(RunIdentity::for_run_dir(run_dir.into())),
            retention,
            options,
            simulated: false,
            gate: RunGate::default(),
        }
    }

    /// Name the run on its providers: the run's store key, so a resume
    /// reaches the crashed run's sandboxes. A router built without one is
    /// named after its run directory ([`RunIdentity::for_run_dir`]).
    #[must_use]
    pub fn with_run_id(mut self, run_id: impl Into<smol_str::SmolStr>) -> Self {
        self.identity = Arc::new(RunIdentity::new(
            self.identity.run_dir().to_path_buf(),
            run_id,
        ));
        self
    }

    /// The durable ledger leases are recorded in. A coordinator sets it
    /// before the first scope; without one, leases live in memory and
    /// sandboxes end with their scopes.
    pub fn set_ledger(&self, ledger: Arc<dyn LeaseLedger>) {
        let _ = self.ledger.set(ledger);
    }

    pub fn identity(&self) -> &Arc<RunIdentity> {
        &self.identity
    }

    /// Close the run's access to its sandboxes, then shut down the
    /// providers this router activated, after its run's admitted operations
    /// settle. Environments handed out earlier refuse new work from here
    /// on. Durable sandboxes and retained workspaces remain.
    pub async fn shutdown(&self) {
        if !self.gate.close(DRAIN_BUDGET).await {
            tracing::warn!(
                budget_secs = DRAIN_BUDGET.as_secs(),
                "sandbox operations were still running when the run closed its sandboxes"
            );
        }
        let Some(Ok(executor)) = self.open.get() else {
            return;
        };
        if let Err(error) = executor.manager().sandboxes().clone().shutdown().await {
            tracing::warn!(error = %error, "shutting down the run's sandbox providers failed");
        }
    }

    /// The environment a scope's lease is reserved on, before resource
    /// creation. The lease's record names the entry once it is admitted.
    pub fn provider_kind_for(&self, runtime: &ir::RuntimeSpec) -> &str {
        if self.simulated {
            SIMULATED_KIND
        } else {
            self.options.environment_for(runtime)
        }
    }

    /// The run's executor over its open catalog, built on first use.
    fn executor(&self) -> Result<&Arc<SandboxExecutor>, EnvError> {
        self.open
            .get_or_init(|| {
                let catalog = self.catalog.clone()?;
                let credentials = Arc::new(Credentials::default());
                let sandboxes = catalog
                    .open_with(credentials.clone())
                    .map_err(|error| error.to_string())?;
                let ledger =
                    self.ledger.get().cloned().unwrap_or_else(|| {
                        Arc::new(MemoryLedger::default()) as Arc<dyn LeaseLedger>
                    });
                let executor = SandboxExecutor::new(
                    sandboxes,
                    credentials,
                    ledger,
                    Arc::clone(&self.identity),
                    self.retention,
                    self.options.clone(),
                )
                .with_host_addresses(catalog.host_addresses().clone())
                .with_gate(self.gate.clone());
                Ok(Arc::new(if self.simulated {
                    executor.simulated()
                } else {
                    executor
                }))
            })
            .as_ref()
            .map_err(|message| EnvError::backend(BACKEND, "configure", message.clone()))
    }

    fn sandboxes(&self) -> Result<&Sandboxes, EnvError> {
        Ok(self.executor()?.manager().sandboxes())
    }

    /// Whether `lease` was simulated by a dry run, by its record.
    async fn simulated_lease(&self, lease: SandboxLeaseId) -> Result<bool, EnvError> {
        let Some(ledger) = self.ledger.get() else {
            return Ok(false);
        };
        let record = ledger
            .lookup(lease)
            .await
            .map_err(|e| EnvError::backend(BACKEND, "lookup", e.to_string()))?;
        Ok(record.is_some_and(|record| record.provider.as_deref() == Some(SIMULATED_KIND)))
    }

    /// The name prefix every sandbox this run owns starts with,
    /// `petri-<run id>-`, for a leak check after release.
    pub fn container_prefix(&self) -> String {
        self.identity.container_prefix()
    }

    /// Recovery state for action hosts created by an earlier version.
    fn action_host(
        &self,
        owner: ActionOwner,
        scope: &ScopeSpec,
    ) -> Result<Arc<ActionHost>, EnvError> {
        let sandboxes = self.sandboxes()?;
        let key = (owner, scope.workspace_id.clone());
        let mut hosts = self
            .action_hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(hosts
            .entry(key)
            .or_insert_with(|| {
                Arc::new(ActionHost::new(
                    sandboxes.clone(),
                    self.identity.clone(),
                    scope.workspace_id.as_str().to_owned(),
                ))
            })
            .clone())
    }

    /// Keep failed hosts registered so a later release can retry cleanup.
    async fn release_action_hosts(&self, owner: ActionOwner) -> ReleaseReport {
        let hosts: Vec<_> = self
            .action_hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|((candidate, _), _)| *candidate == owner)
            .map(|(key, host)| (key.clone(), host.clone()))
            .collect();
        let mut report = ReleaseReport::default();
        for (key, host) in hosts {
            match host.teardown().await {
                Ok(removed) => {
                    if removed {
                        report = report.released("action host");
                    }
                    self.action_hosts
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&key);
                }
                Err(error) => {
                    report = report.problem(format!("action host teardown failed: {error}"));
                }
            }
        }
        report
    }

    /// Ends a lease: stops its sandbox, then keeps or deletes it by this
    /// router's retention for `outcome`. The coordinator calls this when
    /// the invocation that owns the lease finishes.
    pub async fn release_lease(
        &self,
        lease: SandboxLeaseId,
        outcome: ScopeOutcome,
    ) -> ReleaseReport {
        let mut report = self.release_action_hosts(ActionOwner::Lease(lease)).await;
        if !report.is_clean() {
            // The action host can still have this workspace mounted. Keep its
            // owning lease intact so release or prune can retry safely.
            return report;
        }
        match self.leased(lease).await {
            Ok(executor) => {
                let ended = executor
                    .manager()
                    .release_lease(lease, self.retention, outcome)
                    .await;
                report.merge(ended);
            }
            Err(error) => {
                report = report.problem(format!("no executor to release lease {lease}: {error}"));
            }
        }
        report
    }

    /// The executor for a recorded lease; refused for a dry run's lease
    /// under a real router, since no provider holds it.
    async fn leased(&self, lease: SandboxLeaseId) -> Result<&Arc<SandboxExecutor>, EnvError> {
        if !self.simulated && self.simulated_lease(lease).await? {
            return Err(EnvError::backend(
                SIMULATED_KIND,
                "lookup",
                format!("lease {lease} was simulated by a dry run; no provider holds it"),
            ));
        }
        self.executor()
    }

    /// Reconcile the run's recorded leases with their providers before any
    /// create: the takeover step a coordinator runs when it opens a stored
    /// run for writing. Each lease goes to the entry its record names.
    pub async fn reconcile(&self, leases: &[RecordedLease]) -> ReconcileReport {
        match self.executor() {
            Ok(executor) => executor.manager().reconcile(leases).await,
            Err(error) => ReconcileReport {
                problems: vec![format!("no executor to reconcile with: {error}")],
                ..ReconcileReport::default()
            },
        }
    }

    /// Deletes a recorded lease's sandbox with no live handle: prune.
    /// `workspace_id` is the lease's workspace, whose action host, if a
    /// marker records one, goes first.
    pub async fn delete_recorded(
        &self,
        lease: SandboxLeaseId,
        workspace_id: &str,
    ) -> Result<Vec<SandboxId>, EnvError> {
        if !self.simulated && self.simulated_lease(lease).await? {
            // A dry run's lease: nothing was ever on a provider.
            return Ok(Vec::new());
        }
        let executor = self.executor()?;
        // An action host has its own marker and sandbox. Remove it before
        // the lease's own sandbox and workspace go, even if an earlier
        // cleanup left a tombstone for the lease itself. A workspace that
        // never ran an action has no marker, and nothing is contacted.
        let mut deleted = remove_recorded(
            executor.manager().sandboxes(),
            &self.identity,
            workspace_id,
            None,
        )
        .await?;
        deleted.extend(executor.manager().delete_recorded(lease).await?);
        Ok(deleted)
    }
}

#[async_trait]
impl Executor for RoutingExecutor {
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        let executor = self.executor()?;
        if self.simulated {
            return executor.acquire(scope, ctx).await;
        }
        // Host scopes retain action-host recovery but refuse new container
        // actions. Other scopes run actions on their own sandbox.
        let environment = executor.environment_for(&scope.runtime);
        let template = executor
            .manager()
            .sandboxes()
            .environment(environment)
            .await
            .map_err(|error| acquire_failed(&error))?;
        if template.spec.source != Source::HostDirectory {
            return executor.acquire(scope, ctx).await;
        }
        let owner = ctx
            .lease()
            .map_or(ActionOwner::Scope(scope.id), ActionOwner::Lease);
        let action_host = self.action_host(owner, scope);
        if let Ok(host) = &action_host {
            host.prepare().await;
        }
        let handle = executor.acquire(scope, ctx).await?;
        Ok(handle.with_runner(Arc::new(ActionHostRunner {
            gate: self.gate.clone(),
        })))
    }

    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let Some(teardown) = env.teardown::<SandboxTeardown>() else {
            return ReleaseReport::default()
                .problem("no backend of this router acquired this environment");
        };
        let lease = teardown.lease;
        let Some(Ok(executor)) = self.open.get() else {
            return ReleaseReport::default()
                .problem("the environment has no executor to release it");
        };
        let ended = self
            .release_action_hosts(ActionOwner::Scope(env.scope()))
            .await;
        if !ended.is_clean() {
            // End the holder but retain its workspace while an action host
            // can still mount it. A later acquire/release can retry cleanup.
            executor.manager().release_holder(lease).await;
            return ended;
        }
        let mut report = executor.release(env, outcome).await;
        report.merge(ended);
        report
    }
}
