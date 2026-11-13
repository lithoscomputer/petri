//! The run's sandbox catalog: the provider entries and environments scopes
//! are placed on, the providers that serve them, and the credential store
//! the driver resolves entry secrets and service credentials through.
//!
//! Host, Docker and Daytona are built-ins registered with
//! [`Registry::builtin`]; they run in this process and are never launched as
//! plugins. The executor names environments only. Which provider an
//! environment's entry reaches, and how, is the catalog's.
//!
//! Standalone Petri builds its catalog from a file
//! ([`SandboxCatalog::from_file`]) or, when given none, translates the
//! `--backend` options into one entry and one environment per backend
//! ([`SandboxCatalog::from_options`]). An embedding application hands over
//! its own entries, environment store and secret resolver
//! ([`SandboxCatalog::new`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::{env, fs};

use async_trait::async_trait;
use sandbox_driver::ProviderKind;
use sandbox_driver::contract::{
    CatalogOptions, Environment, EnvironmentName, EnvironmentSource, Error, Found, LabelFilter,
    MutationOutcome, ProviderEntry, ProviderFactory, ProviderName, Registry, SandboxSpec,
    Sandboxes, SecretResolver, Source, StaticEnvironments, Timers,
};
use serde::Deserialize;
use tokio::runtime::Handle;
use tokio::task;

use crate::backend::{DaytonaSandboxKind, SandboxBackend, SandboxOptions};
use crate::simulated::{SIMULATED_KIND, SimulatedFactory};

/// The environment a dry run places every scope on.
pub const SIMULATED_ENVIRONMENT: &str = SIMULATED_KIND;

/// Where a run's scopes can be placed: provider entries, the environments
/// on them, and the providers that serve them. Cheap to clone; a router
/// opens its own [`Sandboxes`] from it and shuts that down with the run.
#[derive(Clone)]
pub struct SandboxCatalog(Arc<Parts>);

#[derive(Clone)]
struct Parts {
    providers:      BTreeMap<ProviderName, ProviderEntry>,
    environments:   Arc<dyn EnvironmentSource>,
    registry:       Registry,
    secrets:        Arc<dyn SecretResolver>,
    /// Per environment, how its sandboxes reach services Petri runs on
    /// this machine, in place of what the provider reports.
    host_addresses: BTreeMap<String, Option<String>>,
}

impl SandboxCatalog {
    /// An application's catalog: its provider entries, its environment
    /// store, the providers it registered, and its credential store.
    pub fn new(
        providers: impl IntoIterator<Item = (ProviderName, ProviderEntry)>,
        environments: Arc<dyn EnvironmentSource>,
        registry: Registry,
        secrets: Arc<dyn SecretResolver>,
    ) -> Self {
        Self(Arc::new(Parts {
            providers: providers.into_iter().collect(),
            environments,
            registry,
            secrets,
            host_addresses: BTreeMap::new(),
        }))
    }

    /// How sandboxes in `environment` reach services Petri runs on this
    /// machine (the results and cache service of an action, a forwarded
    /// MCP server): a host name or address, or `None` when they cannot.
    /// Without one, the provider's reported locality decides: loopback for
    /// Host, the host alias on a local Docker daemon, and none elsewhere.
    #[must_use]
    pub fn with_host_address(mut self, environment: &str, address: Option<String>) -> Self {
        Arc::make_mut(&mut self.0)
            .host_addresses
            .insert(environment.to_owned(), address);
        self
    }

    /// The host addresses configured per environment.
    pub(crate) fn host_addresses(&self) -> &BTreeMap<String, Option<String>> {
        &self.0.host_addresses
    }

    /// Host, Docker and Daytona, registered as the built-ins they are.
    pub fn builtin_registry() -> Registry {
        let builtins: [(&str, Arc<dyn ProviderFactory>); 3] = [
            ("host", Arc::new(sandbox_driver_host::HostFactory)),
            ("docker", Arc::new(sandbox_driver_docker::DockerFactory)),
            ("daytona", Arc::new(sandbox_driver_daytona::DaytonaFactory)),
        ];
        builtins
            .into_iter()
            .fold(Registry::new(), |registry, (kind, factory)| {
                registry
                    .builtin(kind_name(kind), factory)
                    .expect("each built-in kind registers once")
            })
    }

