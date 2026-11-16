//! Petri executors over the shared sandbox API of sandbox-driver.
//!
//! [`SandboxExecutor`] places each scope on a catalog environment and keeps
//! one durable lease per workspace on the sandbox it gets. Providers own
//! process execution, files, one-shot containers, and cleanup; Petri names
//! environments and stores references, and never builds a provider's recipe.
//! [`RoutingExecutor`] adds what a process scope on a host directory needs
//! beside its sandbox (an action host for Docker actions) and is what the
//! runtime registers. [`HostExecutor`] is a convenience wrapper for standalone
//! Host execution.
//!
//! Host, Docker and Daytona are built-in providers linked into this crate and
//! registered with [`sandbox_driver::Registry::builtin`]
//! ([`SandboxCatalog::builtin_registry`]); none is launched as a plugin. A
//! dry run acquires every scope on the simulated provider, which creates
//! nothing and runs nothing.

mod actions;
mod backend;
mod catalog;
mod env;
mod gate;
mod host;
pub mod lease;
mod routing;
mod run;
mod simulated;

use std::collections::BTreeMap;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, SandboxLeaseId,
    ScopeOutcome, ScopeSpec,
};
use ir::{RuntimeTarget, SandboxInstance};
use sandbox_driver::{
    ContainerSpec, Descriptor, Error as DriverError, Isolation, NetworkPolicy, ProviderName,
    ReadinessCheck, Sandbox, SandboxSpec, Sandboxes, ServiceSpec, Source, WorkspaceOwnership,
};
use smol_str::SmolStr;

pub use crate::backend::{
    DaytonaResources, DaytonaSandboxKind, LostSandbox, SandboxBackend, SandboxOptions,
};
use crate::catalog::Credentials;
pub use crate::catalog::{SIMULATED_ENVIRONMENT, SandboxCatalog};
use crate::env::{OneShotRunner, SandboxEnv};
use crate::gate::RunGate;
pub use crate::host::HostExecutor;
pub use crate::lease::{
    LeaseLedger, LeaseRecord, LeaseState, LedgerError, MemoryLedger, PendingIntent,
    ReconcileReport, RecordedLease, SandboxAdmission, SandboxLeaseManager,
};
use crate::lease::{LeaseRequest, Prepared, to_reference};
pub use crate::routing::{CONTAINER_KIND, RoutingExecutor};
use crate::run::workspace_dir;
pub use crate::run::{LEASE_LABEL, RUN_LABEL, RunIdentity, WORKSPACE_LABEL};
pub use crate::simulated::{SIMULATED_KIND, SimulatedFactory};

/// The container path every container scope's workspace lives at.
pub const CONTAINER_WORKSPACE: &str = "/workspace";
/// The alias a container on a local daemon reaches the driver's machine
/// through; the container maps it to the daemon's gateway.
const DOCKER_HOST_ALIAS: &str = "host.docker.internal";

/// The backend name this crate's errors carry.
const BACKEND: &str = "sandbox";

/// A service readiness check states its whole budget; a workflow's health
/// check states only some of it. These fill the rest, within the five
/// minutes Docker waits for a service at most.
const READY_DEADLINE_MS: u64 = 300_000;
const READY_INTERVAL_MS: u64 = 1_000;
const READY_TIMEOUT_MS: u64 = 30_000;
const READY_RETRIES: u32 = 30;

pub(crate) fn acquire_failed(error: &DriverError) -> EnvError {
    if let DriverError::FenceLeaked { message } = error {
        return EnvError::FenceLeaked {
            detail: message.clone(),
        };
    }
    EnvError::backend(BACKEND, "acquire", error.to_string())
}

/// What Petri did before the shared sandbox API and that API cannot express
/// yet. Every caller is a parity gap tracked in the sandbox refactor's parity
/// plan (https://ampcode.com/threads/T-7b304bcd-d83a-42ed-818f-ffc6c291814c);
/// none is a decision to drop the feature.
// TODO(parity): remove each caller once sandbox-driver's shared API carries
// the feature it names.
pub(crate) fn parity_gap(feature: &str) -> EnvError {
    EnvError::backend(
        BACKEND,
        "acquire",
        format!(
            "{feature} is not yet carried by sandbox-driver's shared sandbox API; Petri supports \
             it again once the API addition lands"
        ),
    )
}

