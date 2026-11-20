//! Durable runs over an application's own sandbox catalog: leases are
//! recorded, retained, and pruned through the same records the standard
//! catalog writes, and a run that cannot start closes what it opened.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use execution::prune::prune;
use execution::{Coordinator, CoordinatorOptions, LeaseState, ResourceStore};
use executor::{AcquireContext, Executor as _, Retention, ScopeSpec};
use ir::{GraphBuilder, RunStatus, ScopeId, StepRef};
use runtime::{RunOptions, Runtime};
use testkit::RunDir;
use testkit::in_process::host_providers;

fn retained_on_failure(directory: &RunDir) -> RunOptions {
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::OnFailure;
    options
}

/// Runs one failing Host step under a coordinator on `runtime`, and returns
/// the retained workspace it wrote its proof into.
async fn fail_and_retain(runtime: &Runtime, directory: &RunDir) -> PathBuf {
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("coordinator");
    let mut graph = GraphBuilder::new();
    graph.add_node(
        "fail",
        ScopeId::new(0),
        StepRef::new(
            "process",
            serde_json::json!({ "run": "echo retained > proof; exit 1", "shell": "sh" }),
        ),
    );
    let graph = coordinator
        .register_graph(&graph.build())
        .await
        .expect("graph");
    let result = coordinator
        .run_root(graph, BTreeMap::new())
        .await
        .expect("run");
    coordinator.finish().await;
    assert_eq!(result.status, RunStatus::Failed);
    let store = ResourceStore::load(&testkit::read_run_dir(directory.path()).await)
        .await
        .expect("resources");
    let record = store.records().next().expect("host lease");
    assert_eq!(record.provider.as_str(), "host");
    assert_eq!(record.state, LeaseState::Stopped);
    let workspace = directory
        .path()
        .join("scopes")
        .join(record.workspace.as_str())
        .join("work");
    assert_eq!(
        fs::read_to_string(workspace.join("proof")).expect("retained proof"),
        "retained\n"
    );
    workspace
}

async fn assert_pruned(runtime: &Runtime, directory: &RunDir, workspace: &Path) {
    let report = prune(runtime).await.expect("prune");
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.deleted.len(), 1, "{report:?}");
    assert!(
        !workspace.exists(),
        "provider deletion removes its managed workspace"
    );
    let store = ResourceStore::load(&testkit::read_run_dir(directory.path()).await)
        .await
        .expect("resources");
    assert_eq!(
        store.records().next().expect("tombstone").state,
        LeaseState::Deleted
    );
}

#[tokio::test]
async fn a_retained_host_workspace_is_pruned_by_a_fresh_runtime_with_the_same_providers() {
    let directory = RunDir::new("in-process-retain-prune");
    let options = retained_on_failure(&directory);
    let (providers, host, docker) = host_providers();
    let runtime = Runtime::standard()
        .options(options.clone())
        .sandbox_catalog(providers.clone());
    let workspace = fail_and_retain(&runtime, &directory).await;
    assert_eq!(host.connects(), 1);

    // Prune runs later, in a runtime of its own, over the same recipe.
    let fresh = Runtime::bare().options(options).sandbox_catalog(providers);
    assert_pruned(&fresh, &directory, &workspace).await;
    assert_eq!(host.connects(), 2, "prune opened the run's registry again");
    assert_eq!(docker.connects(), 0, "no action host was recorded");
}

/// A lease the standard catalog recorded names its `host` entry, so a
/// worker with its own catalog over the same entry name still prunes it.
#[tokio::test]
async fn a_lease_the_standard_catalog_recorded_is_pruned_through_an_application_catalog() {
    let directory = RunDir::new("plugin-then-in-process-prune");
    let options = retained_on_failure(&directory);
    let standard = Runtime::standard().options(options.clone());
    let workspace = fail_and_retain(&standard, &directory).await;

    let (providers, host, _) = host_providers();
    let fresh = Runtime::bare().options(options).sandbox_catalog(providers);
    assert_pruned(&fresh, &directory, &workspace).await;
    assert_eq!(host.connects(), 1);
}

#[tokio::test]
async fn a_coordinator_that_cannot_start_closes_its_run() {
    for resume in [false, true] {
        let directory = RunDir::new("in-process-refused-start");
        let (providers, ..) = host_providers();
        let runtime = Runtime::standard()
            .options(retained_on_failure(&directory))
            .sandbox_catalog(providers);
        let run = runtime.prepare_run(directory.path());
        let router = run.sandbox_router().expect("standard router").clone();
        let env = router
            .acquire(
                &ScopeSpec::new(ScopeId::new(0), "scope-0"),
                &AcquireContext::bare(),
            )
            .await
            .expect("host scope");
        let exec = env.exec();
        exec.write_file(Path::new("probe"), b"open")
            .await
            .expect("the run is open");
        let refused = if resume {
            // Nothing was ever created here, so there is nothing to resume.
            Coordinator::resume(run, Vec::new(), CoordinatorOptions::default())
                .await
                .err()
        } else {
            let options = CoordinatorOptions {
                max_invocations: 0,
                ..CoordinatorOptions::default()
            };
            Coordinator::create(run, Vec::new(), options).await.err()
        };
        assert!(refused.is_some(), "the coordinator refuses to start");
        let error = exec
            .read_file(Path::new("probe"))
            .await
            .expect_err("the refused run was closed");
        assert!(error.to_string().contains("finished"), "{error}");
        drop(env);
    }
}
