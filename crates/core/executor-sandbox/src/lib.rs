//! Petri executors over the sandbox-driver provider family.
//!
//! [`SandboxExecutor`] maps scopes and durable leases to Host, Docker, or
//! Daytona sandboxes over the JSON-RPC plugin protocol. Providers own process
//! execution, files, one-shot containers, and cleanup. [`RoutingExecutor`]
//! selects the provider from runtime options and the scope's target.
//! [`HostExecutor`] is a convenience wrapper for standalone Host execution.
//!
//! No provider crate is linked here: every provider is a plugin process
//! ([`plugin`]), which is the path a third-party provider must take, so
//! Petri's own take it too.

#[cfg(test)]
mod acquire_tests;
mod actions;
mod backend;
#[cfg(test)]
mod daytona_tests;
mod env;
mod host;
pub mod lease;
pub mod plugin;
mod routing;
mod run;
mod snapshots;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, Retention, SandboxLeaseId,
    ScopeOutcome, ScopeSpec,
};
use ir::{ContainerOptions, RuntimeTarget};
use sandbox_driver::{Sandbox, SandboxProvider, SandboxSource, SandboxSpec, WorkspaceOwnership};
use sandbox_driver_daytona_config::{
    DaytonaProviderConfig, DockerExecutionTarget, NestedDockerConfig,
};
use sandbox_driver_docker_config::{DockerProviderConfig, Health, RegistryAuth, Sidecar};
use smol_str::SmolStr;

pub use crate::backend::{DaytonaResources, DaytonaSandboxKind, SandboxBackend, SandboxOptions};
use crate::env::{OneShotRunner, SandboxEnv};
pub use crate::host::HostExecutor;
pub use crate::lease::{
    LeaseLedger, LeaseRecord, LeaseState, LedgerError, MemoryLedger, PendingIntent,
    SandboxLeaseManager,
};
pub use crate::plugin::{FixedProvider, PluginError, PluginSettings, PluginSource, ProviderSource};
pub use crate::routing::{CONTAINER_KIND, RoutingExecutor};
use crate::run::workspace_dir;
pub use crate::run::{LEASE_LABEL, RUN_ID_FILE, RUN_LABEL, RunIdentity, WORKSPACE_LABEL};
use crate::snapshots::{RunnerSnapshot, RunnerSnapshots};

/// The container path every scope's workspace lives at.
pub const CONTAINER_WORKSPACE: &str = "/workspace";
/// The alias a container on a local daemon reaches the driver's machine
/// through; the provider config maps it to the daemon's gateway.
const DOCKER_HOST_ALIAS: &str = "host.docker.internal";

/// The backend name this crate's errors carry.
const BACKEND: &str = "sandbox";

pub(crate) fn acquire_failed(error: &sandbox_driver::Error) -> EnvError {
    if let sandbox_driver::Error::Provider(error) = error
        && error.provider.as_str() == "host"
        && error.code.as_deref() == Some("fence_leaked")
    {
        return EnvError::FenceLeaked {
            detail: error.message.clone(),
        };
    }
    EnvError::backend(BACKEND, "acquire", error.to_string())
}

/// An [`Executor`] that realizes container scopes on one sandbox-driver
/// provider. The sandbox owns its workspace; every acquisition names the
/// lease it lives under, and one live handle serves every holder of that
/// lease.
pub struct SandboxExecutor {
    manager:      Arc<SandboxLeaseManager>,
    identity:     Arc<RunIdentity>,
    retention:    Retention,
    host_address: Option<String>,
    options:      SandboxOptions,
    snapshots:    RunnerSnapshots,
}

impl SandboxExecutor {
    /// Builds an executor over `source`, with `ledger` as the durable
    /// record of its leases; `identity` names the run.
    pub fn new(
        source: Arc<dyn ProviderSource>,
        ledger: Arc<dyn LeaseLedger>,
        identity: Arc<RunIdentity>,
        retention: Retention,
        host_address: Option<String>,
        options: SandboxOptions,
    ) -> Self {
        Self {
            manager: Arc::new(SandboxLeaseManager::new(source, ledger, identity.clone())),
            identity,
            retention,
            host_address,
            options,
            snapshots: RunnerSnapshots::default(),
        }
    }

