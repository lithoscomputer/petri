use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use engine::{Event, ScopeIdentity};
use execution::prune::prune;
use execution::{
    CallSite, Coordinator, CoordinatorEvent, CoordinatorInvocationClient, CoordinatorOptions,
    ExecutionId, GraphDigest, InvocationClient as _, InvocationId, InvocationRequest, LeaseState,
    ResourceStore, SandboxMode, SecretBindings, read_coordinator_log, read_engine_log,
};
use executor::Retention;
use ir::{
    Arm, EdgeTransition, GraphBuilder, GraphFragment, JoinPolicy, Outcome, RunStatus, ScopeId,
    SplicePolicy, SpliceRequest, StepRef,
};
use runtime::{RunOptions, Runtime};
use steps::{Step, StepCtx};
use testkit::RunDir;
use tokio::sync::Notify;
use tokio::time::timeout;

struct Upload;

#[async_trait::async_trait]
impl Step for Upload {
    const NAME: &'static str = "test/upload";
    type Config = GraphFragment;

    async fn run(&self, fragment: GraphFragment, _ctx: StepCtx) -> Outcome {
        Outcome::success(()).with_splice(SpliceRequest::append(fragment))
    }
}

fn upload_graph(fragment: &GraphFragment) -> ir::Graph {
    let mut graph = GraphBuilder::new();
    let upload = graph.add_node(
        "upload",
        ScopeId::new(0),
        StepRef::new(Upload::NAME, serde_json::json!(fragment)),
    );
    graph.node_mut(upload).splice_policy = SplicePolicy::Append;
    graph.build()
}

async fn resources(directory: &RunDir) -> ResourceStore {
    ResourceStore::load(&testkit::read_run_dir(directory.path()).await)
        .await
        .expect("resource store")
}

fn retained_runtime(directory: &RunDir) -> Runtime {
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::Always;
    Runtime::standard().step(Upload).options(options)
}

#[tokio::test]
async fn a_dynamic_scope_resumes_its_durable_lease_and_rejects_corrupt_provenance() {
    let directory = RunDir::new("dynamic-lease-resume");
    let runtime = retained_runtime(&directory);
    let fragment = GraphFragment::chain([(
        "work",
        StepRef::new(
            "process",
            serde_json::json!({
                "run": "echo again >> proof", "shell": "sh",
            }),
        ),
    )]);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    let graph = coordinator
        .register_graph(&upload_graph(&fragment))
        .await
        .unwrap();
    assert_eq!(
        coordinator
            .run_root(graph, BTreeMap::new())
            .await
            .unwrap()
            .status,
        RunStatus::Success
    );
    let report = coordinator.take_root_report().unwrap();
    let firing = report
        .state
        .history()
        .iter()
        .find(|record| record.name.as_str() == "work")
        .unwrap()
        .firing;
    let events_path = coordinator
        .execution_dir(ExecutionId::new(0))
        .join("events.jsonl");
    coordinator.finish().await;
    let before = resources(&directory)
        .await
        .records()
        .find(|record| matches!(record.allocation.scope, ScopeIdentity::Spliced(_)))
        .unwrap()
        .clone();
    assert_eq!(before.introduced_by, Some(ExecutionId::new(0)));
    assert_eq!(before.state, LeaseState::Stopped);

    // Crash after the dynamic step starts: its introducing upload remains
    // durable, but neither that step's result nor invocation completion does.
    let coordinator_path = directory.path().join("coordinator.jsonl");
    let decoded = read_coordinator_log(&*testkit::read_run_dir(directory.path()).await)
        .await
        .unwrap();
    let mut prefix = Vec::new();
    for record in decoded {
        serde_json::to_writer(&mut prefix, &record).unwrap();
        prefix.push(b'\n');
        if matches!(record.body, CoordinatorEvent::ExecutionDeclared { .. }) {
            break;
        }
    }
    fs::write(&coordinator_path, prefix).unwrap();
    let bytes = fs::read_to_string(&events_path).unwrap();
    let mut lines = bytes.lines();
    let mut prefix = format!("{}\n", lines.next().unwrap());
    for line in lines {
        prefix.push_str(line);
        prefix.push('\n');
        let record: execution::StoredEngineRecord = serde_json::from_str(line).unwrap();
        if matches!(record.body, Event::StepStarted { firing: candidate, .. } if candidate == firing)
        {
            break;
        }
    }
    fs::write(&events_path, prefix).unwrap();
    let mut coordinator = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        coordinator
            .run_root(graph, BTreeMap::new())
            .await
            .unwrap()
            .status,
        RunStatus::Success
    );
    coordinator.finish().await;
    let after = resources(&directory).await;
    assert_eq!(after.records().count(), 2);
    assert_eq!(
        after.resolve(before.lease).unwrap().resource_id,
        before.resource_id
    );
    let proof = directory
        .path()
        .join("scopes")
        .join(before.workspace.as_str())
        .join("work/proof");
    assert_eq!(fs::read_to_string(proof).unwrap(), "again\nagain\n");

    let report = prune(&runtime).await.unwrap();
    assert!(report.is_clean(), "{report:?}");
    // Admission snapshots reject corruption before it can enter the log.
    for corrupt_runtime in [false, true] {
        let logs = testkit::write_run_dir(directory.path()).await;
        let mut store = ResourceStore::load(&logs).await.unwrap();
        let original = store.resolve(before.lease).unwrap().clone();
        let error = store
            .update(before.lease, |record| {
                record.introduced_by = Some(if corrupt_runtime {
                    ExecutionId::new(0)
                } else {
                    ExecutionId::new(999)
                });
                if corrupt_runtime {
                    record.runtime.requirements.push("changed".into());
                }
            })
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            execution::ResourceError::AdmissionMismatch(_)
        ));
        let reopened = ResourceStore::load(&logs).await.unwrap();
        assert_eq!(reopened.resolve(before.lease).unwrap(), &original);
    }
}

