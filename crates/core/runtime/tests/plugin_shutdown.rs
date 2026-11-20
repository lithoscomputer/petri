//! Finishing one run closes its plugins without ending another run's provider.

use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use executor::{AcquireContext, ExecEnv, Executor as _, Retention, ScopeOutcome, ScopeSpec};
use ir::{GraphBuilder, Outcome, RunStatus, ScopeId, Value};
use runtime::steps::{Step, StepCtx};
use runtime::{RunOptions, Runtime};
use testkit::RunDir;

struct CaptureEnvironment(Arc<OnceLock<Arc<dyn ExecEnv>>>);

#[test]
fn a_rejected_resume_does_not_provision_run_services() {
    let directory = RunDir::new("rejected-resume-services");
    let runtime = Runtime::standard()
        .options(RunOptions::new(directory.path()))
        .run_services(|_, _| panic!("a rejected resume must not start run services"));
    // No external start can reproduce this core record.
    let log = engine::EventLog::try_from_records(engine::LOG_VERSION, vec![engine::EventRecord {
        seq:    0,
        origin: engine::EventOrigin::Core,
        event:  engine::Event::ExecutionStarted {
            start: engine::EngineStart::default(),
        },
    }])
    .expect("structurally valid log");
    assert!(
        runtime
            .resume_driver(GraphBuilder::new().build(), log)
            .is_err()
    );
}

#[async_trait::async_trait]
impl Step for CaptureEnvironment {
    const NAME: &'static str = "test/capture-environment";
    type Config = ();

    async fn run(&self, (): (), ctx: StepCtx) -> Outcome {
        ctx.env
            .write_file(Path::new("retained"), b"kept")
            .await
            .expect("retained file");
        assert!(self.0.set(ctx.env).is_ok());
        Outcome::success(Value::Null)
    }
}

#[tokio::test]
async fn standalone_and_resumed_drivers_close_their_plugins_before_returning() {
    for resumed in [false, true] {
        let directory = RunDir::new("standalone-plugin-finish");
        let mut options = RunOptions::new(directory.path());
        options.retention = Retention::Always;
        let environment = Arc::new(OnceLock::new());
        let runtime = Runtime::standard()
            .step(CaptureEnvironment(environment.clone()))
            .options(options);
        let mut graph = GraphBuilder::new();
        graph.add_step("capture", ScopeId::new(0), CaptureEnvironment::NAME);
        let graph = graph.build();
        let report = if resumed {
            runtime
                .resume_driver(graph, engine::EventLog::new())
                .expect("resumed driver")
                .0
                .run()
                .await
        } else {
            runtime.run(graph).await.expect("standalone run")
        };
        assert_eq!(report.status, RunStatus::Success);
        assert!(
            environment
                .get()
                .expect("captured environment")
                .read_file(Path::new("retained"))
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(directory.workspace().join("retained")).expect("retained file"),
            b"kept"
        );
    }
}

#[tokio::test]
async fn run_finish_closes_its_plugin_and_preserves_other_runs_and_retained_files() {
    let first_dir = RunDir::new("plugin-finish-first");
    let second_dir = RunDir::new("plugin-finish-second");
    let mut options = RunOptions::new(first_dir.path());
    options.retention = Retention::Always;
    let runtime = Runtime::standard().options(options);
    let first = runtime.prepare_run(first_dir.path());
    let second = runtime.prepare_run(second_dir.path());
    let first_router = first.sandbox_router().expect("standard router").clone();
    let second_router = second.sandbox_router().expect("standard router").clone();
    let scope = ScopeSpec::new(ScopeId::new(0), "scope-0");
    let first_env = first_router
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("first host scope");
    let second_env = second_router
        .acquire(&scope, &AcquireContext::bare())
        .await
        .expect("second host scope");
    let first_exec = first_env.exec();
    first_exec
        .write_file(Path::new("retained"), b"kept")
        .await
        .expect("retained data");
    assert!(
        first_router
            .release(first_env, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    assert_eq!(
        first_exec
            .read_file(Path::new("retained"))
            .await
            .expect("live plugin"),
        Some(b"kept".to_vec())
    );

    first.finish().await;

    // Keep the old router and environment references alive: drop alone cannot
    // close this transport. The run must explicitly await plugin shutdown.
    assert!(first_exec.read_file(Path::new("retained")).await.is_err());
    assert_eq!(
        fs::read(first_dir.workspace().join("retained")).expect("retained workspace"),
        b"kept"
    );
    second_env
        .exec()
        .write_file(Path::new("still-live"), b"yes")
        .await
        .expect("other run remains live");
    assert!(
        second_router
            .release(second_env, ScopeOutcome::Succeeded)
            .await
            .is_clean()
    );
    second.finish().await;
}
