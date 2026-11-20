//! The run directory and the run's identity on its providers: the run key
//! every label carries, and the per-scope layout the host backend uses.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use executor::SandboxLeaseId;
use sha2::{Digest as _, Sha256};
use smol_str::SmolStr;

/// The label naming a sandbox's workspace: `<run id>/<workspace id>`. It
/// is the reconcile key: recovery lists the provider by it and attaches the
/// one match.
pub const WORKSPACE_LABEL: &str = "petri.workspace";
/// The label naming the run a sandbox belongs to, for prune.
pub const RUN_LABEL: &str = "petri.run";
/// The label naming the lease a sandbox belongs to.
pub const LEASE_LABEL: &str = "petri.lease";

/// The directory a host scope's state lives under: `scopes/<id>`.
pub(crate) fn scope_dir(run_dir: &Path, id: &str) -> PathBuf {
    run_dir.join("scopes").join(id)
}

/// The workspace of `workspace_id` for the host backend.
pub(crate) fn workspace_dir(run_dir: &Path, workspace_id: &str) -> PathBuf {
    scope_dir(run_dir, workspace_id).join("work")
}

/// A run's identity on the provider: the run id every label and name
/// derives from, and the run directory host scopes live under. The run id
/// is the run's store key; the coordinator hands it down so the reconcile
/// key can never disagree between a run and its resume. A driver with no
/// coordinator names its run after its run directory
/// ([`RunIdentity::for_run_dir`]), so a fresh executor over the same
/// directory still reaches what an earlier one left.
pub struct RunIdentity {
    run_dir: PathBuf,
    run_id:  SmolStr,
}

impl RunIdentity {
    pub fn new(run_dir: PathBuf, run_id: impl Into<SmolStr>) -> Self {
        Self {
            run_dir,
            run_id: run_id.into(),
        }
    }

    /// The identity a driver with no coordinator uses: a run id derived
    /// from the run directory's path, the same for every executor over that
    /// directory, in this process or the next.
    pub fn for_run_dir(run_dir: PathBuf) -> Self {
        let canonical = fs::canonicalize(&run_dir).unwrap_or_else(|_| run_dir.clone());
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        let mut id = String::with_capacity(16);
        for byte in &digest[..8] {
            let _ = write!(id, "{byte:02x}");
        }
        Self::new(run_dir, id)
    }

    pub(crate) fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// The run id the labels carry.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// `petri-<run id>-`: what the name of every sandbox this run owns
    /// starts with, for an operator's `docker ps` and a leak check.
    pub fn container_prefix(&self) -> String {
        format!("petri-{}-", self.run_id)
    }

    /// The workspace label value for `workspace_id`.
    pub fn workspace_label(&self, workspace_id: &str) -> String {
        format!("{}/{workspace_id}", self.run_id)
    }

    /// The sandbox name for a lease.
    pub fn container_name(&self, lease: SandboxLeaseId) -> String {
        format!("{}l{}", self.container_prefix(), lease.raw())
    }

    /// Every label a sandbox of this run carries.
    pub fn labels(&self, lease: SandboxLeaseId, workspace_id: &str) -> Vec<(String, String)> {
        vec![
            (RUN_LABEL.to_owned(), self.run_id.to_string()),
            (LEASE_LABEL.to_owned(), lease.raw().to_string()),
            (
                WORKSPACE_LABEL.to_owned(),
                self.workspace_label(workspace_id),
            ),
        ]
    }
}
