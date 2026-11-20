use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use execution::host::EVENTS_FILE;
use execution::prune::prune;
use execution::{
    CallSite, Coordinator, CoordinatorInvocationClient, CoordinatorOptions, GraphDigest,
    InvocationClient as _, InvocationId, InvocationRequest, ResourceLedger, ResourceStore,
    SandboxAllocationKey, SandboxMode, SecretBindings,
};
use executor::{
    AcquireContext, Executor as _, OneShotContainer, Retention, ScopeOutcome, ScopeSpec,
};
use executor_sandbox::{SandboxCatalog, SandboxOptions};
use ir::{GraphBuilder, Outcome, RunStatus, RuntimeSpec, Scope, ScopeId, StepRef};
use runtime::steps::{Step, StepCtx};
use runtime::store::{RunKey, RunStore as _};
use runtime::{RunOptions, Runtime};
use testkit::{RunDir, container_id, container_is_running, is_docker_ready, sandbox_name};
use tokio::sync::Mutex;
use tokio::time::timeout;

struct HandleChildFailure;

#[async_trait::async_trait]
impl Step for HandleChildFailure {
    const NAME: &'static str = "test/handle-child-failure";
    type Config = GraphDigest;

    async fn run(&self, graph: GraphDigest, ctx: StepCtx) -> Outcome {
        let client = ctx
            .require_capability::<CoordinatorInvocationClient>()
            .expect("coordinator client");
        let mut child = client
            .start_or_attach(InvocationRequest {
                site: CallSite {
                    firing:  ctx.firing,
                    attempt: ctx.attempt,
                    slot:    "child".into(),
                },
                graph,
                context: BTreeMap::new(),
                secrets: SecretBindings::None,
                sandbox: SandboxMode::Isolated,
                admission: None,
            })
            .await
            .expect("child starts");
        assert_eq!(child.result().await.status, RunStatus::Failed);
        Outcome::success(serde_json::Value::Null)
    }
}

#[tokio::test]
async fn successful_run_keeps_a_failed_childs_retained_sandbox() {
    if !is_docker_ready().await {
        return;
    }
    check_failed_child_retention(RuntimeSpec::container("alpine:3.20")).await;
}

#[tokio::test]
async fn successful_run_keeps_a_failed_host_childs_workspace() {
    check_failed_child_retention(RuntimeSpec::default()).await;
}

async fn check_failed_child_retention(runtime_spec: RuntimeSpec) {
    let host = matches!(runtime_spec.target, ir::RuntimeTarget::HostProcess);
    let directory = RunDir::new("coordinator-child-retention");
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::OnFailure;
    let runtime = Runtime::standard()
        .step(HandleChildFailure)
        .options(options);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("coordinator starts");
    let mut child = GraphBuilder::bare();
    let mut scope = Scope::new(ScopeId::new(0));
    scope.runtime = runtime_spec;
    let scope = child.add_scope(scope);
    child.add_node(
        "fail",
        scope,
        StepRef::new(
            "process",
            serde_json::json!({ "run": "echo retained > proof; exit 1", "shell": "sh" }),
        ),
    );
    let child = coordinator
        .register_graph(&child.build())
        .await
        .expect("child graph");
    let mut parent = GraphBuilder::new();
    parent.add_node(
        "handle-failure",
        ScopeId::new(0),
        StepRef::new(HandleChildFailure::NAME, serde_json::json!(child)),
    );
    let parent = coordinator
        .register_graph(&parent.build())
        .await
        .expect("parent graph");
    let result = coordinator
        .run_root(parent, BTreeMap::new())
        .await
        .expect("run");
    assert_eq!(result.status, RunStatus::Success);
    let store = execution::ResourceStore::load(&testkit::read_run_dir(directory.path()).await)
        .await
        .expect("resources");
    let child = store
        .records()
        .find(|record| record.allocation.invocation != execution::InvocationId::ROOT)
        .expect("child lease");
    assert_eq!(child.state, execution::LeaseState::Stopped);
    let workspace = directory
        .path()
        .join("scopes")
        .join(child.workspace.as_str())
        .join("work");
    let sandbox = sandbox_name(directory.path(), child.lease.raw());
    let retained = if host {
        None
    } else {
        container_id(&sandbox).await
    };
    coordinator.finish().await;
    if host {
        assert_eq!(
            fs::read_to_string(workspace.join("proof")).expect("retained child workspace"),
            "retained\n"
        );
    } else {
        assert!(retained.is_some(), "child sandbox retained");
        assert_eq!(container_id(&sandbox).await, retained);
        assert!(!container_is_running(&sandbox).await);
    }
    let report = prune(&runtime).await.expect("cleanup retained sandbox");
    assert!(report.is_clean(), "{report:?}");
}

struct HostAction;

