//! Executor decoration must preserve the standard router's ownership and the
//! underlying handle's cleanup record.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use executor::{
    AcquireContext, EnvError, EnvHandle, Executor, ReleaseReport, ScopeOutcome, ScopeSpec,
};
use runtime::frontend::native;
use runtime::ir::RunStatus;
use runtime::{RunOptions, Runtime};
use testkit::RunDir;

struct RecordingLayer {
    inner: Arc<dyn Executor>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    name:  &'static str,
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
        let env = handle.exec();
        Ok(handle.with_exec(env))
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
    let mut options = RunOptions::new(dir.path());
    options.retention = executor::Retention::Never;
    let mut runtime = Runtime::standard().options(options);
    for name in ["inner", "outer"] {
        let calls = calls.clone();
        runtime = runtime.executor_layer(move |inner| {
            Arc::new(RecordingLayer {
                inner,
                calls: calls.clone(),
                name,
            })
        });
    }
    assert!(
        runtime.prepare_run(dir.path()).sandbox_router().is_some(),
        "decoration keeps the router for lease reconciliation"
    );
    let graph = native::load("test.yml", "nodes:\n  one:\n    run: echo ok\n")
        .graph
        .expect("graph");
    let report = runtime.run(graph).await.expect("run");
    assert_eq!(report.status, RunStatus::Success);
    assert_eq!(*calls.lock().expect("calls"), [
        "outer", "inner", "inner", "outer"
    ]);
    assert!(!dir.path().join("workspaces").join("default").exists());
}