#[derive(Default)]
struct Order {
    execution:      AtomicUsize,
    first_done:     Notify,
    change_runtime: bool,
}

struct OrderedUpload;
struct MarkWorkspace;
struct NextExecution;

#[async_trait::async_trait]
impl Step for OrderedUpload {
    const NAME: &'static str = "test/ordered-upload";
    type Config = usize;

    async fn run(&self, producer: usize, ctx: StepCtx) -> Outcome {
        let order = ctx.capability::<Order>().expect("test ordering capability");
        let execution = order.execution.load(Ordering::SeqCst);
        if !order.change_runtime && producer != execution % 2 {
            order.first_done.notified().await;
        }
        let mut fragment = GraphFragment::chain([(
            if producer == 0 { "mark-a" } else { "mark-b" },
            StepRef::new(MarkWorkspace::NAME, serde_json::json!(producer)),
        )]);
        if order.change_runtime && execution > 0 {
            fragment.body.scopes[0]
                .runtime
                .requirements
                .push("changed-runtime".into());
        }
        Outcome::success(()).with_splice(SpliceRequest::append(fragment))
    }
}

#[async_trait::async_trait]
impl Step for MarkWorkspace {
    const NAME: &'static str = "test/mark-workspace";
    type Config = usize;

    async fn run(&self, producer: usize, ctx: StepCtx) -> Outcome {
        let order = ctx.capability::<Order>().expect("test ordering capability");
        let execution = order.execution.load(Ordering::SeqCst);
        let prior = ctx
            .env
            .read_file(Path::new("proof"))
            .await
            .expect("read workspace proof");
        let expected = (execution > 0).then(|| format!("{producer}:0").into_bytes());
        if prior != expected {
            order.first_done.notify_one();
            return Outcome::failure(format!(
                "producer {producer} execution {execution}: got {prior:?}, expected {expected:?} in {}",
                ctx.env.workspace_path(),
            ));
        }
        ctx.env
            .write_file(
                Path::new("proof"),
                format!("{producer}:{execution}").as_bytes(),
            )
            .await
            .expect("write workspace proof");
        if producer == execution % 2 {
            order.first_done.notify_one();
        }
        Outcome::success(())
    }
}

#[async_trait::async_trait]
impl Step for NextExecution {
    const NAME: &'static str = "test/next-execution";
    type Config = ();

    async fn run(&self, (): (), ctx: StepCtx) -> Outcome {
        let order = ctx.capability::<Order>().expect("test ordering capability");
        Outcome::success(
            serde_json::json!({"restart": order.execution.fetch_add(1, Ordering::SeqCst) == 0}),
        )
    }
}