/// An [`Executor`] that realizes scopes on the run's sandbox catalog. A
/// sandbox owns its workspace; every acquisition names the lease it lives
/// under, and one live handle serves every holder of that lease.
pub struct SandboxExecutor {
    manager:        Arc<SandboxLeaseManager>,
    identity:       Arc<RunIdentity>,
    retention:      Retention,
    options:        SandboxOptions,
    credentials:    Arc<Credentials>,
    /// Every scope is realized on the simulated environment, whatever its
    /// target: a bare spec, no host-process semantics, no route back.
    simulated:      bool,
    /// Admission to the environments this executor hands out.
    gate:           RunGate,
    /// Per environment, the configured route back to this machine.
    host_addresses: BTreeMap<String, Option<String>>,
}

impl SandboxExecutor {
    /// Builds an executor over the run's catalog, with `ledger` as the
    /// durable record of its leases; `identity` names the run.
    pub(crate) fn new(
        sandboxes: Sandboxes,
        credentials: Arc<Credentials>,
        ledger: Arc<dyn LeaseLedger>,
        identity: Arc<RunIdentity>,
        retention: Retention,
        options: SandboxOptions,
    ) -> Self {
        Self {
            manager: Arc::new(
                SandboxLeaseManager::new(sandboxes, credentials.clone(), ledger, identity.clone())
                    .with_lost_sandbox(options.lost_sandbox),
            ),
            identity,
            retention,
            options,
            credentials,
            simulated: false,
            gate: RunGate::default(),
            host_addresses: BTreeMap::new(),
        }
    }

    /// The configured route back to this machine, per environment, in
    /// place of the provider's reported locality.
    #[must_use]
    pub(crate) fn with_host_addresses(
        mut self,
        host_addresses: BTreeMap<String, Option<String>>,
    ) -> Self {
        self.host_addresses = host_addresses;
        self
    }

    /// Admits this executor's environments through the router's gate, so
    /// closing the run closes them.
    #[must_use]
    pub(crate) fn with_gate(mut self, gate: RunGate) -> Self {
        self.gate = gate;
        self
    }

    /// Places every scope on the simulated environment: a dry run. Nor is
    /// there a workspace to keep, so a standalone lease ends deleted.
    #[must_use]
    pub(crate) fn simulated(mut self) -> Self {
        self.simulated = true;
        self.retention = Retention::Never;
        self
    }

    pub fn manager(&self) -> &Arc<SandboxLeaseManager> {
        &self.manager
    }

    /// The name prefix every sandbox this run owns starts with,
    /// `petri-<run id>-`, for a leak check after release.
    pub fn container_prefix(&self) -> String {
        self.identity.container_prefix()
    }

    /// The catalog environment a scope with this runtime is placed on.
    pub(crate) fn environment_for(&self, runtime: &ir::RuntimeSpec) -> &str {
        if self.simulated {
            SIMULATED_ENVIRONMENT
        } else {
            self.options.environment_for(runtime)
        }
    }

    async fn descriptor(&self, entry: &str) -> Result<Descriptor, EnvError> {
        self.manager
            .sandboxes()
            .provider(entry)
            .map_err(|error| acquire_failed(&error))?
            .describe()
            .await
            .map_err(|error| acquire_failed(&error))
    }

