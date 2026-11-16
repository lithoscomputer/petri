//! One-shot containers for host-process scopes.
//!
//! Scopes sharing a workspace share one action host. Its sandbox, on the
//! run's container environment, binds the host workspace at `/workspace`;
//! each scope's runner supplies its own env. The action host ends with its
//! owning lease, or with a standalone scope. A marker, written before the
//! create, records the entry and namespace the action host is created on,
//! so the first acquire after a crash sweeps old action hosts without
//! contacting a provider for workspaces that never ran an action.

use std::collections::BTreeMap;
use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use executor::{ContainerRunner, EnvError, OneShotContainer, ProcessHandle, ScopeSpec};
use sandbox_driver::{
    ContainerSpec, Error as DriverError, LabelFilter, NamespaceId, ProviderName, Sandbox,
    SandboxId, SandboxRef, Sandboxes, Source,
};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use tokio::fs;
use tokio::sync::Mutex;

use crate::env::OneShotRunner;
use crate::gate::RunGate;
use crate::run::{RUN_LABEL, RunIdentity, scope_dir, write_record};
use crate::{BACKEND, CONTAINER_WORKSPACE, DOCKER_HOST_ALIAS, acquire_failed, catalog};

pub(crate) const ACTIONS_LABEL: &str = "petri.actions";
const IMAGE_VAR: &str = "PETRI_SANDBOX_ACTION_HOST_IMAGE";
const DEFAULT_IMAGE: &str = "alpine:3.20";
const MARKER: &str = "action-host";

fn marker_path(run_dir: &Path, workspace_id: &str) -> PathBuf {
    scope_dir(run_dir, workspace_id).join(MARKER)
}

fn host_name(prefix: &str, workspace_id: &str) -> String {
    let safe: String = workspace_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{prefix}a-{safe}")
}

/// Where an action host is created: the entry and the namespace that entry
/// reached when the marker was written.
#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    provider:  ProviderName,
    namespace: NamespaceId,
}

