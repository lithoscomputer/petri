//! `inspect_run` over run directories the coordinator wrote: complete runs,
//! restarts, nested invocations, retries, and every way the files can fall
//! short of a trustworthy snapshot.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};
use std::{fs, io};

use execution::inspect::{InspectError, RunInspection, inspect_run_dir};
use execution::{
    COORDINATOR_FILE, CallSite, Coordinator, CoordinatorInvocationClient, CoordinatorOptions,
    ExecutionId, GraphDigest, InterviewReceipt, InvocationClient as _, InvocationId,
    InvocationRequest, RECEIPT_FILE, RECEIPT_VERSION, SandboxMode, SecretBindings, StoreError,
};
use ir::{
    EdgeTransition, GraphBuilder, Outcome, RetryPolicy, RunStatus, Scope, ScopeId, StepRef, Value,
};
use runtime::steps::{Step, StepCtx};
use runtime::{RunOptions, Runtime};
use serde::Deserialize;
use serde_json::json;
use testkit::RunDir;

const ROOT_EVENTS: &str = "executions/0000000000000000/events.jsonl";

/// Every file under `root`: its bytes and modification time.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (SystemTime, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, (SystemTime, Vec<u8>)>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                walk(&path, out)?;
            } else {
                let modified = entry.metadata()?.modified()?;
                out.insert(path.clone(), (modified, fs::read(&path)?));
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out).expect("the run dir walks");
    out
}

fn runtime(dir: &RunDir) -> Runtime {
    Runtime::standard().options(RunOptions::new(dir.path()))
}