    async fn acquire_inner(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        // A coordinator names the lease; a bare driver has none, and its
        // sandbox is the scope's alone, keyed by the scope and ended with it.
        let (lease, standalone) = match ctx.lease() {
            Some(lease) => (lease, false),
            None => (SandboxLeaseId::new(u64::from(scope.id.raw())), true),
        };
        let environment = self.environment_for(&scope.runtime);
        let acquired = self
            .manager
            .acquire(
                LeaseRequest {
                    lease,
                    workspace_id: scope.workspace_id.as_str(),
                    standalone,
                },
                |labels| self.prepare(scope, environment, labels, lease, ctx),
            )
            .await?;
        let sandbox = acquired.sandbox();
        let reference = sandbox.reference();
        let descriptor = match self.descriptor(reference.provider.as_str()).await {
            Ok(descriptor) => descriptor,
            Err(error) => {
                acquired.release().await;
                return Err(error);
            }
        };
        // A retained sandbox may predate the requested block policy. Check
        // provider state before handing it to hooks or executing any stage.
        if !self.simulated && self.options.network == NetworkPolicy::Block {
            let verified = match sandbox.status().await {
                Ok(status) if status.network == Some(NetworkPolicy::Block) => Ok(()),
                Ok(_) => Err(EnvError::backend(
                    BACKEND,
                    "acquire",
                    "the sandbox does not report blocked networking; refusing to execute a \
                     blocked run",
                )),
                Err(error) => Err(acquire_failed(&error)),
            };
            if let Err(error) = verified {
                acquired.release().await;
                return Err(error);
            }
        }

        let ambient = sandbox.env().clone();
        let mut env_overrides = scope.env.clone();
        if let RuntimeTarget::Container { options, .. } = &scope.runtime.target {
            env_overrides.extend(options.env.clone());
        }
        let workspace = sandbox.working_directory().to_string_lossy().into_owned();
        let instance = self.describe(&**sandbox, &workspace).await;
        let locality = &descriptor.locality;
        // A process on this machine, in a directory of this machine: the
        // process step reads and writes it as one.
        let host = !self.simulated
            && descriptor.isolation == Isolation::None
            && locality.mounts_host_paths;
        // The configured route back for this environment, else what the
        // provider reports: loopback for Host, the host alias on a local
        // Docker daemon.
        let host_address = match self.host_addresses.get(environment) {
            Some(configured) => configured.clone(),
            None => locality.host_address.clone().or_else(|| {
                (locality.mounts_host_paths && descriptor.isolation != Isolation::None)
                    .then(|| DOCKER_HOST_ALIAS.to_owned())
            }),
        };
        let env = SandboxEnv {
            host,
            sandbox: sandbox.clone(),
            workspace: workspace.clone(),
            ambient,
            env: env_overrides,
            grace: scope.grace,
            host_address: host_address.clone(),
            gate: self.gate.clone(),
        };
        // Docker actions in this scope run as one-shot containers in the
        // sandbox's world: same workspace, same services, same host alias.
        let runner = OneShotRunner {
            sandbox: sandbox.clone(),
            workspace,
            host_address,
            env: scope.env.clone(),
            gate: self.gate.clone(),
        };
        let teardown = SandboxTeardown {
            sandbox: acquired.into_sandbox(),
            lease,
            standalone,
        };
        Ok(EnvHandle::new(
            scope.id,
            SmolStr::new(scope.environment.as_str()),
            instance,
            Arc::new(env),
            teardown,
        )
        .with_runner(Arc::new(runner)))
    }