/// Delete an action host, reconciling an uncertain create by its marker and
/// label. `sandboxes` is only contacted when there may be an action host to
/// delete. A marker from before the shared sandbox API, which records a
/// provider fingerprint, is reconciled by label on every entry.
pub(crate) async fn remove_recorded(
    sandboxes: &Sandboxes,
    identity: &RunIdentity,
    workspace_id: &str,
    known: Option<&SandboxRef>,
) -> Result<Vec<SandboxId>, EnvError> {
    let path = marker_path(identity.run_dir(), workspace_id);
    let marker = match fs::read_to_string(&path).await {
        Ok(marker) => serde_json::from_str::<Marker>(&marker).ok(),
        Err(error) if error.kind() == ErrorKind::NotFound && known.is_none() => {
            return Ok(Vec::new());
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(EnvError::workspace("read", path.display(), error)),
    };
    let references = if let Some(known) = known {
        vec![known.clone()]
    } else {
        let filter = LabelFilter::eq(ACTIONS_LABEL, identity.workspace_label(workspace_id));
        let found = catalog::find(sandboxes, filter)
            .await
            .map_err(|error| acquire_failed(&error))?;
        let on_entry = |entry: &ProviderName| {
            marker
                .as_ref()
                .is_none_or(|marker| &marker.provider == entry)
        };
        if let Some(failure) = found
            .failures
            .iter()
            .find(|failure| on_entry(&failure.entry))
        {
            return Err(acquire_failed(&failure.error));
        }
        found
            .sandboxes
            .into_iter()
            .map(|summary| summary.reference)
            .filter(|reference| on_entry(&reference.provider))
            .collect()
    };
    for reference in &references {
        if let Some(marker) = &marker
            && marker.provider == reference.provider
            && marker.namespace != reference.namespace
        {
            return Err(EnvError::backend(
                BACKEND,
                "one-shot",
                "the action host's recorded namespace differs from the one its entry reaches",
            ));
        }
        match sandboxes.destroy(reference).await {
            Ok(()) | Err(DriverError::NotFound { .. }) => {}
            Err(error) => return Err(acquire_failed(&error)),
        }
    }
    let _ = fs::remove_file(path).await;
    Ok(references
        .into_iter()
        .map(|reference| reference.id)
        .collect())
}

#[derive(Default)]
struct HostState {
    prepared: bool,
    released: bool,
    sandbox:  Option<Arc<dyn Sandbox>>,
}

/// The shared action host, independent of any scope's environment variables.
pub(crate) struct ActionHost {
    sandboxes:    Sandboxes,
    /// The catalog environment the action host is created on.
    environment:  String,
    identity:     Arc<RunIdentity>,
    workspace:    PathBuf,
    workspace_id: String,
    host_address: String,
    state:        Arc<Mutex<HostState>>,
}

impl ActionHost {
    pub(crate) fn new(
        sandboxes: Sandboxes,
        environment: String,
        identity: Arc<RunIdentity>,
        workspace: PathBuf,
        workspace_id: String,
        host_address: String,
    ) -> Self {
        Self {
            sandboxes,
            environment,
            identity,
            workspace,
            workspace_id,
            host_address,
            state: Arc::new(Mutex::new(HostState::default())),
        }
    }

    fn marker(&self) -> PathBuf {
        marker_path(self.identity.run_dir(), &self.workspace_id)
    }

    /// Sweep once per workspace in this process. A later inherited acquire
    /// shares this state and cannot sweep its caller's live action host.
    async fn prepare_locked(&self, state: &mut HostState) -> Result<(), EnvError> {
        if !state.prepared {
            remove_recorded(&self.sandboxes, &self.identity, &self.workspace_id, None).await?;
            state.prepared = true;
        }
        Ok(())
    }

    pub(crate) async fn prepare(&self) {
        let mut state = self.state.lock().await;
        if let Err(error) = self.prepare_locked(&mut state).await {
            tracing::warn!(error = %error, "sweeping stale action hosts failed");
        }
    }

    async fn create(&self) -> Result<Arc<dyn Sandbox>, EnvError> {
        let builder = self
            .sandboxes
            .builder(&self.environment)
            .await
            .map_err(|error| acquire_failed(&error))?;
        let template = self
            .sandboxes
            .environment(&self.environment)
            .await
            .map_err(|error| acquire_failed(&error))?;
        let descriptor = self
            .sandboxes
            .provider(template.provider.as_str())
            .map_err(|error| acquire_failed(&error))?
            .describe()
            .await
            .map_err(|error| acquire_failed(&error))?;
        // The workspace is a directory on this machine: the action host
        // must be a container that mounts it.
        if !descriptor.locality.mounts_host_paths
            || descriptor.isolation == sandbox_driver::Isolation::None
        {
            return Err(EnvError::backend(
                BACKEND,
                "one-shot",
                format!(
                    "Docker actions in Host jobs require a container environment that mounts \
                     this machine's paths; environment `{}` does not (use --backend docker for \
                     a remote daemon)",
                    self.environment
                ),
            ));
        }
        let marker = Marker {
            provider:  template.provider.clone(),
            namespace: descriptor.namespace.id.clone(),
        };
        let marker = serde_json::to_vec(&marker).expect("a marker serializes");
        write_record(self.marker(), marker).await?;
        let image = env::var(IMAGE_VAR)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_IMAGE.to_owned());
        let mut spec = template.spec;
        spec.source = Source::Image(image);
        spec.name = Some(host_name(
            &self.identity.container_prefix(),
            &self.workspace_id,
        ));
        spec.working_directory = Some(CONTAINER_WORKSPACE.to_owned());
        spec.labels
            .insert(RUN_LABEL.to_owned(), self.identity.run_id().to_owned());
        spec.labels.insert(
            ACTIONS_LABEL.to_owned(),
            self.identity.workspace_label(&self.workspace_id),
        );
        let container = spec.container.get_or_insert_with(ContainerSpec::default);
        container.init = Some(true);
        container
            .extra_hosts
            .insert(DOCKER_HOST_ALIAS.to_owned(), "host-gateway".to_owned());
        // TODO(parity): the host workspace bind is the Docker provider's own
        // option until the shared API carries a portable host-path mount; a
        // container environment on another provider refuses it at admission.
        spec.provider_config.insert(
            "binds".to_owned(),
            serde_json::json!([{
                "host": self.workspace.display().to_string(),
                "container": CONTAINER_WORKSPACE,
            }]),
        );
        let sandbox = builder
            .spec(spec)
            .create()
            .await
            .map_err(|error| acquire_failed(&error))?;
        Ok(Arc::from(sandbox))
    }

    async fn sandbox(self: &Arc<Self>) -> Result<Arc<dyn Sandbox>, EnvError> {
        let mut state = self.state.clone().lock_owned().await;
        if state.released {
            return Err(EnvError::backend(
                BACKEND,
                "one-shot",
                "the action host was released",
            ));
        }
        if let Some(sandbox) = &state.sandbox {
            return Ok(sandbox.clone());
        }
        self.prepare_locked(&mut state).await?;
        let host = self.clone();
        // The task owns the lock until create settles. Cancelling run() cannot
        // discard a late create: teardown waits for it and deletes its result.
        tokio::spawn(async move {
            let sandbox = match host.create().await {
                Ok(sandbox) => sandbox,
                Err(error) => {
                    state.prepared = false;
                    return Err(error);
                }
            };
            state.sandbox = Some(sandbox.clone());
            Ok::<_, EnvError>(sandbox)
        })
        .await
        .map_err(|error| EnvError::backend(BACKEND, "one-shot", error.to_string()))?
    }

    pub(crate) async fn teardown(&self) -> Result<bool, EnvError> {
        let mut state = self.state.lock().await;
        if state.released {
            return Ok(false);
        }
        let known = state.sandbox.as_ref().map(|sandbox| sandbox.reference());
        let removed = remove_recorded(
            &self.sandboxes,
            &self.identity,
            &self.workspace_id,
            known.as_ref(),
        )
        .await?;
        state.sandbox = None;
        state.released = true;
        Ok(!removed.is_empty())
    }
}

