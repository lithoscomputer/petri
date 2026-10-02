//! Acquiring and releasing scope environments.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
/// The scope identities and the lease id are the IR's, so the engine's
/// records can name them without depending on this crate.
pub use ir::{EnvironmentId, SandboxLeaseId, WorkspaceId};
use ir::{
    RegistryCredentials, RuntimeSpec, SandboxInstance, ScopeId, ServiceOptions, WorkspacePolicy,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::container::ContainerRunner;
use crate::env::ExecEnv;
use crate::error::{EnvError, ReleaseReport};
use crate::progress::{NoProgress, ProgressSink};
use crate::secrets::{MapSecrets, SecretProvider};

/// The default time a step gets between `SIGTERM` and `SIGKILL`.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(10);

/// A sidecar container the scope needs, with its env already resolved.
/// Credentials stay a secret *name*; the executor resolves it inside acquire,
/// at the point of use.
#[derive(Clone)]
pub struct ServiceSpec {
    /// The alias other processes in the scope reach it by.
    pub name:        SmolStr,
    pub image:       SmolStr,
    pub env:         BTreeMap<SmolStr, SmolStr>,
    /// Typed service options (the health check rides here).
    pub options:     ServiceOptions,
    pub credentials: Option<RegistryCredentials>,
}

/// Hand-written for the same reason [`crate::ProcessSpec`]'s is: `env` is a
/// resolved environment map, so the shape prints and the values do not. Today
/// nothing secret lands in it — secret-valued service env is refused at
/// lowering — but the type must stay safe to print when that changes.
impl fmt::Debug for ServiceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceSpec")
            .field("name", &self.name)
            .field("image", &self.image)
            .field("env", &self.env.keys())
            .field("options", &self.options)
            .field("credentials", &self.credentials)
            .finish()
    }
}

impl ServiceSpec {
    pub fn new(name: &str, image: &str) -> Self {
        Self {
            name:        SmolStr::new(name),
            image:       SmolStr::new(image),
            env:         BTreeMap::new(),
            options:     ServiceOptions::default(),
            credentials: None,
        }
    }
}

/// What one scope instance needs in order to exist — the complete declarative
/// request: environment, runtime, sidecar services. [`Executor::acquire`]
/// realizes all of it; [`Executor::release`] tears all of it down.
#[derive(Clone)]
pub struct ScopeSpec {
    pub id:           ScopeId,
    /// Process, container, and service fence identity.
    pub environment:  EnvironmentId,
    /// Persistent filesystem identity. It can outlive an environment.
    pub workspace_id: WorkspaceId,
    /// The scope's env, already resolved. Secrets are not here — they are
    /// fetched at spawn time and never written down.
    pub env:          BTreeMap<SmolStr, SmolStr>,
    pub runtime:      RuntimeSpec,
    pub workspace:    WorkspacePolicy,
    /// Sidecar containers with this scope's lifetime, healthy before acquire
    /// returns.
    pub services:     Vec<ServiceSpec>,
    pub grace:        Duration,
}

/// Hand-written for the same reason [`crate::ProcessSpec`]'s is: `env` is a
/// resolved environment map, so the shape prints and the values do not.
impl fmt::Debug for ScopeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopeSpec")
            .field("id", &self.id)
            .field("environment", &self.environment)
            .field("workspace_id", &self.workspace_id)
            .field("env", &self.env.keys())
            .field("runtime", &self.runtime)
            .field("workspace", &self.workspace)
            .field("services", &self.services)
            .field("grace", &self.grace)
            .finish()
    }
}

impl ScopeSpec {
    pub fn new(id: ScopeId, instance: &str) -> Self {
        let instance = SmolStr::new(instance);
        Self {
            id,
            environment: EnvironmentId::new(instance.clone()),
            workspace_id: WorkspaceId::new(instance),
            env: BTreeMap::new(),
            runtime: RuntimeSpec::host_process(),
            workspace: WorkspacePolicy::Shared,
            services: Vec::new(),
            grace: DEFAULT_GRACE,
        }
    }