    /// The record of an acquired sandbox: its reference, and what the
    /// provider reports it runs. A provider that cannot report its status
    /// still has the sandbox: the image and snapshot are then unknown.
    async fn describe(&self, sandbox: &dyn Sandbox, workspace: &str) -> SandboxInstance {
        let reference = sandbox.reference();
        let (image, snapshot) = match sandbox.status().await {
            Ok(status) => (status.image, status.snapshot.map(|id| id.to_string())),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    sandbox = %reference.id,
                    "the acquired sandbox reported no status; its image is unrecorded"
                );
                (None, None)
            }
        };
        SandboxInstance {
            provider:          SmolStr::new(reference.provider.as_str()),
            instance:          SmolStr::new(reference.id.as_str()),
            image:             image.map(SmolStr::new),
            snapshot:          snapshot.map(SmolStr::new),
            working_directory: SmolStr::new(workspace),
            reference:         Some(to_reference(&reference)),
        }
    }

    /// The scope's sandbox over its environment's template: the run's
    /// name and labels, the scope's env, its workload, and its services.
    async fn prepare(
        &self,
        scope: &ScopeSpec,
        environment: &str,
        labels: Vec<(String, String)>,
        lease: SandboxLeaseId,
        ctx: &AcquireContext,
    ) -> Result<Prepared, EnvError> {
        let sandboxes = self.manager.sandboxes();
        let template = sandboxes
            .environment(environment)
            .await
            .map_err(|error| acquire_failed(&error))?;
        let name = self.identity.container_name(lease);
        if self.simulated {
            // A name, a working directory string and the run's labels:
            // nothing the scope declares (its image, services, env or
            // credentials) reaches a provider, because none is involved.
            let mut spec = SandboxSpec::new(Source::HostDirectory);
            spec.name = Some(name);
            spec.working_directory = Some(format!("/{SIMULATED_KIND}/{}", scope.workspace_id));
            spec.labels.extend(labels);
            return self
                .prepared(environment, template.provider, spec, Vec::new())
                .await;
        }
        let descriptor = self.descriptor(template.provider.as_str()).await?;
        let mut spec = template.spec;
        spec.name = Some(name);
        spec.labels.extend(labels);
        spec.env.extend(
            scope
                .env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
        if self.options.network != NetworkPolicy::ProviderDefault {
            spec.network = self.options.network.clone();
        }
        // An image sandbox's workspace is `/workspace` unless the
        // environment names its own.
        if spec.source != Source::HostDirectory && spec.working_directory.is_none() {
            spec.working_directory = Some(CONTAINER_WORKSPACE.to_owned());
        }
        // On a local daemon a container reaches this machine through the
        // host alias, as a process on it would through loopback.
        let host_alias =
            descriptor.locality.mounts_host_paths && descriptor.isolation != Isolation::None;
        match (&spec.source, &scope.runtime.target) {
            (Source::HostDirectory, RuntimeTarget::HostProcess) => {
                if !scope.services.is_empty() {
                    return Err(EnvError::backend(
                        BACKEND,
                        "acquire",
                        format!(
                            "services require a containerized job; environment `{environment}` \
                             runs processes in a host directory"
                        ),
                    ));
                }
                // The run's own directory: it outlives the sandbox, so a
                // kept workspace survives the provider, and Petri removes it
                // when the lease is deleted.
                let workspace = workspace_dir(self.identity.run_dir(), scope.workspace_id.as_str());
                spec.working_directory = Some(workspace.to_string_lossy().into_owned());
                spec.workspace_ownership = Some(WorkspaceOwnership::Designated);
            }
            (Source::HostDirectory, RuntimeTarget::Container { .. }) => {
                return Err(EnvError::backend(
                    BACKEND,
                    "acquire",
                    format!(
                        "environment `{environment}` runs processes in a host directory; a \
                         container scope needs an image environment"
                    ),
                ));
            }
            (_, RuntimeTarget::HostProcess) => {
                if let Some(image) = self.options.runner_image(&scope.runtime)? {
                    spec.source = Source::Image(image);
                }
                if host_alias {
                    let container = spec.container.get_or_insert_with(ContainerSpec::default);
                    container.init = Some(true);
                    container
                        .extra_hosts
                        .insert(DOCKER_HOST_ALIAS.to_owned(), "host-gateway".to_owned());
                }
            }
            (
                _,
                RuntimeTarget::Container {
                    image,
                    options,
                    credentials,
                },
            ) => {
                if credentials.is_some() {
                    // TODO(parity): workload image registry credentials.
                    return Err(parity_gap("a registry credential for the job image"));
                }
                spec.source = Source::Image(image.to_string());
                if let Some(user) = &options.user {
                    spec.user = Some(user.to_string());
                }
                spec.env.extend(
                    options
                        .env
                        .iter()
                        .map(|(key, value)| (key.to_string(), value.to_string())),
                );
                // Steps run as a program plus arguments, so an image without
                // bash works; an init process reaps what they leave.
                let container = spec.container.get_or_insert_with(ContainerSpec::default);
                container.init = Some(true);
                if options.privileged {
                    container.privileged = Some(true);
                }
                container
                    .capabilities_add
                    .extend(options.cap_add.iter().map(ToString::to_string));
                container
                    .dns
                    .extend(options.dns.iter().map(ToString::to_string));
                if host_alias {
                    container
                        .extra_hosts
                        .insert(DOCKER_HOST_ALIAS.to_owned(), "host-gateway".to_owned());
                }
                if let Some(platform) = &options.platform {
                    // Docker's own per-sandbox option; a provider whose
                    // spec_schema lacks it refuses it at admission.
                    // TODO(parity): a portable platform field, if the parity
                    // plan adds one.
                    spec.provider_config.insert(
                        "platform".to_owned(),
                        serde_json::Value::String(platform.to_string()),
                    );
                }
            }
        }
        let credentials = self.services(scope, lease, ctx, &mut spec)?;
        self.prepared(environment, template.provider, spec, credentials)
            .await
    }

    async fn prepared(
        &self,
        environment: &str,
        provider: ProviderName,
        spec: SandboxSpec,
        credentials: Vec<String>,
    ) -> Result<Prepared, EnvError> {
        let builder = self
            .manager
            .sandboxes()
            .builder(environment)
            .await
            .map_err(|error| acquire_failed(&error))?
            .spec(spec);
        Ok(Prepared {
            environment: environment.to_owned(),
            provider,
            builder,
            credentials,
        })
    }

    /// Maps the scope's services onto the spec. A registry credential is
    /// resolved here, from the run's secrets, and handed to the driver
    /// under a name only this create uses; the names come back so they are
    /// dropped once the create settles.
    fn services(
        &self,
        scope: &ScopeSpec,
        lease: SandboxLeaseId,
        ctx: &AcquireContext,
        spec: &mut SandboxSpec,
    ) -> Result<Vec<String>, EnvError> {
        let mut credentials = Vec::new();
        for service in &scope.services {
            let typed = &service.options;
            // TODO(parity): service user, entrypoint, dns, cap_add and
            // privileged.
            for (set, option) in [
                (typed.user.is_some(), "user"),
                (typed.entrypoint.is_some(), "entrypoint"),
                (!typed.dns.is_empty(), "dns"),
                (!typed.cap_add.is_empty(), "cap_add"),
                (typed.privileged, "privileged"),
            ] {
                if set {
                    return Err(parity_gap(&format!(
                        "the service option `{option}` (service `{}`)",
                        service.name
                    )));
                }
            }
            let mut mapped = ServiceSpec::new(service.image.as_str());
            // Declared env first, then `-e` flags on top, as `docker create`
            // would apply them.
            mapped.env = service
                .env
                .iter()
                .chain(typed.env.iter().map(|(key, value)| (key, value)))
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>();
            if let Some(registry) = &service.credentials {
                let password = resolve_secret(&registry.password_secret, ctx)?;
                let name = format!(
                    "petri-{}-l{}-{}",
                    self.identity.run_id(),
                    lease.raw(),
                    service.name
                );
                let value = serde_json::json!({
                    "username": registry.username.as_str(),
                    "password": password,
                });
                self.credentials.insert(name.clone(), value.to_string());
                mapped.credential = Some(name.clone());
                credentials.push(name);
            }
            if let Some(cmd) = typed.health.as_ref().and_then(|health| health.cmd.as_ref()) {
                let health = typed.health.as_ref().expect("checked above");
                let positive = |value: Option<u64>, default: u64| {
                    value.filter(|value| *value > 0).unwrap_or(default)
                };
                let deadline = READY_DEADLINE_MS;
                let interval = positive(health.interval_ms, READY_INTERVAL_MS).min(deadline - 1);
                let timeout = positive(health.timeout_ms, READY_TIMEOUT_MS).min(deadline);
                let retries = health
                    .retries
                    .filter(|retries| *retries > 0)
                    .map_or(READY_RETRIES, |retries| {
                        u32::try_from(retries).unwrap_or(u32::MAX)
                    });
                // A health check is a shell command, as Docker's CMD-SHELL
                // runs it.
                mapped.ready = Some(ReadinessCheck {
                    command:     vec!["sh".to_owned(), "-c".to_owned(), cmd.to_string()],
                    interval_ms: nonzero(interval),
                    timeout_ms:  nonzero(timeout),
                    deadline_ms: nonzero(deadline),
                    retries:     NonZeroU32::new(retries).expect("positive"),
                });
            }
            spec.services.insert(service.name.to_string(), mapped);
        }
        Ok(credentials)
    }
}