#[async_trait::async_trait]
impl Step for HostAction {
    const NAME: &'static str = "test/host-action";
    type Config = ();

    async fn run(&self, (): (), ctx: StepCtx) -> Outcome {
        let runner = ctx.require_container_runner().expect("host runner");
        let process = runner
            .run(OneShotContainer::registry("alpine:3.20").with_args(&["true"]))
            .await;
        let mut process = match process {
            Ok(process) => process,
            Err(error) => return Outcome::failure(error.to_string()),
        };
        let mut lines = process.lines().expect("action output");
        while lines.recv().await.is_some() {}
        assert!(process.wait().await.expect("action finishes").is_success());
        Outcome::success(serde_json::Value::Null)
    }
}

#[tokio::test]
async fn a_refused_host_action_finishes_its_invocation_without_a_sandbox() {
    if !is_docker_ready().await {
        return;
    }
    let directory = RunDir::new("coordinator-host-action-release");
    let runtime = Runtime::standard()
        .step(HostAction)
        .options(RunOptions::new(directory.path()));
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("coordinator starts");
    let mut graph = GraphBuilder::new();
    graph.add_step("action", ScopeId::new(0), HostAction::NAME);
    let graph = coordinator
        .register_graph(&graph.build())
        .await
        .expect("graph");
    let result = timeout(
        Duration::from_secs(5),
        coordinator.run_root(graph, BTreeMap::new()),
    )
    .await
    .expect("a refused action must not strand the invocation")
    .expect("run");
    assert_eq!(result.status, RunStatus::Failed);
    let prefix = format!("petri-{}-", testkit::recorded_run_id(directory.path()));
    assert!(testkit::list_containers(&prefix).await.is_empty());
    coordinator.finish().await;
}

#[tokio::test]
async fn an_execution_restart_keeps_the_container_identity_and_workspace() {
    if !is_docker_ready().await {
        return;
    }
    let directory = RunDir::new("coordinator-container-restart");
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::Always;
    let runtime = Runtime::standard().options(options);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    let mut graph = GraphBuilder::bare();
    let mut scope = Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container("alpine:3.20");
    let scope = graph.add_scope(scope);
    let start = graph.add_node(
        "before",
        scope,
        StepRef::new(
            "process",
            serde_json::json!({
                "run": "printf '%s' \"$HOSTNAME\" > before", "shell": "sh",
            }),
        ),
    );
    let after = graph.add_node(
        "after",
        scope,
        StepRef::new(
            "process",
            serde_json::json!({
                "run": "test \"$(cat before)\" = \"$HOSTNAME\"", "shell": "sh",
            }),
        ),
    );
    graph.mark_entry(start);
    graph.link(start, after);
    graph.node_mut(start).routing.groups[0].arms[0].transition = ir::EdgeTransition::Restart;
    let graph = coordinator.register_graph(&graph.build()).await.unwrap();
    let result = timeout(
        Duration::from_secs(60),
        coordinator.run_root(graph, BTreeMap::new()),
    )
    .await
    .unwrap_or_else(|_| {
        let logs = [0, 1].map(|index| {
            let path = coordinator
                .execution_dir(execution::ExecutionId::new(index))
                .join(EVENTS_FILE);
            fs::read_to_string(path)
        });
        panic!(
            "restart did not finish; coordinator: {:?}; execution logs: {logs:?}",
            coordinator.store().state()
        )
    })
    .unwrap();
    assert_eq!(result.status, RunStatus::Success);
    assert_eq!(result.final_execution.raw(), 1);
    let records = execution::ResourceStore::load(&testkit::read_run_dir(directory.path()).await)
        .await
        .unwrap();
    assert_eq!(
        records.records().count(),
        1,
        "both executions share one lease"
    );
    timeout(Duration::from_secs(60), coordinator.finish())
        .await
        .expect("coordinator shutdown did not finish");
    assert!(
        timeout(Duration::from_secs(60), prune(&runtime))
            .await
            .expect("retained sandbox prune did not finish")
            .unwrap()
            .is_clean()
    );
}

#[tokio::test]
async fn a_failed_host_scope_is_retained_and_pruned_through_a_fresh_plugin() {
    let directory = RunDir::new("host-retention-prune");
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::OnFailure;
    let runtime = Runtime::standard().options(options.clone());
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
    let store = execution::ResourceStore::load(&testkit::read_run_dir(directory.path()).await)
        .await
        .expect("resources");
    let record = store.records().next().expect("host lease");
    assert_eq!(record.provider.as_str(), "host");
    assert_eq!(record.state, execution::LeaseState::Stopped);
    assert!(
        record
            .resource_id
            .as_ref()
            .expect("provider id")
            .starts_with("host-")
    );
    let workspace = directory
        .path()
        .join("scopes")
        .join(record.workspace.as_str())
        .join("work");
    assert_eq!(
        fs::read_to_string(workspace.join("proof")).expect("retained proof"),
        "retained\n"
    );
    let fresh = Runtime::standard().options(options);
    let report = prune(&fresh)
        .await
        .expect("prune through a new Host plugin");
    assert!(report.problems.is_empty(), "{report:?}");
    assert_eq!(report.deleted.len(), 1);
    assert!(
        !workspace.exists(),
        "provider deletion removes its managed workspace"
    );
    let store = execution::ResourceStore::load(&testkit::read_run_dir(directory.path()).await)
        .await
        .expect("resources");
    assert_eq!(
        store.records().next().expect("tombstone").state,
        execution::LeaseState::Deleted
    );
}