    pub fn manager(&self) -> &Arc<SandboxLeaseManager> {
        &self.manager
    }

    /// The name prefix every sandbox this run owns starts with,
    /// `petri-<run id>-`, for a leak check after release.
    pub async fn container_prefix(&self) -> Result<String, EnvError> {
        self.identity.container_prefix().await
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
        let name = self.identity.container_name(lease).await?;
        let acquired = self
            .manager
            .acquire(
                lease::LeaseRequest {
                    lease,
                    workspace_id: scope.workspace_id.as_str(),
                    standalone,
                },
                |labels, provider| async move {
                    self.build_spec(scope, &labels, &name, ctx, &*provider)
                        .await
                },
            )
            .await?;
        let sandbox = acquired.sandbox();

        // The ambient environment is a fact the steps rely on (`PATH` for
        // the process step, say); a provider that cannot report it is not
        // one this executor can serve, and says so at acquire.
        let ambient = match sandbox.environment().await {
            Ok(ambient) => ambient,
            Err(error) => {
                acquired.release().await;
                return Err(EnvError::backend(
                    BACKEND,
                    "acquire",
                    format!("the sandbox's environment could not be read: {error}"),
                ));
            }
        };
        let mut env_overrides = scope.env.clone();
        if let RuntimeTarget::Container { options, .. } = &scope.runtime.target {
            env_overrides.extend(options.env.clone());
        }
        let workspace = sandbox.working_directory().to_owned();
        let host = self.options.backend == SandboxBackend::Host
            && matches!(scope.runtime.target, RuntimeTarget::HostProcess);
        let env = SandboxEnv {
            host,
            sandbox: sandbox.clone(),
            workspace: workspace.clone(),
            ambient,
            env: env_overrides,
            grace: scope.grace,
            host_address: self.host_address.clone(),
        };
        // Docker actions in this scope run as one-shot containers in the
        // sandbox's world: same workspace, same services, same host alias.
        let runner = OneShotRunner {
            sandbox: sandbox.clone(),
            workspace,
            host_address: self.host_address.clone(),
            env: scope.env.clone(),
        };
        let teardown = SandboxTeardown {
            host,
            sandbox: acquired.into_sandbox(),
            lease,
            standalone,
        };
        Ok(EnvHandle::new(
            scope.id,
            SmolStr::new(scope.environment.as_str()),
            Arc::new(env),
            teardown,
        )
        .with_runner(Arc::new(runner)))
    }

    async fn build_spec(
        &self,
        scope: &ScopeSpec,
        labels: &[(String, String)],
        name: &str,
        ctx: &AcquireContext,
        provider: &dyn SandboxProvider,
    ) -> Result<SandboxSpec, EnvError> {
        if self.options.backend == SandboxBackend::Host
            && matches!(scope.runtime.target, RuntimeTarget::HostProcess)
        {
            if !scope.services.is_empty() {
                return Err(EnvError::backend(
                    "host",
                    "acquire",
                    "services require a containerized job; set container: or select the docker backend",
                ));
            }
            let workspace = workspace_dir(self.identity.run_dir(), scope.workspace_id.as_str());
            let mut spec = SandboxSpec::new(SandboxSource::HostDirectory)
                .name(name)
                .working_directory(workspace.to_string_lossy());
            spec.workspace_ownership = Some(WorkspaceOwnership::Managed);
            spec.labels.extend(labels.iter().cloned());
            spec.env.extend(
                scope
                    .env
                    .iter()
                    .map(|(key, value)| (key.to_string(), value.to_string())),
            );
            return Ok(spec);
        }
        if self.options.backend != SandboxBackend::Daytona {
            if self.options.backend == SandboxBackend::Docker
                && matches!(scope.runtime.target, RuntimeTarget::HostProcess)
            {
                let mut runner = scope.clone();
                runner.runtime.target = RuntimeTarget::Container {
                    image:       self.options.runner_image(&scope.runtime)?.into(),
                    options:     ContainerOptions::default(),
                    credentials: None,
                };
                return build_spec(&runner, labels, name, ctx);
            }
            return build_spec(scope, labels, name, ctx);
        }
        let image = self.options.runner_image(&scope.runtime)?;
        let snapshot = RunnerSnapshot::new(
            &image,
            self.options.daytona_resources.validated()?,
            self.options.daytona_kind.sandbox_kind(),
            self.manager.source().region(),
        );
        let mut spec = build_daytona_spec(scope, ctx, &snapshot)?;
        spec.name = Some(name.to_owned());
        spec.labels.extend(labels.iter().cloned());
        let snapshots = provider.snapshots().ok_or_else(|| {
            EnvError::backend(
                "daytona",
                "snapshot",
                "the provider does not support runner snapshots",
            )
        })?;
        self.snapshots.ensure(snapshots, &snapshot).await?;
        Ok(spec)
    }
}