#[tokio::test]
async fn reordered_dynamic_scopes_keep_their_workspaces_across_restart() {
    let directory = RunDir::new("dynamic-lease-restart");
    let runtime = retained_runtime(&directory)
        .step(OrderedUpload)
        .step(MarkWorkspace)
        .step(NextExecution)
        .capability(Order::default());
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    let mut graph = GraphBuilder::new();
    let seed = graph.add_step("seed", ScopeId::new(0), "noop");
    let start = graph.add_step("start", ScopeId::new(0), "noop");
    graph.set_join(start, JoinPolicy::Any);
    graph.link(seed, start);
    let producers: Vec<_> = ["a", "b"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let node = graph.add_node(
                name,
                ScopeId::new(0),
                StepRef::new(OrderedUpload::NAME, serde_json::json!(index)),
            );
            graph.node_mut(node).splice_policy = SplicePolicy::Append;
            node
        })
        .collect();
    let next = graph.add_step("next", ScopeId::new(0), NextExecution::NAME);
    graph.set_join(next, JoinPolicy::All);
    graph.mark_entry(seed);
    graph.fan_out(start, &producers);
    for producer in producers {
        graph.link(producer, next);
    }
    let restart = graph.exprs().path("output", &["restart"]);
    graph.select(next, vec![Arm::when(start, restart).with_back()]);
    graph.node_mut(next).routing.groups[0].arms[0].transition = EdgeTransition::Restart;
    for node in &mut graph.graph_mut().body.nodes {
        node.budget.max_firings = 2;
    }
    let digest = coordinator.register_graph(&graph.build()).await.unwrap();
    let result = timeout(
        Duration::from_secs(30),
        coordinator.run_root(digest, BTreeMap::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.status, RunStatus::Success, "{result:?}");
    assert_eq!(result.final_execution, ExecutionId::new(1));
    let records = resources(&directory).await;
    assert_eq!(
        records.records().count(),
        3,
        "one root and two invocation-owned dynamic leases"
    );
    let logs: Vec<_> = [0, 1]
        .into_iter()
        .map(|execution| {
            read_engine_log(
                &coordinator
                    .execution_dir(ExecutionId::new(execution))
                    .join("events.jsonl"),
            )
            .unwrap()
            .log
        })
        .collect();
    let graph = coordinator.load_graph(digest).await.unwrap();
    let states: Vec<_> = logs
        .iter()
        .map(|log| engine::replay((*graph).clone(), log))
        .collect();
    assert_ne!(
        states[0].scope_identity(ScopeId::new(1)),
        states[1].scope_identity(ScopeId::new(1))
    );
    assert_eq!(
        states[0].scope_identity(ScopeId::new(1)),
        states[1].scope_identity(ScopeId::new(2))
    );
    coordinator.finish().await;
    assert!(prune(&runtime).await.unwrap().is_clean());
}

#[tokio::test]
async fn a_restart_refuses_a_changed_runtime_for_an_existing_dynamic_scope() {
    let directory = RunDir::new("dynamic-runtime-mismatch");
    let runtime = retained_runtime(&directory)
        .step(OrderedUpload)
        .step(MarkWorkspace)
        .step(NextExecution)
        .capability(Order {
            change_runtime: true,
            ..Order::default()
        });
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    let mut graph = GraphBuilder::new();
    let seed = graph.add_step("seed", ScopeId::new(0), "noop");
    let upload = graph.add_node(
        "upload",
        ScopeId::new(0),
        StepRef::new(OrderedUpload::NAME, serde_json::json!(0)),
    );
    graph.set_join(upload, JoinPolicy::Any);
    graph.node_mut(upload).splice_policy = SplicePolicy::Append;
    let next = graph.add_step("next", ScopeId::new(0), NextExecution::NAME);
    graph.link(seed, upload);
    graph.link(upload, next);
    graph.mark_entry(seed);
    let restart = graph.exprs().path("output", &["restart"]);
    graph.select(next, vec![Arm::when(upload, restart).with_back()]);
    graph.node_mut(next).routing.groups[0].arms[0].transition = EdgeTransition::Restart;
    for node in &mut graph.graph_mut().body.nodes {
        node.budget.max_firings = 2;
    }
    let digest = coordinator.register_graph(&graph.build()).await.unwrap();
    let result = timeout(
        Duration::from_secs(30),
        coordinator.run_root(digest, BTreeMap::new()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.status, RunStatus::Failed);
    let report = coordinator.take_root_report().unwrap();
    assert!(
        report.state.history().iter().any(|record| record
            .outcome
            .status
            .failure_info()
            .is_some_and(|failure| failure
                .message
                .contains("does not match its recorded resource"))),
        "{:?}",
        report.state.history()
    );
    let records = resources(&directory).await;
    assert_eq!(records.records().count(), 2);
    assert!(
        records
            .records()
            .all(|record| record.runtime.requirements.is_empty())
    );
    coordinator.finish().await;
    assert!(prune(&runtime).await.unwrap().is_clean());
}

struct InheritDynamic;

#[async_trait::async_trait]
impl Step for InheritDynamic {
    const NAME: &'static str = "test/inherit-dynamic";
    type Config = (GraphDigest, RunStatus);

    async fn run(&self, (graph, expected): Self::Config, ctx: StepCtx) -> Outcome {
        ctx.env
            .write_file(Path::new("inherited"), b"kept")
            .await
            .expect("write inherited proof");
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
                sandbox: SandboxMode::Inherit { scope: ctx.scope },
                admission: None,
            })
            .await
            .expect("inherited child starts");
        assert_eq!(child.result().await.status, expected);
        Outcome::failure("keep this dynamic workspace on failure")
    }
}