#[tokio::test]
async fn prune_removes_crashed_host_action_containers_including_tombstoned_leases() {
    if !is_docker_ready().await {
        return;
    }
    for tombstoned in [false, true] {
        let directory = RunDir::new("host-action-prune");
        let key = RunKey::new(directory.run_id());
        // The run manifest: a run declaration under a writer this test
        // holds, released before prune takes the run.
        let logs = execution::RunDirStore::new(directory.path())
            .open(&key, execution::Access::Create {
                owner: execution::OwnerId::mint(),
            })
            .await
            .expect("the run is created");
        drop(
            execution::CoordinatorStore::create(logs.clone(), key, Vec::new())
                .await
                .expect("run manifest"),
        );
        let runtime = Runtime::standard().options(RunOptions::new(directory.path()));
        let scope = ScopeSpec::new(ScopeId::new(0), "scope-0");
        let mut resources = ResourceStore::load(&logs).await.expect("resource store");
        let lease = resources
            .ensure_record(
                SandboxAllocationKey {
                    invocation: InvocationId::ROOT,
                    scope:      engine::ScopeIdentity::Declared(scope.id),
                },
                "host",
                scope.workspace_id.clone(),
                scope.runtime.clone(),
                None,
            )
            .await
            .expect("host reservation")
            .lease;
        let ledger = Arc::new(ResourceLedger::new(Arc::new(Mutex::new(resources))));
        let router = runtime
            .sandbox_router_for(directory.path())
            .expect("router");
        router.set_ledger(ledger.clone());
        let handle = router
            .acquire(&scope, &AcquireContext::bare().with_lease(lease))
            .await
            .expect("host scope");
        let prefix = router.container_prefix();
        // Seed an old action host through the shared API: new Host actions
        // are refused, but recovery must still delete their predecessors.
        let sandboxes = SandboxCatalog::from_options(&SandboxOptions::default())
            .expect("catalog")
            .open()
            .expect("open catalog");
        let mut spec = sandboxes
            .environment("docker")
            .await
            .expect("environment")
            .spec;
        spec.name = Some(format!("{prefix}a-scope-0"));
        spec.labels
            .insert("petri.run".to_owned(), directory.run_id());
        spec.labels.insert(
            "petri.actions".to_owned(),
            format!("{}/{}", directory.run_id(), scope.workspace_id),
        );
        let sandbox = sandboxes
            .builder("docker")
            .await
            .expect("builder")
            .spec(spec)
            .create()
            .await
            .expect("seed old action host");
        let reference = sandbox.reference();
        let marker =
            serde_json::json!({"provider": reference.provider, "namespace": reference.namespace});
        fs::write(
            directory
                .path()
                .join("scopes")
                .join(scope.workspace_id.as_str())
                .join("action-host"),
            serde_json::to_vec(&marker).expect("marker serializes"),
        )
        .expect("old action marker");
        sandboxes.shutdown().await.expect("catalog shuts down");
        assert_eq!(testkit::list_containers(&prefix).await.len(), 1);
        // A fresh router has only durable records, just as after a crash.
        router.shutdown().await;
        drop(handle);
        drop(router);
        if tombstoned {
            let cleanup = runtime
                .sandbox_router_for(directory.path())
                .expect("cleanup router");
            cleanup.set_ledger(ledger);
            assert!(
                cleanup
                    .release_lease(lease, ScopeOutcome::Succeeded)
                    .await
                    .is_clean()
            );
            cleanup.shutdown().await;
            drop(cleanup);
        } else {
            drop(ledger);
        }
        // The test's own writer handle goes before prune takes the run.
        drop(logs);

        let report = prune(&runtime).await.expect("prune");
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.deleted.len(), 1);
        assert!(testkit::list_containers(&prefix).await.is_empty());
        let resources = ResourceStore::load(&testkit::read_run_dir(directory.path()).await)
            .await
            .expect("persisted tombstone");
        assert_eq!(
            resources.resolve(lease).unwrap().state,
            execution::LeaseState::Deleted
        );
        assert!(
            !directory
                .path()
                .join("scopes")
                .join(scope.workspace_id.as_str())
                .join("action-host")
                .exists()
        );
    }
}