    /// Reads `[providers.*]` and `[environments.*]` from a catalog file, with
    /// the built-in providers and entry secrets read from this process's
    /// environment.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            #[serde(default)]
            providers:    BTreeMap<ProviderName, ProviderEntry>,
            #[serde(default)]
            environments: BTreeMap<EnvironmentName, Environment>,
        }
        let content = fs::read_to_string(path)
            .map_err(|error| format!("reading sandbox catalog {}: {error}", path.display()))?;
        let file: File = toml::from_str(&content)
            .map_err(|error| format!("sandbox catalog {}: {error}", path.display()))?;
        Ok(Self::new(
            file.providers,
            Arc::new(StaticEnvironments(file.environments)),
            Self::builtin_registry(),
            Arc::new(ProcessSecrets),
        ))
    }

    /// The catalog the `--backend` options describe, for a run given no
    /// catalog file: an entry per built-in provider, configured from the
    /// environment as their plugins were (`DOCKER_HOST`, `DAYTONA_*`), and an
    /// environment named after each backend.
    ///
    /// - `host`: a host directory the run chooses per scope.
    /// - `docker`: Petri's pinned slim runner image; a container scope replaces
    ///   it with its own image.
    /// - `daytona`: the pinned runner image with Docker, at the configured
    ///   resources, with every timer disabled, since the run decides when its
    ///   sandboxes stop and go.
    pub fn from_options(options: &SandboxOptions) -> Result<Self, String> {
        let mut providers = BTreeMap::new();
        let mut environments = BTreeMap::new();
        let mut docker = BTreeMap::new();
        if let Some(host) = env_value("DOCKER_HOST") {
            docker.insert("host".to_owned(), serde_json::Value::String(host));
        }
        providers.insert(
            provider_name("host"),
            entry("host", BTreeMap::new(), Vec::new()),
        );
        providers.insert(provider_name("docker"), entry("docker", docker, Vec::new()));
        let mut daytona = BTreeMap::new();
        for (key, var) in [
            ("api_url", "DAYTONA_API_URL"),
            ("organization", "DAYTONA_ORGANIZATION_ID"),
            ("target", "DAYTONA_TARGET"),
        ] {
            if let Some(value) = env_value(var) {
                daytona.insert(key.to_owned(), serde_json::Value::String(value));
            }
        }
        let secrets = ["DAYTONA_API_KEY", "DAYTONA_JWT_TOKEN"]
            .into_iter()
            .find(|name| env_value(name).is_some())
            .map_or_else(
                || vec!["DAYTONA_API_KEY".to_owned()],
                |name| vec![name.to_owned()],
            );
        providers.insert(provider_name("daytona"), entry("daytona", daytona, secrets));

        if options.daytona_kind == DaytonaSandboxKind::VirtualMachine {
            // TODO(parity): Daytona VM sandboxes. The built-in Daytona
            // provider creates container sandboxes only until the shared API
            // carries the sandbox class (parity plan,
            // https://ampcode.com/threads/T-7b304bcd-d83a-42ed-818f-ffc6c291814c).
            // Refused loudly rather than placed on a container.
            return Err(
                "--daytona-kind vm is not yet carried by sandbox-driver's shared sandbox \
                        API; Petri supports it again once the API addition lands"
                    .to_owned(),
            );
        }
        // Host refuses every policy but its default, so a blocked run fails
        // there rather than running unblocked.
        let mut host_spec = SandboxSpec::new(Source::HostDirectory);
        host_spec.network = options.network.clone();
        environments.insert(environment_name("host"), Environment {
            provider: provider_name("host"),
            spec:     host_spec,
        });
        let mut docker_spec = SandboxSpec::new(Source::Image(
            SandboxOptions::default_runner_image(SandboxBackend::Docker),
        ));
        docker_spec.network = options.network.clone();
        environments.insert(environment_name("docker"), Environment {
            provider: provider_name("docker"),
            spec:     docker_spec,
        });
        let resources = options
            .daytona_resources
            .validated()
            .map_err(|error| error.to_string())?;
        let mut daytona_spec = SandboxSpec::new(Source::Image(
            SandboxOptions::default_runner_image(SandboxBackend::Daytona),
        ));
        daytona_spec.resources = resources;
        daytona_spec.network = options.network.clone();
        daytona_spec.user = Some("root".to_owned());
        daytona_spec.timers = Timers {
            auto_stop_ms: Some(0),
            auto_delete_ms: Some(0),
            ..Timers::default()
        };
        environments.insert(environment_name("daytona"), Environment {
            provider: provider_name("daytona"),
            spec:     daytona_spec,
        });
        let mut catalog = Self::new(
            providers,
            Arc::new(StaticEnvironments(environments)),
            Self::builtin_registry(),
            Arc::new(ProcessSecrets),
        );
        // An operator names the route back for a remote Docker daemon or
        // for Daytona, which report none.
        for (environment, var) in [
            ("docker", "PETRI_SANDBOX_DOCKER_HOST_ADDRESS"),
            ("daytona", "PETRI_SANDBOX_DAYTONA_HOST_ADDRESS"),
        ] {
            if let Some(address) = env_value(var) {
                catalog = catalog.with_host_address(environment, Some(address));
            }
        }
        Ok(catalog)
    }

    /// A dry run's catalog: one entry and one environment over the
    /// simulated provider, which creates nothing and runs nothing.
    pub fn simulated() -> Self {
        let registry = Registry::new()
            .builtin(kind_name(SIMULATED_KIND), Arc::new(SimulatedFactory))
            .expect("one registration");
        Self::new(
            [(
                provider_name(SIMULATED_KIND),
                entry(SIMULATED_KIND, BTreeMap::new(), Vec::new()),
            )],
            Arc::new(StaticEnvironments(BTreeMap::from([(
                environment_name(SIMULATED_ENVIRONMENT),
                Environment {
                    provider: provider_name(SIMULATED_KIND),
                    spec:     SandboxSpec::new(Source::HostDirectory),
                },
            )]))),
            registry,
            Arc::new(ProcessSecrets),
        )
    }

    /// A live catalog over the application's own credentials only: for an
    /// observer or a maintenance tool outside a run. Shut it down when done.
    pub fn open(&self) -> Result<Sandboxes, Error> {
        self.open_with(Arc::new(Credentials::default()))
    }

    /// A live catalog for one run. Service credentials the run resolves go
    /// into `credentials`, which the driver reads before the application's
    /// own store.
    pub(crate) fn open_with(&self, credentials: Arc<Credentials>) -> Result<Sandboxes, Error> {
        let secrets: Arc<dyn SecretResolver> = Arc::new(Layered {
            run:         credentials,
            application: self.0.secrets.clone(),
        });
        Sandboxes::try_new(
            self.0.providers.clone(),
            self.0.environments.clone(),
            &self.0.registry,
            secrets,
            CatalogOptions::default(),
        )
    }
}

