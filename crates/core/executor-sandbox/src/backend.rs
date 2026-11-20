//! Run-level placement. Workflow targets describe a process or container;
//! these options choose the system that provides it.

use std::collections::BTreeMap;
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::str::FromStr;

use executor::EnvError;
use ir::RuntimeSpec;
use sandbox_driver::{NetworkPolicy, Resources};

const RUNNER_PIN: &str = "f8bbbfd81934";
const DEFAULT_LABEL: &str = "ubuntu-24.04";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxBackend {
    /// Local processes, with Docker for explicit container targets.
    #[default]
    Host,
    /// Docker runner images for process targets and job images for containers.
    Docker,
    /// A Daytona sandbox, with a nested job container when requested.
    Daytona,
}

impl fmt::Display for SandboxBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Host => "host",
            Self::Docker => "docker",
            Self::Daytona => "daytona",
        })
    }
}

impl FromStr for SandboxBackend {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "host" => Ok(Self::Host),
            "docker" => Ok(Self::Docker),
            "daytona" => Ok(Self::Daytona),
            _ => Err(format!(
                "unknown backend {value:?}; expected host, docker, or daytona"
            )),
        }
    }
}

/// What an acquire does when a lease's recorded sandbox is gone from its
/// provider: the container was removed, the remote sandbox deleted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LostSandbox {
    /// Fail the acquire. The workspace was lost with the sandbox, and
    /// Petri does not replace it silently.
    #[default]
    Refuse,
    /// Create a fresh sandbox under the lease, with an empty workspace, for
    /// a host that restores the workspace itself before any attempt runs
    /// in it (`ExecutionHooks::scope_acquired`).
    Replace,
}

/// Configuration for the built-in sandbox router. Credentials come from
/// provider environment variables and never belong in this structure.
#[derive(Clone, Debug, Default)]
pub struct SandboxOptions {
    /// Which built-in environment process scopes go to when no catalog file
    /// names environments ([`crate::SandboxCatalog::from_options`]).
    pub backend:               SandboxBackend,
    /// A catalog file of provider entries and environments, in place of the
    /// one `backend` describes.
    pub catalog:               Option<PathBuf>,
    /// The environment process scopes are placed on; by default the
    /// backend's own.
    pub process_environment:   Option<String>,
    /// The environment container scopes, and the action host of a process
    /// scope on a host directory, are placed on; by default `daytona` on the
    /// Daytona backend and `docker` otherwise.
    pub container_environment: Option<String>,
    /// Network policy for every sandbox created by this run. Providers reject
    /// policies they cannot enforce. Blocked runs also verify attached
    /// sandboxes.
    pub network:               NetworkPolicy,
    /// What to do when a lease's recorded sandbox is gone from the provider.
    pub lost_sandbox:          LostSandbox,
    /// Label-to-image overrides. Daytona images must include Docker,
    /// `start-docker`, and Python 3. Empty requirements use `ubuntu-24.04`.
    pub runner_images:         BTreeMap<String, String>,
    pub daytona_resources:     DaytonaResources,
    /// The outer Daytona sandbox. Workflow `container:` selects a nested job.
    /// The Daytona provider offers container sandboxes only.
    pub daytona_kind:          DaytonaSandboxKind,
}

/// The Daytona offering that hosts a workflow runner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DaytonaSandboxKind {
    #[default]
    Container,
    VirtualMachine,
}

impl FromStr for DaytonaSandboxKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "container" => Ok(Self::Container),
            "vm" => Ok(Self::VirtualMachine),
            _ => Err(format!(
                "unknown Daytona sandbox kind {value:?}; expected container or vm"
            )),
        }
    }
}

/// Snapshot allocation. Include these values in the snapshot identity:
/// Daytona fixes a sandbox's resources when its snapshot is created.
#[derive(Clone, Copy, Debug)]
pub struct DaytonaResources {
    pub cpu_cores: u32,
    /// Requested memory in MiB. The driver's effective allocation must
    /// meet the runner minimum; Daytona rounds up to whole GiB.
    pub memory_mb: u64,
    /// `None` lets Daytona choose the disk allocation when building the
    /// snapshot. Explicit allocations must be positive; Daytona validates
    /// whether the image fits and the account allows the requested size.
    pub disk_mb:   Option<u64>,
}

impl Default for DaytonaResources {
    fn default() -> Self {
        Self {
            cpu_cores: 2,
            memory_mb: 4096,
            disk_mb:   None,
        }
    }
}