/// Each scope retains its own environment, even when the action host is shared.
pub(crate) struct ActionHostRunner {
    host: Result<Arc<ActionHost>, String>,
    env:  BTreeMap<SmolStr, SmolStr>,
    gate: RunGate,
}

impl ActionHostRunner {
    pub(crate) fn new(
        host: Result<Arc<ActionHost>, EnvError>,
        scope: &ScopeSpec,
        gate: RunGate,
    ) -> Self {
        Self {
            host: host.map_err(|error| error.to_string()),
            env: scope.env.clone(),
            gate,
        }
    }

    fn host(&self) -> Result<&Arc<ActionHost>, EnvError> {
        self.host
            .as_ref()
            .map_err(|message| EnvError::backend(BACKEND, "one-shot", message.clone()))
    }
}

#[async_trait::async_trait]
impl ContainerRunner for ActionHostRunner {
    fn workspace_path(&self) -> &str {
        CONTAINER_WORKSPACE
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        Ok(&self.host()?.host_address)
    }

    async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let host = self.host()?;
        // Admitted before the action host is created, so a finished run
        // creates none.
        let admission = self.gate.admit("one-shot")?;
        let sandbox = host.sandbox().await?;
        let runner = OneShotRunner {
            sandbox,
            workspace: CONTAINER_WORKSPACE.to_owned(),
            host_address: Some(host.host_address.clone()),
            env: self.env.clone(),
            gate: self.gate.clone(),
        };
        runner.run_admitted(spec, admission)
    }
}