async fn run_graph(runtime: &Runtime, dir: &RunDir, graph: &ir::Graph) -> RunStatus {
    let mut coordinator = Coordinator::create(
        runtime.prepare_run(dir.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");
    let digest = coordinator
        .register_graph(graph)
        .await
        .expect("graph registers");
    let result = coordinator
        .run_root(digest, BTreeMap::new())
        .await
        .expect("the root runs");
    coordinator.finish().await;
    result.status
}

fn two_noops() -> ir::Graph {
    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    let first = builder.add_node(
        "first",
        scope,
        StepRef::new("noop", json!({ "marker": "alpha" })),
    );
    let second = builder.add_node(
        "second",
        scope,
        StepRef::new("noop", json!({ "marker": "beta" })),
    );
    builder.link(first, second);
    builder.build()
}

#[tokio::test]
async fn a_finished_run_inspects_complete_with_its_context() {
    let dir = RunDir::new("inspect-finished");
    let rt = runtime(&dir);
    assert_eq!(run_graph(&rt, &dir, &two_noops()).await, RunStatus::Success);

    let before = snapshot(dir.path());
    let first = inspect_run_dir(dir.path()).await.expect("inspects");
    let second = inspect_run_dir(dir.path()).await.expect("inspects again");
    assert_eq!(first, second, "inspection is deterministic");
    assert_eq!(
        snapshot(dir.path()),
        before,
        "inspection changed the run dir"
    );

    assert!(first.complete, "{:?}", first.incomplete);
    assert_eq!(first.status.as_deref(), Some("success"));
    assert_eq!(first.inspect_format_version, 3);
    assert_eq!(first.root.final_execution.map(ExecutionId::raw), Some(0));
    assert_eq!(first.invocations.len(), 1);
    assert_eq!(first.executions.len(), 1);
    let execution = &first.executions[0];
    assert_eq!(execution.status, "finished");
    assert_eq!(execution.log.replay, "verified");
    let engine = execution.engine.as_ref().expect("a replayed engine");
    assert!(engine.finished);
    assert_eq!(engine.folded_status, "success");
    let nodes = &engine.context.nodes;
    assert_eq!(nodes["first"].status, "success");
    assert_eq!(nodes["first"].output, json!({ "marker": "alpha" }));
    assert_eq!(nodes["second"].attempts, 1);
    assert_eq!(nodes["second"].generation, 0);
    assert_eq!(
        engine
            .history
            .iter()
            .map(|record| record.node.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    // The terminal node has no routing group, so it applies no route.
    assert_eq!(engine.routes.len(), 1, "{:?}", engine.routes);
    assert_eq!(engine.routes[0].node.as_deref(), Some("first"));
    assert_eq!(engine.routes[0].target.as_deref(), Some("second"));
    assert!(engine.live.is_empty());
}

#[tokio::test]
async fn a_restart_keeps_every_execution_and_names_the_final_one() {
    let dir = RunDir::new("inspect-restart");
    let rt = runtime(&dir);
    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    let start = builder.add_node("start", scope, StepRef::new("noop", json!("one")));
    let target = builder.add_node("target", scope, StepRef::new("noop", json!("two")));
    builder.mark_entry(start);
    builder.link(start, target);
    builder.node_mut(start).routing.groups[0].arms[0].transition = EdgeTransition::Restart;
    assert_eq!(
        run_graph(&rt, &dir, &builder.build()).await,
        RunStatus::Success
    );

    let inspection = inspect_run_dir(dir.path()).await.expect("inspects");
    assert!(inspection.complete, "{:?}", inspection.incomplete);
    assert_eq!(
        inspection.root.final_execution.map(ExecutionId::raw),
        Some(1)
    );
    assert_eq!(
        inspection.root.latest_execution.map(ExecutionId::raw),
        Some(1)
    );
    let root = &inspection.invocations[0];
    assert_eq!(
        root.executions
            .iter()
            .copied()
            .map(ExecutionId::raw)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    let [first, second] = inspection.executions.as_slice() else {
        panic!("two executions: {:?}", inspection.executions);
    };
    assert_eq!(first.status, "restarted");
    assert_eq!(first.successor.map(ExecutionId::raw), Some(1));
    assert_eq!(first.entry_node, None);
    let first_engine = first.engine.as_ref().expect("replayed");
    assert!(first_engine.context.nodes.contains_key("start"));
    assert!(!first_engine.context.nodes.contains_key("target"));
    assert_eq!(second.status, "finished");
    assert_eq!(second.predecessor.map(ExecutionId::raw), Some(0));
    assert_eq!(second.execution_index, 1);
    assert_eq!(second.entry_node.as_deref(), Some("target"));
    let second_engine = second.engine.as_ref().expect("replayed");
    assert!(
        !second_engine.context.nodes.contains_key("start"),
        "the successor's context is its own"
    );
    assert_eq!(second_engine.context.nodes["target"].output, json!("two"));
}

/// A step that fails on its first attempt and succeeds on the second.
struct FlakyStep;

#[derive(Clone)]
struct FlakyCalls(Arc<AtomicU32>);

#[async_trait::async_trait]
impl Step for FlakyStep {
    const NAME: &'static str = "test/flaky";
    type Config = ();

    async fn run(&self, (): (), ctx: StepCtx) -> Outcome {
        let calls = match ctx.require_capability::<FlakyCalls>() {
            Ok(calls) => calls,
            Err(error) => return error.into(),
        };
        if calls.0.fetch_add(1, Ordering::SeqCst) == 0 {
            Outcome::failure("first try fails").with_context_update("tries", 1)
        } else {
            Outcome::success(json!("recovered")).with_context_update("tries", 2)
        }
    }
}

#[tokio::test]
async fn retries_show_in_attempts_and_not_in_the_node_record() {
    let dir = RunDir::new("inspect-retry");
    let rt = runtime(&dir)
        .step(FlakyStep)
        .capability(FlakyCalls(Arc::new(AtomicU32::new(0))));
    let mut builder = GraphBuilder::bare();
    let scope = builder.add_scope(Scope::new(ScopeId::new(0)));
    let flaky = builder.add_step("flaky", scope, FlakyStep::NAME);
    builder.node_mut(flaky).retry = RetryPolicy::attempts(2).with_backoff(ir::Backoff {
        initial: Duration::from_millis(1),
        factor:  1.0,
        max:     Duration::from_millis(1),
        jitter:  false,
    });
    assert_eq!(
        run_graph(&rt, &dir, &builder.build()).await,
        RunStatus::Success
    );

    let inspection = inspect_run_dir(dir.path()).await.expect("inspects");
    assert!(inspection.complete, "{:?}", inspection.incomplete);
    let engine = inspection.executions[0].engine.as_ref().expect("replayed");
    let record = &engine.context.nodes["flaky"];
    assert_eq!(record.status, "success");
    assert_eq!(record.attempts, 2);
    assert_eq!(record.output, json!("recovered"));
    assert_eq!(
        engine.context.kv["tries"],
        json!(2),
        "a retried attempt's context_updates never merge"
    );
    assert_eq!(engine.history.len(), 1, "one final record per firing");
    assert_eq!(engine.history[0].attempt, 2);
    let attempts: Vec<_> = engine
        .attempts
        .iter()
        .map(|attempt| (attempt.attempt, attempt.status, attempt.is_final))
        .collect();
    assert_eq!(attempts, [(1, "failure", false), (2, "success", true)]);
    assert_eq!(
        engine.attempts[0]
            .failure
            .as_ref()
            .map(|failure| failure.message.as_str()),
        Some("first try fails")
    );
}

#[derive(Deserialize)]
struct InvokeConfig {
    graph: GraphDigest,
}

struct InvokeStep;

#[async_trait::async_trait]
impl Step for InvokeStep {
    const NAME: &'static str = "test/invoke";
    type Config = InvokeConfig;

    async fn run(&self, config: InvokeConfig, ctx: StepCtx) -> Outcome {
        let client = match ctx.require_capability::<CoordinatorInvocationClient>() {
            Ok(client) => client,
            Err(error) => return error.into(),
        };
        let mut handle = match client
            .start_or_attach(InvocationRequest {
                site:      CallSite {
                    firing:  ctx.firing,
                    attempt: ctx.attempt,
                    slot:    "child".into(),
                },
                graph:     config.graph,
                context:   BTreeMap::from([("seed".into(), json!("from-parent"))]),
                secrets:   SecretBindings::None,
                sandbox:   SandboxMode::Isolated,
                admission: None,
            })
            .await
        {
            Ok(handle) => handle,
            Err(error) => return Outcome::failure(error.to_string()),
        };
        let result = handle.result().await;
        Outcome::success(result.output).with_context_update("child_kv", json!(result.context))
    }
}

#[tokio::test]
async fn a_nested_invocation_keeps_its_own_context_and_parent_link() {
    let dir = RunDir::new("inspect-child");
    let rt = runtime(&dir).step(InvokeStep);
    let mut coordinator = Coordinator::create(
        rt.prepare_run(dir.path()),
        Vec::new(),
        CoordinatorOptions::default(),
    )
    .await
    .expect("the coordinator starts");

    let mut child = GraphBuilder::bare();
    let scope = child.add_scope(Scope::new(ScopeId::new(0)));
    let inner = child.add_node("inner", scope, StepRef::new("noop", json!("child-output")));
    let mut child = child.build();
    child.result = ir::ResultProjection::NodeOutput(inner);
    let child_digest = coordinator
        .register_graph(&child)
        .await
        .expect("child registers");

    let mut root = GraphBuilder::bare();
    let scope = root.add_scope(Scope::new(ScopeId::new(0)));
    root.add_node(
        "parent",
        scope,
        StepRef::new(InvokeStep::NAME, json!({ "graph": child_digest })),
    );
    let root_digest = coordinator
        .register_graph(&root.build())
        .await
        .expect("root registers");
    let result = coordinator
        .run_root(root_digest, BTreeMap::new())
        .await
        .expect("the root runs");
    assert_eq!(result.status, RunStatus::Success);
    coordinator.finish().await;

    let inspection = inspect_run_dir(dir.path()).await.expect("inspects");
    assert!(inspection.complete, "{:?}", inspection.incomplete);
    assert_eq!(inspection.graphs.len(), 2);
    let [root, child] = inspection.invocations.as_slice() else {
        panic!("two invocations: {:?}", inspection.invocations);
    };
    assert_eq!(root.children, [InvocationId::new(1)]);
    assert_eq!(child.parent.as_ref().map(|p| p.execution.raw()), Some(0));
    assert_eq!(
        child.parent.as_ref().map(|p| p.slot.as_str()),
        Some("child")
    );
    assert_eq!(child.graph, child_digest);
    assert_eq!(child.context["seed"], json!("from-parent"));
    assert_eq!(child.secrets.mode, "none");
    let child_result = child.result.as_ref().expect("the child finished");
    assert_eq!(child_result.output, json!("child-output"));
    let root_execution = &inspection.executions[0];
    assert_eq!(root_execution.children, [InvocationId::new(1)]);
    let child_execution = inspection
        .executions
        .iter()
        .find(|execution| execution.invocation == InvocationId::new(1))
        .expect("the child's execution");
    assert_eq!(child_execution.status, "finished");
    let child_engine = child_execution.engine.as_ref().expect("replayed");
    assert_eq!(
        child_engine.context.nodes["inner"].output,
        json!("child-output")
    );
    assert!(
        !child_engine.context.nodes.contains_key("parent"),
        "the child's context is not the parent's"
    );
    let root_engine = root_execution.engine.as_ref().expect("replayed");
    assert_eq!(
        root_engine.context.nodes["parent"].output,
        json!("child-output")
    );
}

/// Keep only the first `keep` complete lines of `path`, then append `extra`.
fn damage(path: &Path, keep: usize, extra: &[u8]) {
    let bytes = fs::read(path).expect("reads");
    let mut end = 0;
    let mut seen = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            seen += 1;
            if seen == keep {
                end = index + 1;
                break;
            }
        }
    }
    let mut out = bytes[..end].to_vec();
    out.extend_from_slice(extra);
    fs::write(path, out).expect("writes");
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path).expect("reads").lines().count()
}

async fn finished_run(label: &str) -> RunDir {
    let dir = RunDir::new(label);
    let rt = runtime(&dir);
    assert_eq!(run_graph(&rt, &dir, &two_noops()).await, RunStatus::Success);
    dir
}

#[tokio::test]
async fn a_run_without_its_finish_record_is_incomplete_not_final() {
    let dir = finished_run("inspect-unfinished").await;
    let path = dir.path().join(COORDINATOR_FILE);
    damage(&path, line_count(&path) - 1, b"");

    let inspection = inspect_run_dir(dir.path()).await.expect("inspects");
    assert!(!inspection.complete);
    assert_eq!(inspection.status, None);
    assert!(
        inspection
            .incomplete
            .iter()
            .any(|reason| reason.contains("not recorded its finish")),
        "{:?}",
        inspection.incomplete
    );
}

/// A torn tail is the run directory's own business: a reader drops it,
/// leaves the file as found, and reports the complete prefix.
#[tokio::test]
async fn a_torn_coordinator_tail_is_dropped_and_left_in_place() {
    let dir = finished_run("inspect-torn-coordinator").await;
    let path = dir.path().join(COORDINATOR_FILE);
    let mut bytes = fs::read(&path).expect("reads");
    bytes.extend_from_slice(b"{\"seq\":99,\"event\":");
    fs::write(&path, &bytes).expect("writes");

    let before = snapshot(dir.path());
    let inspection = inspect_run_dir(dir.path()).await.expect("inspects");
    assert_eq!(snapshot(dir.path()), before, "the torn tail was rewritten");
    assert!(inspection.complete, "{:?}", inspection.incomplete);
    assert_eq!(inspection.status.as_deref(), Some("success"));
}

#[tokio::test]
async fn a_short_engine_log_is_a_prefix_and_a_torn_tail_is_dropped() {
    let dir = finished_run("inspect-torn-events").await;
    let path = dir.path().join(ROOT_EVENTS);
    // Cut right after the first routing decision: the core records it
    // derives (the applied route, the emitted token) are gone, so the file is
    // a byte-prefix of what replay regenerates. Then tear the tail.
    let text = fs::read_to_string(&path).expect("reads");
    let keep = text
        .lines()
        .position(|line| line.contains("routing.resolved"))
        .expect("a routing decision")
        + 1;
    damage(&path, keep, b"{\"seq\":");

    let inspection = inspect_run_dir(dir.path()).await.expect("inspects");
    assert!(!inspection.complete);
    let execution = &inspection.executions[0];
    assert_eq!(execution.log.replay, "prefix");
    assert_eq!(execution.log.records, keep);
    assert!(
        inspection
            .incomplete
            .iter()
            .any(|reason| reason.contains("before the core's derived records")),
        "{:?}",
        inspection.incomplete
    );
    assert_eq!(
        execution.status, "finished",
        "the coordinator's exit still stands"
    );
    let engine = execution.engine.as_ref().expect("replayed");
    assert!(!engine.finished, "the short log does not reach the exit");
    assert!(
        engine.context.nodes.contains_key("first") && !engine.context.nodes.contains_key("second")
    );
}

#[tokio::test]
async fn a_missing_engine_log_is_incomplete() {
    let dir = finished_run("inspect-missing-events").await;
    fs::remove_file(dir.path().join(ROOT_EVENTS)).expect("removes");

    let inspection = inspect_run_dir(dir.path()).await.expect("inspects");
    assert!(!inspection.complete);
    assert_eq!(inspection.executions[0].log.replay, "missing");
    assert!(inspection.executions[0].engine.is_none());
}

#[tokio::test]
async fn a_complete_undecodable_record_is_an_error() {
    let dir = finished_run("inspect-corrupt-events").await;
    let path = dir.path().join(ROOT_EVENTS);
    let mut bytes = fs::read(&path).expect("reads");
    bytes.extend_from_slice(b"not-json\n");
    fs::write(&path, &bytes).expect("writes");
    let error = inspect_run_dir(dir.path())
        .await
        .expect_err("corruption is refused");
    assert!(matches!(error, InspectError::EngineLog(_)), "{error}");

    let dir = finished_run("inspect-corrupt-coordinator").await;
    let path = dir.path().join(COORDINATOR_FILE);
    let mut bytes = fs::read(&path).expect("reads");
    bytes.extend_from_slice(b"not-json\n");
    fs::write(&path, &bytes).expect("writes");
    let error = inspect_run_dir(dir.path())
        .await
        .expect_err("corruption is refused");
    assert!(
        matches!(error, InspectError::Store(StoreError::Store(_))),
        "{error}"
    );
}

#[tokio::test]
async fn a_log_that_diverges_from_replay_is_an_error() {
    let dir = finished_run("inspect-diverged").await;
    let path = dir.path().join(ROOT_EVENTS);
    let text = fs::read_to_string(&path).expect("reads");
    let mut changed = false;
    let rewritten: Vec<String> = text
        .lines()
        .map(|line| {
            if !changed && line.contains("step.finished") && line.contains("alpha") {
                changed = true;
                line.replace("alpha", "omega")
            } else {
                line.to_owned()
            }
        })
        .collect();
    assert!(changed, "the first outcome was found");
    fs::write(&path, format!("{}\n", rewritten.join("\n"))).expect("writes");

    let error = inspect_run_dir(dir.path())
        .await
        .expect_err("divergence is refused");
    assert!(
        matches!(error, InspectError::ReplayDiverged { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn an_unsupported_format_is_an_error() {
    let dir = finished_run("inspect-unsupported").await;
    let path = dir.path().join(COORDINATOR_FILE);
    let text = fs::read_to_string(&path).expect("reads");
    let mut lines: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect();
    lines[0]["body"]["format_version"] = json!(1);
    let rewritten: Vec<String> = lines
        .iter()
        .map(|line| serde_json::to_string(line).expect("encodes"))
        .collect();
    fs::write(&path, format!("{}\n", rewritten.join("\n"))).expect("writes");
    let error = inspect_run_dir(dir.path())
        .await
        .expect_err("an old format is refused");
    assert!(
        matches!(error, InspectError::UnsupportedFormat {
            found:    1,
            expected: execution::COORDINATOR_FORMAT_VERSION,
        }),
        "{error}"
    );
}

#[tokio::test]
async fn a_missing_registered_graph_is_an_error() {
    let dir = finished_run("inspect-missing-graph").await;
    let graphs = dir.path().join(execution::GRAPHS_DIR);
    for entry in fs::read_dir(&graphs).expect("graphs dir") {
        fs::remove_file(entry.expect("entry").path()).expect("removes");
    }
    let error = inspect_run_dir(dir.path())
        .await
        .expect_err("a missing graph is refused");
    assert!(
        matches!(error, InspectError::Store(StoreError::MissingGraph(_))),
        "{error}"
    );
}

#[tokio::test]
async fn a_directory_that_is_not_a_run_is_an_error() {
    let dir = RunDir::new("inspect-not-a-run");
    let error = inspect_run_dir(dir.path()).await.expect_err("no run.json");
    assert!(matches!(error, InspectError::Store(_)), "{error}");
}

#[tokio::test]
async fn the_document_serializes_with_its_version_first_class() {
    let dir = finished_run("inspect-json").await;
    let inspection: RunInspection = inspect_run_dir(dir.path()).await.expect("inspects");
    let json = serde_json::to_value(&inspection).expect("encodes");
    assert_eq!(json["inspect_format_version"], json!(3));
    assert_eq!(
        json["coordinator_format_version"],
        json!(execution::COORDINATOR_FORMAT_VERSION)
    );
    assert!(json["locator"].is_string());
    assert!(json["run_key"].is_string());
    assert_eq!(json["complete"], json!(true));
    assert_eq!(json["status"], json!("success"));
    assert_eq!(json["root"]["final_execution"], json!(0));
    assert_eq!(
        json["executions"][0]["engine"]["context"]["nodes"]["first"]["status"],
        json!("success")
    );
    assert_eq!(
        json["executions"][0]["engine"]["attempts"][0]["final"],
        json!(true)
    );
    assert_eq!(
        json["interviews"],
        Value::Null,
        "a run without an interviewer has no receipt"
    );
}

#[tokio::test]
async fn the_interview_receipt_is_read_back_as_written() {
    let dir = finished_run("inspect-receipt").await;
    let receipt = InterviewReceipt {
        lifetime:  0,
        version:   RECEIPT_VERSION,
        questions: Vec::new(),
        errors:    vec!["scripted entry `never-asked` answered 0 of 1".to_owned()],
        script:    Some(json!({ "entries": [] })),
    };
    let path = dir.path().join(RECEIPT_FILE);
    fs::write(&path, serde_json::to_vec_pretty(&receipt).expect("encodes")).expect("writes");
    let inspection = inspect_run_dir(dir.path()).await.expect("inspects");
    assert!(inspection.complete, "{:?}", inspection.incomplete);
    assert_eq!(inspection.interviews, Some(receipt));

    fs::write(&path, b"{\"version\": \"one\"}\n").expect("writes");
    let error = inspect_run_dir(dir.path())
        .await
        .expect_err("a bad receipt is an error");
    assert!(
        matches!(&error, InspectError::BadReceipt { path: bad, .. } if *bad == path),
        "{error}"
    );
}