impl DaytonaResources {
    pub(crate) fn validated(self) -> Result<Resources, EnvError> {
        if self.cpu_cores < 2 || self.memory_mb < 4096 || self.disk_mb == Some(0) {
            return Err(EnvError::backend(
                "daytona",
                "configure",
                "the Daytona runner needs at least 2 CPUs and 4096 MiB of memory; an explicit disk allocation must be positive",
            ));
        }
        Ok(Resources {
            cpu_cores: NonZeroU32::new(self.cpu_cores),
            memory_mb: NonZeroU64::new(self.memory_mb),
            disk_mb:   self.disk_mb.and_then(NonZeroU64::new),
            gpus:      None,
        })
    }
}

impl SandboxOptions {
    /// The environment a scope with this runtime is placed on.
    pub(crate) fn environment_for(&self, runtime: &RuntimeSpec) -> &str {
        match runtime.target {
            ir::RuntimeTarget::HostProcess => self
                .process_environment
                .as_deref()
                .unwrap_or_else(|| self.backend_environment()),
            ir::RuntimeTarget::Container { .. } => self.container_environment(),
        }
    }

    /// The environment container scopes and action hosts are placed on.
    pub(crate) fn container_environment(&self) -> &str {
        self.container_environment
            .as_deref()
            .unwrap_or(match self.backend {
                SandboxBackend::Daytona => "daytona",
                SandboxBackend::Host | SandboxBackend::Docker => "docker",
            })
    }

    fn backend_environment(&self) -> &str {
        match self.backend {
            SandboxBackend::Host => "host",
            SandboxBackend::Docker => "docker",
            SandboxBackend::Daytona => "daytona",
        }
    }

    /// The pinned runner image a backend's built-in environment starts from.
    pub(crate) fn default_runner_image(backend: SandboxBackend) -> String {
        let options = Self {
            backend,
            ..Self::default()
        };
        options
            .default_image(DEFAULT_LABEL)
            .expect("the default label has a pinned image on every image backend")
    }

    /// The image a process scope on an image environment runs, when its
    /// placement labels choose one other than the environment's own: a
    /// `--runner-image` override, or, with no catalog file, the backend's
    /// pinned image for that label. `None` keeps the environment's image.
    pub(crate) fn runner_image(&self, runtime: &RuntimeSpec) -> Result<Option<String>, EnvError> {
        let backend = self.backend.to_string();
        let mut selected: Option<Option<String>> = None;
        let labels = runtime
            .requirements
            .iter()
            .map(smol_str::SmolStr::as_str)
            .chain(runtime.requirements.is_empty().then_some(DEFAULT_LABEL));
        for label in labels {
            let overridden = self
                .runner_images
                .get(label)
                .filter(|image| !image.trim().is_empty())
                .cloned();
            let image = match overridden {
                Some(image) => Some(image),
                None if self.catalog.is_none() => {
                    Some(self.default_image(label).ok_or_else(|| {
                        EnvError::backend(
                            &backend,
                            "configure",
                            format!(
                                "no runner image for label {label:?}; configure --runner-image \
                             {label}=IMAGE"
                            ),
                        )
                    })?)
                }
                // A catalog's environment is the default label's image.
                None if matches!(label, "ubuntu-latest" | DEFAULT_LABEL) => None,
                None => {
                    return Err(EnvError::backend(
                        &backend,
                        "configure",
                        format!(
                            "no runner image for label {label:?}; configure --runner-image \
                             {label}=IMAGE"
                        ),
                    ));
                }
            };
            if selected.as_ref().is_some_and(|previous| previous != &image) {
                return Err(EnvError::backend(
                    &backend,
                    "configure",
                    "placement labels select different runner images",
                ));
            }
            selected = Some(image);
        }
        let image = selected.expect("the default supplies a label for empty requirements");
        let pinned = self
            .catalog
            .is_none()
            .then(|| self.default_image(DEFAULT_LABEL))
            .flatten();
        Ok(image.filter(|image| Some(image) != pinned.as_ref()))
    }