    #[must_use]
    pub fn with_environment_id(mut self, environment: EnvironmentId) -> Self {
        self.environment = environment;
        self
    }

    #[must_use]
    pub fn with_workspace_id(mut self, workspace: WorkspaceId) -> Self {
        self.workspace_id = workspace;
        self
    }

    #[must_use]
    pub fn with_env(mut self, env: BTreeMap<SmolStr, SmolStr>) -> Self {
        self.env = env;
        self
    }

    #[must_use]
    pub fn with_runtime(mut self, runtime: RuntimeSpec) -> Self {
        self.runtime = runtime;
        self
    }

    #[must_use]
    pub fn with_services(mut self, services: Vec<ServiceSpec>) -> Self {
        self.services = services;
        self
    }

    #[must_use]
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }
}

/// The effect services acquire may need, resolved at the exact effect boundary:
/// credential references become plaintext only inside acquire, and live
/// progress (pulls, service health) flows out without ever entering the replay
/// log.
#[derive(Clone)]
pub struct AcquireContext {
    secrets:  Arc<dyn SecretProvider>,
    progress: Arc<dyn ProgressSink>,
    lease:    Option<SandboxLeaseId>,
}

impl AcquireContext {
    pub fn new(secrets: Arc<dyn SecretProvider>, progress: Arc<dyn ProgressSink>) -> Self {
        Self {
            secrets,
            progress,
            lease: None,
        }
    }

    /// No secrets, no progress: tests and hosts with nothing to wire.
    pub fn bare() -> Self {
        Self {
            secrets:  Arc::new(MapSecrets::empty()),
            progress: Arc::new(NoProgress),
            lease:    None,
        }
    }

    /// The durable lease this scope's sandbox lives under, when a coordinator
    /// allocated one. Without it, the executor treats the acquisition as
    /// standalone: the sandbox is the scope's alone and goes with its release.
    #[must_use]
    pub fn with_lease(mut self, lease: SandboxLeaseId) -> Self {
        self.lease = Some(lease);
        self
    }

    pub fn secrets(&self) -> &Arc<dyn SecretProvider> {
        &self.secrets
    }

    pub fn progress(&self) -> &Arc<dyn ProgressSink> {
        &self.progress
    }

    pub fn lease(&self) -> Option<SandboxLeaseId> {
        self.lease
    }
}

/// Whether the work in a scope succeeded, which decides workspace retention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeOutcome {
    Succeeded,
    Failed,
}

/// When to keep a workspace after release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Retention {
    Always,
    /// The default: a failed scope's workspace is what you need to debug it.
    #[default]
    OnFailure,
    Never,
}

impl Retention {
    pub fn keeps(self, outcome: ScopeOutcome) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::OnFailure => outcome == ScopeOutcome::Failed,
        }
    }
}

/// What it takes to tear one environment down.
///
/// Each executor defines its own — a workspace path and a retention policy on
/// the host, a container name besides under Docker — and gets it back,
/// untouched, in [`Executor::release`]. The interface only carries it and never
/// looks inside, so a new kind of environment needs no change here. Any `Debug
/// + Send + Sync + 'static` type qualifies: an executor derives `Debug` on a
/// struct and passes it to [`EnvHandle::new`].
pub trait Teardown: Any + fmt::Debug + Send + Sync {}

impl<T: Any + fmt::Debug + Send + Sync> Teardown for T {}

/// A live environment, and what it takes to get rid of it.
#[derive(Clone)]
pub struct EnvHandle {
    scope:    ScopeId,
    instance: SmolStr,
    sandbox:  SandboxInstance,
    env:      Arc<dyn ExecEnv>,
    teardown: Arc<dyn Teardown>,
    runner:   Option<Arc<dyn ContainerRunner>>,
}

impl EnvHandle {
    /// `instance` is the executor's environment id for the scope; `sandbox`
    /// is what the executor acquired for it, as the driver records it.
    pub fn new(
        scope: ScopeId,
        instance: SmolStr,
        sandbox: SandboxInstance,
        env: Arc<dyn ExecEnv>,
        teardown: impl Teardown,
    ) -> Self {
        Self {
            scope,
            instance,
            sandbox,
            env,
            teardown: Arc::new(teardown),
            runner: None,
        }
    }