fn env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn kind_name(kind: &str) -> ProviderKind {
    ProviderKind::try_new(kind).expect("a constant kind name is valid")
}

fn provider_name(name: &str) -> ProviderName {
    ProviderName::try_new(name).expect("a constant entry name is valid")
}

fn environment_name(name: &str) -> EnvironmentName {
    EnvironmentName::try_new(name).expect("a constant environment name is valid")
}

fn entry(
    kind: &str,
    config: BTreeMap<String, serde_json::Value>,
    secrets: Vec<String>,
) -> ProviderEntry {
    ProviderEntry {
        plugin: kind_name(kind),
        config,
        secrets,
        path: None,
        trust: None,
        args: Vec::new(),
        env: BTreeMap::new(),
        inherit_env: Some(Vec::new()),
    }
}

/// Entry secrets read from this process's environment by name, the way the
/// provider plugins read them before.
struct ProcessSecrets;

#[async_trait]
impl SecretResolver for ProcessSecrets {
    async fn resolve(&self, name: &str) -> Result<Option<String>, Error> {
        Ok(env_value(name))
    }
}

/// Credentials a run resolved from its own secrets for the sandboxes it is
/// creating: a service's registry credential, under a name only the spec
/// that needs it carries.
#[derive(Default)]
pub(crate) struct Credentials(Mutex<BTreeMap<String, String>>);

impl Credentials {
    pub(crate) fn insert(&self, name: String, value: String) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name, value);
    }

    pub(crate) fn remove(&self, names: &[String]) {
        let mut values = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        for name in names {
            values.remove(name);
        }
    }
}

struct Layered {
    run:         Arc<Credentials>,
    application: Arc<dyn SecretResolver>,
}

#[async_trait]
impl SecretResolver for Layered {
    async fn resolve(&self, name: &str) -> Result<Option<String>, Error> {
        let run = self
            .run
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned();
        match run {
            Some(value) => Ok(Some(value)),
            None => self.application.resolve(name).await,
        }
    }
}

/// [`Sandboxes::find`], run so that its caller's future stays `Send`.
///
/// The future `Sandboxes::find` returns is not `Send` as rustc sees it: its
/// per-entry closure trips the higher-ranked lifetime limitation
/// (rust-lang/rust#102211), so it cannot be awaited inside a spawned task or
/// an `async_trait` method. It runs here on a blocking thread, driven by the
/// current runtime, which never moves the future across threads.
// TODO(sandbox-driver): await `find` directly once sandbox-driver makes its
// future provably `Send`.
pub(crate) async fn find(sandboxes: &Sandboxes, filter: LabelFilter) -> Result<Found, Error> {
    let sandboxes = sandboxes.clone();
    let runtime = Handle::current();
    task::spawn_blocking(move || runtime.block_on(sandboxes.find(filter)))
        .await
        .unwrap_or_else(|error| {
            Err(Error::Transport {
                message: format!("the inventory task failed: {error}"),
                outcome: MutationOutcome::NotStarted,
            })
        })
}
