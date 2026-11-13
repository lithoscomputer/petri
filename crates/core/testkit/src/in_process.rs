//! Built-in provider factories, registered the way an embedding application
//! registers them, for tests of an application's own sandbox catalog.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use executor_sandbox::SandboxCatalog;
use sandbox_driver::ProviderKind;
use sandbox_driver::contract::{
    Activation, Environment, EnvironmentName, Error, MutationOutcome, PluginLocation,
    ProviderEntry, ProviderFactory, ProviderName, ProviderRuntime, ProviderSchemas, Registry,
    SandboxSpec, SecretResolver, Source, StaticEnvironments,
};

/// sandbox-driver's built-in Host provider, counting how many times a run
/// activated it.
#[derive(Default)]
pub struct HostFactory {
    connects: AtomicUsize,
}

impl HostFactory {
    pub fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ProviderFactory for HostFactory {
    async fn discover(&self, location: &PluginLocation) -> Result<ProviderSchemas, Error> {
        sandbox_driver_host::HostFactory.discover(location).await
    }

    async fn activate(&self, activation: Activation) -> Result<Arc<dyn ProviderRuntime>, Error> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        sandbox_driver_host::HostFactory.activate(activation).await
    }
}

/// A factory for `kind` that must never be activated: it counts the
/// attempt and refuses it.
pub struct UnusedFactory {
    kind:     &'static str,
    connects: AtomicUsize,
}

impl UnusedFactory {
    pub fn new(kind: &'static str) -> Self {
        Self {
            kind,
            connects: AtomicUsize::new(0),
        }
    }

    pub fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ProviderFactory for UnusedFactory {
    async fn discover(&self, _: &PluginLocation) -> Result<ProviderSchemas, Error> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        Err(Error::Unavailable {
            message: format!(
                "this test's run must not activate the {} provider",
                self.kind
            ),
            outcome: MutationOutcome::NotStarted,
        })
    }

    async fn activate(&self, _: Activation) -> Result<Arc<dyn ProviderRuntime>, Error> {
        unreachable!("discovery refuses first")
    }
}

struct NoSecrets;

#[async_trait]
impl SecretResolver for NoSecrets {
    async fn resolve(&self, _: &str) -> Result<Option<String>, Error> {
        Ok(None)
    }
}

/// A catalog of `host` and `docker` entries and environments over the given
/// factories, as an application that registers its own providers builds it.
/// With no Docker factory, the `docker` entry has no adapter installed.
pub fn catalog(
    host: Arc<dyn ProviderFactory>,
    docker: Option<Arc<dyn ProviderFactory>>,
) -> SandboxCatalog {
    let name = |name: &str| ProviderName::try_new(name).expect("a constant entry name");
    let entry = |kind: &str| -> ProviderEntry {
        serde_json::from_value(serde_json::json!({ "plugin": kind })).expect("a constant entry")
    };
    let mut registry = Registry::new()
        .builtin(ProviderKind::try_new("host").expect("kind"), host)
        .expect("one registration");
    if let Some(docker) = docker {
        registry = registry
            .builtin(ProviderKind::try_new("docker").expect("kind"), docker)
            .expect("each kind registers once");
    }
    let environments = BTreeMap::from([
        (
            EnvironmentName::try_new("host").expect("name"),
            Environment {
                provider: name("host"),
                spec:     SandboxSpec::new(Source::HostDirectory),
            },
        ),
        (
            EnvironmentName::try_new("docker").expect("name"),
            Environment {
                provider: name("docker"),
                spec:     SandboxSpec::new(Source::Image("alpine:3.20".to_owned())),
            },
        ),
    ]);
    SandboxCatalog::new(
        [
            (name("host"), entry("host")),
            (name("docker"), entry("docker")),
        ],
        Arc::new(StaticEnvironments(environments)),
        registry,
        Arc::new(NoSecrets),
    )
}

/// Built-in Host, with a Docker factory that must stay unactivated.
pub fn host_providers() -> (SandboxCatalog, Arc<HostFactory>, Arc<UnusedFactory>) {
    let host = Arc::new(HostFactory::default());
    let docker = Arc::new(UnusedFactory::new("docker"));
    (catalog(host.clone(), Some(docker.clone())), host, docker)
}