fn nonzero(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value.max(1)).expect("positive")
}

/// Resolves a secret by name inside acquire, registering it for masking.
fn resolve_secret(name: &str, ctx: &AcquireContext) -> Result<String, EnvError> {
    ctx.secrets()
        .resolve(name)
        .map(|secret| secret.expose().to_string())
        .map_err(|error| {
            EnvError::backend(
                BACKEND,
                "acquire",
                format!("resolving secret `{name}`: {error}"),
            )
        })
}

/// What release needs: the lease the environment held, and whether this
/// executor alone owns it.
pub(crate) struct SandboxTeardown {
    sandbox:    Arc<dyn Sandbox>,
    lease:      SandboxLeaseId,
    /// No coordinator named the lease: the sandbox ends with the scope.
    standalone: bool,
}

impl fmt::Debug for SandboxTeardown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxTeardown")
            .field("sandbox", &self.sandbox.reference())
            .field("lease", &self.lease)
            .field("standalone", &self.standalone)
            .finish()
    }
}

#[async_trait]
impl Executor for SandboxExecutor {
    #[tracing::instrument(name = "scope.acquire", skip_all, fields(scope = scope.id.raw()))]
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        match self.acquire_inner(scope, ctx).await {
            Ok(handle) => {
                tracing::info!("scope environment acquired");
                Ok(handle)
            }
            Err(error) => {
                tracing::error!(error = ?error, "scope environment acquire failed");
                Err(error)
            }
        }
    }

    /// Drops this execution's holder. A standalone acquisition — no
    /// coordinator, no lease — ends its sandbox here by retention; a
    /// managed one leaves that to the lease's release.
    #[tracing::instrument(
        name = "scope.release",
        skip_all,
        fields(scope = env.scope().raw(), outcome = ?outcome)
    )]
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let Some(teardown) = env.teardown::<SandboxTeardown>() else {
            return ReleaseReport::default()
                .problem("sandbox executor was handed a foreign environment");
        };
        let (lease, standalone) = (teardown.lease, teardown.standalone);
        drop(env);
        let remaining = self.manager.release_holder(lease).await;
        let mut report = ReleaseReport::default().released(format!("holder of lease {lease}"));
        if !standalone || remaining > 0 {
            return report;
        }
        let ended = self
            .manager
            .release_lease(lease, self.retention, outcome)
            .await;
        report.merge(ended);
        report
    }
}
