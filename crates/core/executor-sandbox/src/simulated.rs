//! The simulated provider: what a dry run acquires its scopes on.
//!
//! A dry run touches no provider. Its step kinds are stubs that run no
//! process and read no file, so a scope needs nothing behind it. This
//! built-in provider hands out sandboxes that are a name, a working
//! directory string and labels, and nothing else: no process runs and no
//! directory is created. The lease manager, the ledger and the run's scope
//! records work unchanged over it, so `scope.acquired` names the entry
//! `simulated`, `scope.released` records the lease's end, and a resume or a
//! prune finds records it knows to leave alone.
//!
//! A simulated sandbox lives as long as the process that created it. A
//! resumed dry run finds none on the provider and creates fresh ones
//! ([`crate::LostSandbox::Replace`]), which is what a sandbox with no state
//! amounts to.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use sandbox_driver::contract::{
    AccessScope, Activation, ByteStream, CapabilitySet, Descriptor, Durability, Error,
    ExecControls, ExecOutcome, ExecSpec, Expose, Exposure, FileEntry, FileRange, Files, Health,
    JsonSchema, LabelFilter, Locality, Namespace, NamespaceId, NamespaceScope, NetworkMode,
    PluginLocation, PortSupport, Ports, ProviderFactory, ProviderName, ProviderRuntime,
    ProviderSchemas, Resources, Sandbox, SandboxRef, SandboxState, SandboxSummary, SourceKind,
    Status, WorkloadAdmission,
};
use sandbox_driver::{Isolation, ProviderKind, SandboxId};

/// The provider kind, entry and environment a dry run's leases and scope
/// records name.
pub const SIMULATED_KIND: &str = "simulated";

/// The built-in factory of the simulated provider.
pub struct SimulatedFactory;

#[async_trait]
impl ProviderFactory for SimulatedFactory {
    async fn discover(&self, _location: &PluginLocation) -> Result<ProviderSchemas, Error> {
        Ok(schemas())
    }

    async fn activate(&self, activation: Activation) -> Result<Arc<dyn ProviderRuntime>, Error> {
        Ok(Arc::new(SimulatedRuntime {
            entry:     activation.name,
            sandboxes: Mutex::new(BTreeMap::new()),
            unnamed:   AtomicU64::new(0),
        }))
    }
}

fn schemas() -> ProviderSchemas {
    let closed: JsonSchema = serde_json::from_value(serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "additionalProperties": false,
    }))
    .expect("a constant schema");
    ProviderSchemas {
        kind:          ProviderKind::try_new(SIMULATED_KIND).expect("a constant kind name"),
        version:       env!("CARGO_PKG_VERSION").to_owned(),
        config_schema: closed.clone(),
        spec_schema:   Some(closed),
        secrets:       Vec::new(),
    }
}

fn namespace() -> NamespaceId {
    NamespaceId::try_new(SIMULATED_KIND).expect("a constant namespace")
}

/// One activation's sandboxes, keyed by id.
struct SimulatedRuntime {
    entry:     ProviderName,
    sandboxes: Mutex<BTreeMap<SandboxId, Arc<SimulatedSandbox>>>,
    unnamed:   AtomicU64,
}

impl SimulatedRuntime {
    fn get(&self, id: &SandboxId) -> Result<Arc<SimulatedSandbox>, Error> {
        self.sandboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound {
                resource: "sandbox".to_owned(),
                id:       id.to_string(),
            })
    }
}

#[async_trait]
impl ProviderRuntime for SimulatedRuntime {
    async fn describe(&self) -> Result<Descriptor, Error> {
        Ok(Descriptor {
            schemas:      schemas(),
            namespace:    Namespace {
                id:    namespace(),
                scope: NamespaceScope::Process,
            },
            // No isolation is claimed: there is nothing to isolate.
            isolation:    Isolation::None,
            durability:   Durability::Process,
            access:       AccessScope::OwnerProcess,
            sources:      BTreeSet::from([SourceKind::HostDirectory]),
            locality:     Locality::default(),
            resources:    BTreeSet::new(),
            network:      BTreeSet::from([NetworkMode::ProviderDefault]),
            ports:        PortSupport::None,
            capabilities: CapabilitySet::default(),
        })
    }

    async fn health(&self) -> Result<Health, Error> {
        Ok(Health {
            healthy: true,
            message: None,
        })
    }

    /// A sandbox named as the spec asks, at the working directory it asks
    /// for, carrying its labels. A name already in use is the sandbox that
    /// holds it, as on a real provider.
    async fn create(&self, admission: WorkloadAdmission) -> Result<Box<dyn Sandbox>, Error> {
        let spec = admission.spec;
        let name = spec.name.clone().unwrap_or_else(|| {
            format!(
                "{SIMULATED_KIND}-{}",
                self.unnamed.fetch_add(1, Ordering::Relaxed)
            )
        });
        let id = SandboxId::try_new(name.as_str()).map_err(|error| Error::InvalidSpec {
            path:    "name".to_owned(),
            message: error.to_string(),
        })?;
        let mut sandboxes = self
            .sandboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if sandboxes.contains_key(&id) {
            return Err(Error::AlreadyExists {
                name,
                id: id.to_string(),
            });
        }
        let sandbox = Arc::new(SimulatedSandbox {
            reference: SandboxRef {
                provider:  self.entry.clone(),
                namespace: namespace(),
                id:        id.clone(),
            },
            name,
            labels: spec.labels,
            working_directory: PathBuf::from(
                spec.working_directory
                    .unwrap_or_else(|| format!("/{SIMULATED_KIND}")),
            ),
            env: BTreeMap::new(),
            state: Mutex::new(SandboxState::Running),
        });
        sandboxes.insert(id, sandbox.clone());
        Ok(Box::new(Handle(sandbox)))
    }