    /// Replace the process environment while preserving the scope identity,
    /// sandbox, container runner and the originating executor's teardown
    /// record. An executor layer uses this to decorate the environment it
    /// acquired.
    #[must_use]
    pub fn with_exec(mut self, env: Arc<dyn ExecEnv>) -> Self {
        self.env = env;
        self
    }

    /// Bind a scope-bound one-shot container runner to this environment. An
    /// executor that can run containers in this scope's world attaches one;
    /// pure executors that cannot simply never call this.
    #[must_use]
    pub fn with_runner(mut self, runner: Arc<dyn ContainerRunner>) -> Self {
        self.runner = Some(runner);
        self
    }

    pub fn scope(&self) -> ScopeId {
        self.scope
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The sandbox the executor acquired: the record the driver appends
    /// when the scope is acquired.
    pub fn sandbox(&self) -> &SandboxInstance {
        &self.sandbox
    }

    /// The capability handed to step kinds.
    pub fn exec(&self) -> Arc<dyn ExecEnv> {
        self.env.clone()
    }

    /// The scope-bound one-shot container runner, when the executor provided
    /// one.
    pub fn container_runner(&self) -> Option<Arc<dyn ContainerRunner>> {
        self.runner.clone()
    }

    /// The executor's own teardown record, when this handle was made by an
    /// executor that uses `T`. `None` means the handle came from some other
    /// executor.
    pub fn teardown<T: Teardown>(&self) -> Option<&T> {
        // Deref to the trait object before upcasting. `Arc<dyn Teardown>` is itself a
        // `Teardown`, and a method call on the `Arc` would resolve there and downcast
        // the wrapper instead of what it holds.
        let record: &dyn Any = &*self.teardown;
        record.downcast_ref::<T>()
    }
}

#[expect(
    clippy::missing_fields_in_debug,
    reason = "`env` and `runner` are trait objects with no `Debug` bound — an executor's \
              environment and container runner have no printable form, and requiring one \
              would constrain every implementation"
)]
impl fmt::Debug for EnvHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvHandle")
            .field("scope", &self.scope)
            .field("instance", &self.instance)
            .field("sandbox", &self.sandbox)
            .field("teardown", &self.teardown)
            .finish()
    }
}

/// Materializes environments. Knows nothing about what steps mean.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Materialize the scope's complete environment: workspace, env, sidecar
    /// services — everything the [`ScopeSpec`] declares. `ctx` carries the
    /// effect services: secrets resolve here, at the point of use, and live
    /// progress flows through the sink without touching the replay log.
    ///
    /// **`acquire` fences prior work** (§9): when it returns, no process from a
    /// previous acquisition of this scope can still mutate the workspace or be
    /// observed as this environment's status. A driver crash does not kill
    /// running steps — release owns cleanup, and remote sandboxes outlive
    /// workers by design — so resume leans on this fence before re-dispatching.
    /// The stock executors implement it (the host executor's generation-scoped
    /// sentinel protocol; Docker's remove-by-deterministic-name); a remote
    /// provider implements its own reconnect-and-fence here. Work that cannot
    /// be safely ended fails the acquire with [`EnvError::FenceLeaked`] rather
    /// than signalling an unverified id. The fence is idempotent, and it covers
    /// the workspace only: side effects outside it (network calls, pushes) may
    /// have happened in the crashed attempt and happen again — resume is
    /// at-least-once for external side effects.
    async fn acquire(&self, scope: &ScopeSpec, ctx: &AcquireContext)
    -> Result<EnvHandle, EnvError>;

    /// Tear down. Idempotent, and never fails the run: problems are reported.
    ///
    /// `outcome` decides workspace retention, which the handoff's signature has
    /// no room for — a scope cannot know on its own whether its work
    /// failed.
    async fn release(&self, env: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport;
}
