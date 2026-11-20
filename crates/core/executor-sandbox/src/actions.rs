//! One-shot containers for host-process scopes.
//!
//! Container actions in Host jobs are refused until sandbox-driver supports
//! portable host-workspace mounts. Recovery still sweeps action hosts created
//! by earlier versions, using their persisted entry and namespace markers.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use executor::{ContainerRunner, EnvError, OneShotContainer, ProcessHandle};
use sandbox_driver::{
    Error as DriverError, LabelFilter, NamespaceId, ProviderName, SandboxId, SandboxRef, Sandboxes,
};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::Mutex;

use crate::gate::RunGate;
use crate::run::{RunIdentity, scope_dir};
use crate::{BACKEND, CONTAINER_WORKSPACE, acquire_failed, parity_gap};

pub(crate) const ACTIONS_LABEL: &str = "petri.actions";
const MARKER: &str = "action-host";

fn marker_path(run_dir: &Path, workspace_id: &str) -> PathBuf {
    scope_dir(run_dir, workspace_id).join(MARKER)
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
        let found = sandboxes
            .find(filter)
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
}

/// The shared action host, independent of any scope's environment variables.
pub(crate) struct ActionHost {
    sandboxes:    Sandboxes,
    identity:     Arc<RunIdentity>,
    workspace_id: String,
    state:        Arc<Mutex<HostState>>,
}

impl ActionHost {
    pub(crate) fn new(
        sandboxes: Sandboxes,
        identity: Arc<RunIdentity>,
        workspace_id: String,
    ) -> Self {
        Self {
            sandboxes,
            identity,
            workspace_id,
            state: Arc::new(Mutex::new(HostState::default())),
        }
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

    pub(crate) async fn teardown(&self) -> Result<bool, EnvError> {
        let mut state = self.state.lock().await;
        if state.released {
            return Ok(false);
        }
        let removed =
            remove_recorded(&self.sandboxes, &self.identity, &self.workspace_id, None).await?;
        state.released = true;
        Ok(!removed.is_empty())
    }
}

/// Refuse actions without blocking ordinary Host process scopes or recovery.
pub(crate) struct ActionHostRunner {
    pub(crate) gate: RunGate,
}

#[async_trait::async_trait]
impl ContainerRunner for ActionHostRunner {
    fn workspace_path(&self) -> &str {
        CONTAINER_WORKSPACE
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        Err(parity_gap(
            "a host-workspace mount for container actions in Host jobs",
        ))
    }

    async fn run(&self, _spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        let _admission = self.gate.admit("one-shot")?;
        // sandbox-driver lacks a portable host-path mount. Creating an action
        // host without that mount would silently give actions another workspace.
        Err(parity_gap(
            "a host-workspace mount for container actions in Host jobs",
        ))
    }
}
