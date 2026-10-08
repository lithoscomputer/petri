//! Environment added to a scope's processes at the moment they start.
//!
//! An embedding host that must put credentials into every process a scope
//! starts — renewable ones, minted fresh for each spawn — supplies a
//! [`SpawnEnv`]. [`crate::EnvHandle::with_spawn_env`] applies it to both ways a
//! scope starts work: [`ExecEnv::spawn`] and [`ContainerRunner::run`].
//! Everything else about the environment is the executor's, forwarded
//! unchanged.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use smol_str::SmolStr;

use crate::container::{ContainerRunner, OneShotContainer};
use crate::env::{DirectoryEntry, ExecEnv, PreviewUrl, ProcessHandle, ProcessSpec};
use crate::error::EnvError;

/// What a scope is about to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnTarget {
    /// A process in the scope's own environment, through [`ExecEnv::spawn`]:
    /// it sees the scope's filesystem, so a file a layer wrote there through
    /// the scope is a path it can name.
    Process,
    /// A one-shot container, through [`ContainerRunner::run`]: a filesystem
    /// of its own that shares only the workspace mount with the scope.
    Container,
}

/// Adjusts the environment of a process just before a scope starts it.
///
/// Called once per spawn, for a process and for a one-shot container alike,
/// so a value can be fresh each time. An error fails that spawn.
#[async_trait]
pub trait SpawnEnv: Send + Sync {
    /// Edit `env`, the environment its step built for what `target` names.
    /// The executor still puts the scope's ambient environment beneath it.
    async fn apply(
        &self,
        target: SpawnTarget,
        env: &mut BTreeMap<SmolStr, SmolStr>,
    ) -> Result<(), EnvError>;
}

/// `env` with `spawn_env` applied to every process it starts.
pub fn layer_exec(env: Arc<dyn ExecEnv>, spawn_env: Arc<dyn SpawnEnv>) -> Arc<dyn ExecEnv> {
    Arc::new(LayeredExec {
        inner: env,
        spawn_env,
    })
}

pub(crate) fn layer_runner(
    runner: Arc<dyn ContainerRunner>,
    spawn_env: Arc<dyn SpawnEnv>,
) -> Arc<dyn ContainerRunner> {
    Arc::new(LayeredRunner {
        inner: runner,
        spawn_env,
    })
}

struct LayeredExec {
    inner:     Arc<dyn ExecEnv>,
    spawn_env: Arc<dyn SpawnEnv>,
}

/// Forwards every method, the provided ones included, so the executor's own
/// answers survive the layer.
#[async_trait]
impl ExecEnv for LayeredExec {
    async fn spawn(&self, mut spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
        self.spawn_env
            .apply(SpawnTarget::Process, &mut spec.env)
            .await?;
        self.inner.spawn(spec).await
    }

    fn workspace_path(&self) -> &str {
        self.inner.workspace_path()
    }

    async fn read_file(&self, relative: &Path) -> Result<Option<Vec<u8>>, EnvError> {
        self.inner.read_file(relative).await
    }

    async fn read_file_limited(
        &self,
        relative: &Path,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, EnvError> {
        self.inner.read_file_limited(relative, limit).await
    }

    async fn write_file(&self, relative: &Path, contents: &[u8]) -> Result<(), EnvError> {
        self.inner.write_file(relative, contents).await
    }

    async fn list_directory(
        &self,
        path: &Path,
        depth: usize,
    ) -> Result<Vec<DirectoryEntry>, EnvError> {
        self.inner.list_directory(path, depth).await
    }

    fn grace(&self) -> Duration {
        self.inner.grace()
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        self.inner.host_address()
    }

    fn ambient_env(&self, name: &str) -> Option<String> {
        self.inner.ambient_env(name)
    }

    fn shares_host_filesystem(&self) -> bool {
        self.inner.shares_host_filesystem()
    }

    async fn preview_url(&self, port: u16) -> Result<Option<PreviewUrl>, EnvError> {
        self.inner.preview_url(port).await
    }

    async fn release_preview_url(&self, port: u16) -> Result<(), EnvError> {
        self.inner.release_preview_url(port).await
    }
}

struct LayeredRunner {
    inner:     Arc<dyn ContainerRunner>,
    spawn_env: Arc<dyn SpawnEnv>,
}

#[async_trait]
impl ContainerRunner for LayeredRunner {
    fn workspace_path(&self) -> &str {
        self.inner.workspace_path()
    }