fn build_daytona_spec(
    scope: &ScopeSpec,
    ctx: &AcquireContext,
    snapshot: &RunnerSnapshot,
) -> Result<SandboxSpec, EnvError> {
    let (target, image, user, options) = match &scope.runtime.target {
        RuntimeTarget::HostProcess => {
            if !scope.services.is_empty() {
                return Err(EnvError::backend(
                    "daytona",
                    "acquire",
                    "services require a container job",
                ));
            }
            (
                DockerExecutionTarget::VirtualMachine,
                String::new(),
                None,
                DockerProviderConfig::default(),
            )
        }
        RuntimeTarget::Container {
            image,
            options,
            credentials,
        } => {
            let mut config = docker_provider_config(
                registry_auth(credentials.as_ref(), ctx)?,
                sidecars(scope, ctx)?,
                options,
            );
            // A Daytona job cannot use the local Docker daemon's host alias.
            config.extra_hosts.clear();
            (
                DockerExecutionTarget::Container,
                image.to_string(),
                options.user.as_ref().map(ToString::to_string),
                config,
            )
        }
    };
    let mut spec = snapshot
        .sandbox_spec()
        .working_directory(CONTAINER_WORKSPACE);
    spec.user = Some("root".to_owned());
    spec.public = Some(false);
    spec.timers.auto_stop_after_idle = Some(Duration::ZERO);
    spec.timers.auto_pause_after_idle = Some(Duration::ZERO);
    spec.timers.auto_delete_after_stop = Some(Duration::ZERO);
    spec.timers.ttl = Some(Duration::ZERO);
    spec.env.extend(
        scope
            .env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    if let RuntimeTarget::Container { options, .. } = &scope.runtime.target {
        spec.env.extend(
            options
                .env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        );
    }
    spec.provider_config = DaytonaProviderConfig {
        docker: Some(NestedDockerConfig {
            image,
            target,
            user,
            options,
        }),
        ..Default::default()
    }
    .into_value();
    Ok(spec)
}

/// Maps a container scope to a `SandboxSpec`: the workspace inside the
/// sandbox, the run's labels, and the provider's typed options.
fn build_spec(
    scope: &ScopeSpec,
    labels: &[(String, String)],
    name: &str,
    ctx: &AcquireContext,
) -> Result<SandboxSpec, EnvError> {
    let RuntimeTarget::Container {
        image,
        options,
        credentials,
    } = &scope.runtime.target
    else {
        return Err(EnvError::backend(
            BACKEND,
            "acquire",
            "this executor realizes container scopes only; a host-process scope needs the host \
             executor or a runner image",
        ));
    };
    let registry_auth = registry_auth(credentials.as_ref(), ctx)?;
    let sidecars = sidecars(scope, ctx)?;
    let provider_config = docker_provider_config(registry_auth, sidecars, options);
    let mut spec = SandboxSpec::new(SandboxSource::Image {
        reference: image.to_string(),
    })
    .working_directory(CONTAINER_WORKSPACE)
    .provider_config(provider_config.into_value())
    .name(name);
    for (key, value) in labels {
        spec = spec.label(key.clone(), value.clone());
    }
    spec.user = options.user.as_ref().map(ToString::to_string);

    // Scope env is the trusted channel for the container; a `-e` option lands
    // on top, as the later `docker create` flag would have.
    for (key, value) in &scope.env {
        spec = spec.env_var(key.as_str(), value.as_str());
    }
    for (key, value) in &options.env {
        spec = spec.env_var(key.as_str(), value.as_str());
    }
    Ok(spec)
}

/// Resolves image pull credentials to registry auth, or `None` when the
/// scope declares none.
fn registry_auth(
    credentials: Option<&ir::RegistryCredentials>,
    ctx: &AcquireContext,
) -> Result<Option<RegistryAuth>, EnvError> {
    let Some(credentials) = credentials else {
        return Ok(None);
    };
    let password = resolve_secret(&credentials.password_secret, ctx)?;
    Ok(Some(RegistryAuth {
        username: credentials.username.to_string(),
        password,
        server: None,
    }))
}

/// Maps the scope's services to Docker sidecars. Ports are dropped: a
/// containerized job reaches a service by its network alias, not a published
/// port.
fn sidecars(scope: &ScopeSpec, ctx: &AcquireContext) -> Result<Vec<Sidecar>, EnvError> {
    let mut sidecars = Vec::with_capacity(scope.services.len());
    for service in &scope.services {
        let typed = &service.options;
        let mut sidecar = Sidecar::new(service.name.as_str(), service.image.as_str());
        // Declared env first, then `-e` flags on top, as `docker create`
        // would apply them.
        sidecar.env = service
            .env
            .iter()
            .chain(typed.env.iter().map(|(key, value)| (key, value)))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        sidecar.dns = typed.dns.iter().map(ToString::to_string).collect();
        sidecar.cap_add = typed.cap_add.iter().map(ToString::to_string).collect();
        sidecar.user = typed.user.as_ref().map(ToString::to_string);
        sidecar.privileged = typed.privileged;
        sidecar.entrypoint = typed
            .entrypoint
            .as_ref()
            .map(|words| words.iter().map(ToString::to_string).collect());
        sidecar.health = typed.health.as_ref().map(|health| Health {
            cmd:             health
                .cmd
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            interval_ms:     health.interval_ms,
            timeout_ms:      health.timeout_ms,
            retries:         health.retries,
            start_period_ms: health.start_period_ms,
        });
        if let Some(credentials) = &service.credentials {
            let password = resolve_secret(&credentials.password_secret, ctx)?;
            sidecar.registry_auth = Some(RegistryAuth {
                username: credentials.username.to_string(),
                password,
                server: None,
            });
        }
        sidecars.push(sidecar);
    }
    Ok(sidecars)
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

/// The Docker `provider_config` for a scope container: an init process
/// reaps zombies, the host alias resolves to the gateway, and the
/// workspace is the sandbox's own volume — no bind from this machine.
fn docker_provider_config(
    registry_auth: Option<RegistryAuth>,
    sidecars: Vec<Sidecar>,
    container: &ir::ContainerOptions,
) -> DockerProviderConfig {
    // Steps run as a program plus arguments, so an image without bash
    // (alpine) works: the provider's exec wrapper needs only /bin/sh.
    DockerProviderConfig {
        init: true,
        privileged: container.privileged,
        platform: container.platform.as_ref().map(ToString::to_string),
        extra_hosts: vec![format!("{DOCKER_HOST_ALIAS}:host-gateway")],
        dns: container.dns.iter().map(ToString::to_string).collect(),
        cap_add: container.cap_add.iter().map(ToString::to_string).collect(),
        registry_auth,
        sidecars,
        ..DockerProviderConfig::default()
    }
}

/// What release needs: the lease the environment held, and whether this
/// executor alone owns it.
pub(crate) struct SandboxTeardown {
    host:       bool,
    sandbox:    Arc<dyn Sandbox>,
    lease:      SandboxLeaseId,
    /// No coordinator named the lease: the sandbox ends with the scope.
    standalone: bool,
}

impl fmt::Debug for SandboxTeardown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxTeardown")
            .field("host", &self.host)
            .field("sandbox", &self.sandbox.id())
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