    fn default_image(&self, label: &str) -> Option<String> {
        let version = match label {
            "ubuntu-latest" | "ubuntu-24.04" => "24.04",
            "ubuntu-22.04" if self.backend != SandboxBackend::Daytona => "22.04",
            "ubuntu-26.04" if self.backend != SandboxBackend::Daytona => "26.04",
            _ => return None,
        };
        let flavor = if self.backend == SandboxBackend::Daytona {
            "dind"
        } else {
            "slim"
        };
        Some(format!(
            "ghcr.io/lithoscomputer/ubuntu-{version}:{flavor}-{RUNNER_PIN}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use ir::RuntimeTarget;

    use super::*;

    fn runtime(labels: &[&str]) -> RuntimeSpec {
        RuntimeSpec {
            target:       RuntimeTarget::HostProcess,
            requirements: labels.iter().map(|label| (*label).into()).collect(),
        }
    }

    #[test]
    fn daytona_uses_only_available_pinned_dind_images_unless_overridden() {
        let mut options = SandboxOptions {
            backend: SandboxBackend::Daytona,
            ..Default::default()
        };
        // The default label keeps the environment's own pinned image.
        assert_eq!(options.runner_image(&runtime(&[])).unwrap(), None);
        assert_eq!(
            SandboxOptions::default_runner_image(SandboxBackend::Daytona),
            format!("ghcr.io/lithoscomputer/ubuntu-24.04:dind-{RUNNER_PIN}")
        );
        assert!(options.runner_image(&runtime(&["ubuntu-22.04"])).is_err());
        options.runner_images.insert(
            "ubuntu-22.04".to_owned(),
            "custom:22-dind-pinned".to_owned(),
        );
        assert_eq!(
            options.runner_image(&runtime(&["ubuntu-22.04"])).unwrap(),
            Some("custom:22-dind-pinned".to_owned())
        );
        assert!(
            options
                .runner_image(&runtime(&["ubuntu-22.04", "ubuntu-24.04"]))
                .is_err()
        );
    }

    #[test]
    fn docker_maps_supported_labels_and_rejects_unknown_placement() {
        let options = SandboxOptions {
            backend: SandboxBackend::Docker,
            ..Default::default()
        };
        assert_eq!(
            options.runner_image(&runtime(&["ubuntu-22.04"])).unwrap(),
            Some(format!(
                "ghcr.io/lithoscomputer/ubuntu-22.04:slim-{RUNNER_PIN}"
            ))
        );
        assert!(options.runner_image(&runtime(&["custom-runner"])).is_err());
    }

    #[test]
    fn a_catalog_keeps_its_environment_image_for_the_default_label_only() {
        let mut options = SandboxOptions {
            backend: SandboxBackend::Docker,
            catalog: Some("catalog.toml".into()),
            ..Default::default()
        };
        assert_eq!(options.runner_image(&runtime(&[])).unwrap(), None);
        assert_eq!(
            options.runner_image(&runtime(&["ubuntu-latest"])).unwrap(),
            None
        );
        // A catalog has no pinned images of Petri's for other labels.
        assert!(options.runner_image(&runtime(&["ubuntu-22.04"])).is_err());
        options
            .runner_images
            .insert("ubuntu-22.04".to_owned(), "custom:22".to_owned());
        assert_eq!(
            options.runner_image(&runtime(&["ubuntu-22.04"])).unwrap(),
            Some("custom:22".to_owned())
        );
    }

    #[test]
    fn daytona_rejects_resources_below_the_dind_minimum() {
        assert!(DaytonaResources::default().validated().is_ok());
        assert!(
            DaytonaResources {
                cpu_cores: 1,
                ..Default::default()
            }
            .validated()
            .is_err()
        );
        assert!(
            DaytonaResources {
                memory_mb: 2048,
                ..Default::default()
            }
            .validated()
            .is_err()
        );
        assert!(
            DaytonaResources {
                disk_mb: Some(0),
                ..Default::default()
            }
            .validated()
            .is_err()
        );
        for disk in [1, 1024, 3073, 4096, 20 * 1024] {
            let resources = DaytonaResources {
                disk_mb: Some(disk),
                ..Default::default()
            }
            .validated()
            .unwrap();
            assert_eq!(resources.disk_mb.map(NonZeroU64::get), Some(disk));
        }
    }

    #[test]
    fn daytona_defaults_use_a_container_with_provider_selected_disk() {
        let options = SandboxOptions {
            backend: SandboxBackend::Daytona,
            ..Default::default()
        };
        assert_eq!(options.daytona_kind, DaytonaSandboxKind::Container);
        let resources = options.daytona_resources.validated().unwrap();
        assert_eq!(resources.cpu_cores.map(NonZeroU32::get), Some(2));
        assert_eq!(resources.memory_mb.map(NonZeroU64::get), Some(4096));
        assert_eq!(resources.disk_mb, None);
    }

    #[test]
    fn daytona_validates_requested_memory_without_rounding() {
        for memory_mb in [0, 1, 2048, 3072, 3073, 3815, 4095] {
            assert!(
                DaytonaResources {
                    memory_mb,
                    ..Default::default()
                }
                .validated()
                .is_err()
            );
        }
        for memory_mb in [4096, 4097, 8191] {
            let resources = DaytonaResources {
                memory_mb,
                ..Default::default()
            }
            .validated()
            .unwrap();
            assert_eq!(resources.memory_mb.map(NonZeroU64::get), Some(memory_mb));
            assert_eq!(resources.disk_mb, None);
        }
    }
}