#[tokio::test]
async fn nested_dynamic_scopes_can_lend_their_lease_and_keep_failure_retention() {
    let directory = RunDir::new("dynamic-lease-inherit");
    let mut options = RunOptions::new(directory.path());
    options.retention = Retention::OnFailure;
    let runtime = Runtime::standard()
        .step(Upload)
        .step(InheritDynamic)
        .options(options);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    let mut child = GraphBuilder::new();
    child.add_node(
        "read",
        ScopeId::new(0),
        StepRef::new(
            "process",
            serde_json::json!({
                "run": "test \"$(cat inherited)\" = kept", "shell": "sh",
            }),
        ),
    );
    let child = coordinator.register_graph(&child.build()).await.unwrap();
    let inner = GraphFragment::chain([(
        "invoke",
        StepRef::new(
            InheritDynamic::NAME,
            serde_json::json!((child, RunStatus::Success)),
        ),
    )]);
    let mut outer = GraphFragment::chain([(
        "upload-inner",
        StepRef::new(Upload::NAME, serde_json::json!(inner)),
    )]);
    outer.body.nodes[0].splice_policy = SplicePolicy::Append;
    let graph = coordinator
        .register_graph(&upload_graph(&outer))
        .await
        .unwrap();
    assert_eq!(
        coordinator
            .run_root(graph, BTreeMap::new())
            .await
            .unwrap()
            .status,
        RunStatus::Failed
    );
    let records = resources(&directory).await;
    assert_eq!(
        records.records().count(),
        3,
        "the inherited child owns no extra lease"
    );
    let nested = records.records().find(|record| matches!(&record.allocation.scope, ScopeIdentity::Spliced(path) if path.len() == 5)).unwrap();
    assert_eq!(nested.state, LeaseState::Stopped);
    let proof = directory
        .path()
        .join("scopes")
        .join(nested.workspace.as_str())
        .join("work/inherited");
    coordinator.finish().await;
    assert_eq!(fs::read_to_string(proof).unwrap(), "kept");
    let coordinator = Coordinator::resume(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    coordinator.finish().await;
    assert!(prune(&runtime).await.unwrap().is_clean());
}

#[tokio::test]
async fn an_inherited_child_refuses_a_dynamic_scope_with_a_different_container() {
    let directory = RunDir::new("inherited-dynamic-runtime");
    let runtime = retained_runtime(&directory).step(InheritDynamic);
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(directory.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .unwrap();
    let mut fragment =
        GraphFragment::chain([("conflict", StepRef::new("noop", serde_json::Value::Null))]);
    fragment.body.scopes[0].runtime = ir::RuntimeSpec::container("different-image");
    let child = coordinator
        .register_graph(&upload_graph(&fragment))
        .await
        .unwrap();
    let mut root = GraphBuilder::new();
    root.add_node(
        "invoke",
        ScopeId::new(0),
        StepRef::new(
            InheritDynamic::NAME,
            serde_json::json!((child, RunStatus::Failed)),
        ),
    );
    let root = coordinator.register_graph(&root.build()).await.unwrap();
    assert_eq!(
        coordinator
            .run_root(root, BTreeMap::new())
            .await
            .unwrap()
            .status,
        RunStatus::Failed
    );
    let child = &coordinator.store().state().invocations[&InvocationId::new(1)];
    assert!(
        child
            .result
            .as_ref()
            .unwrap()
            .failure
            .as_ref()
            .unwrap()
            .message
            .contains("different container from its inherited sandbox")
    );
    assert_eq!(resources(&directory).await.records().count(), 1);
    coordinator.finish().await;
    assert!(prune(&runtime).await.unwrap().is_clean());
}
