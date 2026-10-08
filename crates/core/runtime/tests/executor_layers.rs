//! Executor decoration must preserve the standard router's ownership and the
//! underlying handle's cleanup record, and reach the processes the scope
//! starts without changing what the environment answers.
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, ScopeOutcome, ScopeSpec,
    SpawnEnv, SpawnTarget,
};
use runtime::frontend::native;
use runtime::ir::RunStatus;
use runtime::{RunOptions, Runtime};
use smol_str::SmolStr;
use testkit::RunDir;

/// Adds `LAYER_<name>=<name>` to every process.
struct NamedVariable(&'static str);

#[async_trait]
impl SpawnEnv for NamedVariable {
    async fn apply(
        &self,
        target: SpawnTarget,
        env: &mut BTreeMap<SmolStr, SmolStr>,
    ) -> Result<(), EnvError> {
        // A native `run:` node starts a process in the scope.
        assert_eq!(target, SpawnTarget::Process);
        env.insert(
            SmolStr::new(format!("LAYER_{}", self.0)),
            SmolStr::new(self.0),
        );
        Ok(())
    }
}

struct RecordingLayer {
    inner:      Arc<dyn Executor>,
    calls:      Arc<Mutex<Vec<&'static str>>>,
    workspaces: Arc<Mutex<Vec<PathBuf>>>,
    name:       &'static str,
}

#[async_trait]
impl Executor for RecordingLayer {
    async fn acquire(
        &self,
        scope: &ScopeSpec,
        ctx: &AcquireContext,
    ) -> Result<EnvHandle, EnvError> {
        self.calls.lock().expect("calls").push(self.name);
        let handle = self.inner.acquire(scope, ctx).await?;
        let handle = handle.with_spawn_env(Arc::new(NamedVariable(self.name)));
        let env = handle.exec();
        // The layer keeps the executor's answers: a host scope still shares
        // the host filesystem.
        assert!(env.shares_host_filesystem());
        self.workspaces
            .lock()
            .expect("workspaces")
            .push(PathBuf::from(env.workspace_path()));
        Ok(handle)
    }
    async fn release(&self, handle: EnvHandle, outcome: ScopeOutcome) -> ReleaseReport {
        let report = self.inner.release(handle, outcome).await;
        assert!(report.is_clean(), "{report:?}");
        self.calls.lock().expect("calls").push(self.name);
        report
    }
}

#[tokio::test]
async fn layers_keep_the_router_and_release_the_original_environment() {
    let dir = RunDir::new("executor-layers");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let workspaces = Arc::new(Mutex::new(Vec::new()));
    let mut options = RunOptions::new(dir.path());
    options.retention = executor::Retention::Never;
    let mut runtime = Runtime::standard().options(options);
    for name in ["inner", "outer"] {
        let calls = calls.clone();
        let workspaces = workspaces.clone();
        runtime = runtime.executor_layer(move |inner| {
            Arc::new(RecordingLayer {
                inner,
                calls: calls.clone(),
                workspaces: workspaces.clone(),
                name,
            })
        });
    }
    // A separate run directory, finished before the run, so the check
    // provisions nothing the run below shares or leaks.
    let probe_dir = RunDir::new("executor-layers-probe");
    let probe = runtime.prepare_run(probe_dir.path());
    assert!(
        probe.sandbox_router().is_some(),
        "decoration keeps the router for lease reconciliation"
    );
    probe.finish().await;
    let record = dir.path().join("layers.txt");
    let script = format!(
        "nodes:\n  one:\n    run: printf '%s %s' \"$LAYER_inner\" \"$LAYER_outer\" > '{}'\n",
        record.display()
    );
    let graph = native::load("test.yml", &script).graph.expect("graph");
    let report = runtime.run(graph).await.expect("run");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );
    assert_eq!(*calls.lock().expect("calls"), [
        "outer", "inner", "inner", "outer"
    ]);
    assert_eq!(
        fs::read_to_string(&record).expect("record"),
        "inner outer",
        "every layer's variable reaches the process"
    );
    // Both layers saw the same host workspace, and releasing the original
    // handle removed it.
    let workspaces = workspaces.lock().expect("workspaces");
    assert_eq!(workspaces.len(), 2, "{workspaces:?}");
    assert_eq!(workspaces[0], workspaces[1]);
    assert!(
        workspaces[0].starts_with(dir.path().canonicalize().expect("run dir")),
        "{} is not under the run directory",
        workspaces[0].display()
    );
    assert!(
        !workspaces[0].exists(),
        "{} survived release",
        workspaces[0].display()
    );
}