    fn host_address(&self) -> Result<&str, EnvError> {
        self.inner.host_address()
    }

    async fn run(&self, mut spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
        self.spawn_env
            .apply(SpawnTarget::Container, &mut spec.env)
            .await?;
        self.inner.run(spec).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ir::{SandboxInstance, ScopeId};

    use super::*;
    use crate::scope::EnvHandle;

    /// Records the env of whatever it is asked to start, then refuses.
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<BTreeMap<SmolStr, SmolStr>>>,
    }

    impl Recorder {
        fn record(&self, env: BTreeMap<SmolStr, SmolStr>) -> EnvError {
            self.seen.lock().expect("seen").push(env);
            EnvError::backend("test", "spawn", "recorded")
        }
    }

    #[async_trait]
    impl ExecEnv for Recorder {
        async fn spawn(&self, spec: ProcessSpec) -> Result<Box<dyn ProcessHandle>, EnvError> {
            Err(self.record(spec.env))
        }

        fn workspace_path(&self) -> &'static str {
            "/work"
        }

        async fn read_file(&self, _: &Path) -> Result<Option<Vec<u8>>, EnvError> {
            Ok(None)
        }

        async fn write_file(&self, _: &Path, _: &[u8]) -> Result<(), EnvError> {
            Ok(())
        }

        fn grace(&self) -> Duration {
            Duration::from_secs(1)
        }

        fn ambient_env(&self, name: &str) -> Option<String> {
            (name == "HOME").then(|| "/home".into())
        }

        fn shares_host_filesystem(&self) -> bool {
            true
        }
    }

    #[async_trait]
    impl ContainerRunner for Recorder {
        fn workspace_path(&self) -> &'static str {
            "/mnt"
        }

        fn host_address(&self) -> Result<&str, EnvError> {
            Ok("host.docker.internal")
        }

        async fn run(&self, spec: OneShotContainer) -> Result<Box<dyn ProcessHandle>, EnvError> {
            Err(self.record(spec.env))
        }
    }

    /// A fresh token for anything, and a store path only a process in the
    /// scope can read.
    struct Token;

    #[async_trait]
    impl SpawnEnv for Token {
        async fn apply(
            &self,
            target: SpawnTarget,
            env: &mut BTreeMap<SmolStr, SmolStr>,
        ) -> Result<(), EnvError> {
            env.entry("TOKEN".into()).or_insert_with(|| "fresh".into());
            if target == SpawnTarget::Process {
                env.insert("STORE".into(), "/tmp/store".into());
            }
            Ok(())
        }
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<SmolStr, SmolStr> {
        pairs
            .iter()
            .map(|(key, value)| (SmolStr::new(key), SmolStr::new(value)))
            .collect()
    }

    #[tokio::test]
    async fn the_layer_reaches_processes_and_containers_by_target_and_keeps_the_rest() {
        let exec = Arc::new(Recorder::default());
        let runner = Arc::new(Recorder::default());
        let sandbox = SandboxInstance {
            provider:          "host".into(),
            instance:          "scope-0".into(),
            image:             None,
            snapshot:          None,
            working_directory: "/work".into(),
        };
        let handle = EnvHandle::new(ScopeId::new(0), "scope-0".into(), sandbox, exec.clone(), ())
            .with_runner(runner.clone())
            .with_spawn_env(Arc::new(Token));

        let layered = handle.exec();
        assert!(layered.shares_host_filesystem());
        assert_eq!(layered.ambient_env("HOME").as_deref(), Some("/home"));
        assert_eq!(layered.workspace_path(), "/work");
        let spec = ProcessSpec::new("true", &[]).with_env(env(&[("TOKEN", "own")]));
        assert!(layered.spawn(spec).await.is_err());
        assert!(layered.spawn(ProcessSpec::new("true", &[])).await.is_err());
        assert_eq!(*exec.seen.lock().expect("seen"), [
            env(&[("STORE", "/tmp/store"), ("TOKEN", "own")]),
            env(&[("STORE", "/tmp/store"), ("TOKEN", "fresh")])
        ]);

        let containers = handle.container_runner().expect("runner");
        assert_eq!(containers.workspace_path(), "/mnt");
        assert!(
            containers
                .run(OneShotContainer::registry("alpine"))
                .await
                .is_err()
        );
        assert_eq!(*runner.seen.lock().expect("seen"), [env(&[(
            "TOKEN", "fresh"
        )])]);
    }
}