    async fn attach(&self, id: &SandboxId) -> Result<Box<dyn Sandbox>, Error> {
        Ok(Box::new(Handle(self.get(id)?)))
    }

    async fn list(&self, filter: &LabelFilter) -> Result<Vec<SandboxSummary>, Error> {
        Ok(self
            .sandboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|sandbox| filter.matches(&sandbox.labels))
            .map(|sandbox| SandboxSummary {
                reference: sandbox.reference.clone(),
                name:      Some(sandbox.name.clone()),
                state:     sandbox.state(),
                labels:    sandbox.labels.clone(),
            })
            .collect())
    }

    async fn destroy(&self, id: &SandboxId) -> Result<(), Error> {
        self.sandboxes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// A sandbox that is a name and a state.
struct SimulatedSandbox {
    reference:         SandboxRef,
    name:              String,
    labels:            BTreeMap<String, String>,
    working_directory: PathBuf,
    /// Empty: no process starts here, so no variable is in force.
    env:               BTreeMap<String, String>,
    state:             Mutex<SandboxState>,
}

impl SimulatedSandbox {
    fn state(&self) -> SandboxState {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set_state(&self, state: SandboxState) {
        *self.state.lock().unwrap_or_else(PoisonError::into_inner) = state;
    }
}

/// A handle on a simulated sandbox. It refuses every process and holds no
/// file.
struct Handle(Arc<SimulatedSandbox>);

fn refusal(what: &str) -> Error {
    Error::Unsupported {
        capability: format!("{what}: a simulated sandbox runs no process and holds no files"),
    }
}

#[async_trait]
impl Sandbox for Handle {
    fn reference(&self) -> SandboxRef {
        self.0.reference.clone()
    }

    fn working_directory(&self) -> &Path {
        &self.0.working_directory
    }

    fn env(&self) -> &BTreeMap<String, String> {
        &self.0.env
    }

    async fn status(&self) -> Result<Status, Error> {
        Ok(Status {
            reference: self.reference(),
            state:     self.0.state(),
            name:      Some(self.0.name.clone()),
            labels:    self.0.labels.clone(),
            image:     None,
            snapshot:  None,
            resources: Resources::default(),
            network:   None,
            services:  BTreeMap::new(),
            web_url:   None,
        })
    }

    async fn start(&self) -> Result<(), Error> {
        self.0.set_state(SandboxState::Running);
        Ok(())
    }

    async fn stop(&self) -> Result<(), Error> {
        self.0.set_state(SandboxState::Stopped);
        Ok(())
    }

    async fn exec(&self, _spec: ExecSpec, _controls: ExecControls) -> Result<ExecOutcome, Error> {
        Err(refusal("exec"))
    }

    fn files(&self) -> &dyn Files {
        self
    }

    fn ports(&self) -> &dyn Ports {
        self
    }
}

/// Nothing exists, and nothing can be written.
#[async_trait]
impl Files for Handle {
    async fn get(&self, path: &str, _range: Option<FileRange>) -> Result<ByteStream, Error> {
        Err(Error::NotFound {
            resource: "file".to_owned(),
            id:       path.to_owned(),
        })
    }

    async fn put(&self, _path: &str, _bytes: ByteStream) -> Result<(), Error> {
        Err(refusal("files"))
    }

    async fn list(&self, _path: &str, _depth: u32) -> Result<Vec<FileEntry>, Error> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl Ports for Handle {
    async fn expose(&self, _port: NonZeroU16, _options: Expose) -> Result<Exposure, Error> {
        Err(refusal("ports"))
    }

    async fn release(&self, _port: NonZeroU16) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sandbox_driver::contract::{ExecControls, ExecSpec, LabelFilter, SandboxSpec, Source};

    use crate::SandboxCatalog;

    #[tokio::test]
    async fn a_created_sandbox_is_found_by_label_and_attached_until_destroyed() {
        let sandboxes = SandboxCatalog::simulated()
            .open()
            .expect("the simulated catalog opens");
        let mut spec = SandboxSpec::new(Source::HostDirectory);
        spec.name = Some("petri-run-l1".to_owned());
        spec.working_directory = Some("/simulated/main".to_owned());
        spec.labels.insert("petri.run".to_owned(), "run".to_owned());
        let sandbox = sandboxes
            .builder(super::SIMULATED_KIND)
            .await
            .expect("the environment exists")
            .spec(spec)
            .create()
            .await
            .expect("a simulated sandbox is created");
        assert_eq!(sandbox.working_directory(), Path::new("/simulated/main"));
        let reference = sandbox.reference();
        let found = sandboxes
            .find(LabelFilter::eq("petri.run", "run"))
            .await
            .expect("find");
        assert_eq!(
            found
                .sandboxes
                .iter()
                .map(|summary| summary.reference.clone())
                .collect::<Vec<_>>(),
            vec![reference.clone()]
        );
        assert!(
            sandbox
                .exec(ExecSpec::argv(["true"]), ExecControls::default())
                .await
                .is_err_and(|error| error.to_string().contains("runs no process"))
        );
        sandboxes.attach(&reference).await.expect("attached");
        sandboxes.destroy(&reference).await.expect("destroyed");
        assert!(sandboxes.attach(&reference).await.is_err());
    }
}
