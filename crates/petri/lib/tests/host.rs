//! The standalone host's run dir under the coordinator layout: the per-run
//! store, the per-execution `events.jsonl`, the strict read-back rules, and
//! the known-secret refusal.

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use petri::driver::{EventObserver, ExecutionReport};
use petri::engine::{self, EngineState, EventRecord, InvalidRecords};
use petri::execution::prune::{PruneError, prune};
use petri::execution::{self, CoordinatorError};
use petri::executor::{MapSecrets, Retention, SecretProvider as _};
use petri::host::{self, EVENTS_FILE, EventsDecodeError, HostError};
use petri::ir::{Graph, GraphBuilder, RunStatus, RuntimeSpec, Scope, ScopeId, StepRef};
use petri::steps::PROCESS_KIND;
use petri::{RunOptions, Runtime};
use serde_json::json;
use testkit::{RunDir, add_script, wait_for_file};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

fn test_runtime(dir: &RunDir) -> Runtime {
    petri::runtime().options(RunOptions::new(dir.path()))
}

/// The root invocation's first execution directory — the engine log a
/// single-execution run writes and resume reads.
fn root_events(dir: &RunDir) -> PathBuf {
    dir.path()
        .join("executions/0000000000000000")
        .join(EVENTS_FILE)
}

/// The coordinator workspace for scope 0 of the root invocation.
fn root_workspace(dir: &RunDir) -> PathBuf {
    dir.path().join("scopes/invocation-0-scope-0/work")
}

/// The single graph the run registered, from `graphs/`.
fn registered_graph_bytes(dir: &RunDir) -> Vec<u8> {
    let graphs = dir.path().join(execution::GRAPHS_DIR);
    let mut entries: Vec<PathBuf> = fs::read_dir(&graphs)
        .expect("the graphs dir exists")
        .map(|entry| entry.expect("the graphs dir reads").path())
        .collect();
    assert_eq!(entries.len(), 1, "one registered graph: {entries:?}");
    fs::read(entries.remove(0)).expect("the graph reads")
}

fn two_step_graph() -> Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = add_script(&mut b, "first", scope, "echo one");
    let c = add_script(&mut b, "second", scope, "echo two");
    b.link(a, c);
    b.build()
}

/// End to end: the registered graph is byte-exact, the execution's
/// `events.jsonl` reads back equal to the in-memory log, and the reloaded
/// pair passes `verify_replay`.
#[tokio::test]
async fn the_run_dir_is_self_describing() {
    let dir = RunDir::new("host-e2e");
    let rt = test_runtime(&dir);
    let graph = two_step_graph();
    let report = host::run(&rt, graph.clone()).await.expect("runs");
    assert_eq!(report.status, RunStatus::Success);
    assert!(
        report.observer_errors.is_empty(),
        "{:?}",
        report.observer_errors
    );

    let graph_bytes = registered_graph_bytes(&dir);
    assert_eq!(
        graph_bytes,
        serde_json::to_vec(&graph).expect("encodes"),
        "the graph is persisted byte-exact"
    );

    let decoded = host::read_events(&root_events(&dir)).expect("events.jsonl reads");
    assert!(!decoded.torn);
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&report.state.log).expect("encodes"),
        "the file reads back byte-for-byte equal after reserialization"
    );

    let reloaded: Graph = serde_json::from_slice(&graph_bytes).expect("the graph reloads");
    engine::verify_replay(reloaded, &decoded.log).expect("the reloaded pair replays");
}

/// The same framing with no filesystem: encode, decode, byte-equal.
#[tokio::test]
async fn the_byte_codec_round_trips_without_a_filesystem() {
    let dir = RunDir::new("host-codec");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    let times = fabricated_times(&report.state.log);
    let bytes = host::encode_events(&report.state.log, &times);
    let decoded = host::decode_events(&bytes).expect("decodes");
    assert!(!decoded.torn);
    assert_eq!(decoded.clean_len, bytes.len());
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&report.state.log).expect("encodes"),
    );
    assert_eq!(
        decoded.recorded_at, times,
        "each record's recording time rides beside it"
    );
}

/// One distinct recording time per record, for codec tests that need the
/// framing but not a clock.
fn fabricated_times(log: &engine::EventLog) -> Vec<u64> {
    (0..log.len() as u64).map(|seq| 1_000 + seq).collect()
}

/// The engine log version is the run format's, checked on the run
/// declaration: a log of another version reaches the decoder as records
/// whose shape this build does not read, and is refused as such.
#[test]
fn a_record_of_another_shape_is_rejected() {
    let bytes = b"{\"seq\":0,\"recorded_at\":1,\"event\":\"execution.started\"}\n".to_vec();
    match host::decode_events(&bytes) {
        Err(EventsDecodeError::BadRecord { line, .. }) => assert_eq!(line, 1),
        other => panic!("expected a record rejection, got {other:?}"),
    }
}

/// Records whose seqs are not contiguous from 0 cannot become a log.
#[test]
fn out_of_sequence_records_are_rejected() {
    let mut bytes = host::encode_events(&engine::EventLog::new(), &[]);
    let event = serde_json::to_string(&engine::Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    })
    .expect("an event serializes");
    bytes.extend(
        format!("{{\"seq\":3,\"origin\":\"external\",\"recorded_at\":1,\"body\":{event}}}\n")
            .as_bytes(),
    );
    match host::decode_events(&bytes) {
        Err(EventsDecodeError::Invalid(InvalidRecords::SeqMismatch { index, found })) => {
            assert_eq!((index, found), (0, 3));
        }
        other => panic!("expected a seq rejection, got {other:?}"),
    }
}

/// EOF before the final newline: that line is torn, and only that line — the
/// prefix loads, and truncating to `clean_len` yields a clean file.
#[tokio::test]
async fn an_eof_torn_final_line_drops_to_the_prefix() {
    let dir = RunDir::new("host-torn");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    let bytes = host::encode_events(&report.state.log, &fabricated_times(&report.state.log));
    let torn = &bytes[..bytes.len() - 10];
    let decoded = host::decode_events(torn).expect("the prefix loads");
    assert!(decoded.torn);
    assert_eq!(decoded.log.len(), report.state.log.len() - 1);

    let clean = &torn[..decoded.clean_len];
    let redecoded = host::decode_events(clean).expect("the clean prefix loads");
    assert!(!redecoded.torn);
    assert_eq!(redecoded.log.len(), decoded.log.len());
}

/// A newline-terminated line that does not decode refuses the load: corruption
/// or tampering is never silently accepted as a crash prefix.
#[tokio::test]
async fn a_terminated_undecodable_line_refuses_the_load() {
    let dir = RunDir::new("host-tamper");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    let mut bytes = host::encode_events(&report.state.log, &fabricated_times(&report.state.log));
    bytes.extend(b"not a record\n");
    match host::decode_events(&bytes) {
        Err(EventsDecodeError::BadRecord { line, .. }) => {
            assert_eq!(line, report.state.log.len() + 1);
        }
        other => panic!("expected a record rejection, got {other:?}"),
    }
}

const SECRET: &str = "sk-live-9f3a2b7c1d4e";

/// A run with secrets leaves no secret bytes in either file: the records are
/// post-mask before the battery sees them, and the graph carries references.
#[tokio::test]
async fn the_files_hold_no_secret_bytes() {
    let dir = RunDir::new("host-secrets");
    let provider = MapSecrets::from_pairs(&[("DEPLOY_TOKEN", SECRET)]);
    let rt = petri::runtime()
        .secrets(provider)
        .options(RunOptions::new(dir.path()));

    let mut b = GraphBuilder::new();
    b.add_node(
        "deploy",
        ScopeId::new(0),
        StepRef::new(
            PROCESS_KIND,
            json!({
                "run": r#"echo "token is $DEPLOY_TOKEN""#,
                "env": { "DEPLOY_TOKEN": { "$secret": "DEPLOY_TOKEN" } }
            }),
        ),
    );
    let report = host::run(&rt, b.build()).await.expect("runs");
    assert_eq!(report.status, RunStatus::Success);

    let events = fs::read_to_string(root_events(&dir)).expect("exists");
    assert!(!events.contains(SECRET), "the secret leaked into the log");
    assert!(events.contains("***"), "the masked line was persisted");
    let graph_text = String::from_utf8(registered_graph_bytes(&dir)).expect("the graph is UTF-8");
    assert!(
        !graph_text.contains(SECRET),
        "the secret leaked into the graph"
    );
}

/// A registered secret value placed in `Graph.params`: the host refuses to
/// start the run rather than persist the graph.
#[tokio::test]
async fn a_known_secret_in_the_graph_refuses_the_run() {
    let dir = RunDir::new("host-refuse");
    let provider = MapSecrets::empty();
    provider.register("answer:1", SECRET).expect("registers");
    let rt = petri::runtime()
        .secrets(provider)
        .options(RunOptions::new(dir.path()));

    let mut graph = two_step_graph();
    graph
        .params
        .insert(smol_str::SmolStr::new("leak"), json!(SECRET));
    match host::run(&rt, graph).await {
        Err(HostError::SecretInGraph) => {}
        Ok(_) => panic!("the run started with a secret in the graph"),
        Err(other) => panic!("expected the known-secret refusal, got {other}"),
    }
    let persisted = fs::read_dir(dir.path().join(execution::GRAPHS_DIR)).map_or(0, Iterator::count);
    assert_eq!(persisted, 0, "nothing was persisted");
}

/// Wait for the marker a script writes, so a stop lands mid-step.
async fn started(dir: &RunDir) {
    assert!(
        wait_for_file(
            &root_workspace(dir).join("running"),
            Duration::from_secs(10)
        )
        .await,
        "the step never started"
    );
}

/// Run a one-script graph through the host on its own task, handing the
/// coordinator handle back so the test can stop the run mid-step.
fn spawn_run(
    dir: &RunDir,
    script: &str,
) -> (
    JoinHandle<Result<ExecutionReport, HostError>>,
    oneshot::Receiver<execution::CoordinatorHandle>,
) {
    let rt = test_runtime(dir);
    let mut b = GraphBuilder::new();
    add_script(&mut b, "long", ScopeId::new(0), script);
    let graph = b.build();
    let (tx, rx) = oneshot::channel();
    let run = tokio::spawn(async move {
        host::run_with_handle(&rt, graph, |handle| {
            let _ = tx.send(handle);
        })
        .await
    });
    (run, rx)
}

/// A cancelled run still leaves complete files: every log's `finish` is
/// awaited inside the run, whatever way the run ended.
#[tokio::test]
async fn a_cancelled_run_leaves_complete_files() {
    let dir = RunDir::new("host-cancel");
    let (run, handle) = spawn_run(&dir, "echo go > running; sleep 300");
    let handle = handle.await.expect("the run started");
    started(&dir).await;
    handle.cancel_root();

    let report = run.await.expect("the run task").expect("the run completes");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(
        report.observer_errors.is_empty(),
        "{:?}",
        report.observer_errors
    );
    let decoded = host::read_events(&root_events(&dir)).expect("reads");
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&report.state.log).expect("encodes"),
    );
}

/// So does a killed run.
#[tokio::test]
async fn a_killed_run_leaves_complete_files() {
    let dir = RunDir::new("host-kill");
    let (run, handle) = spawn_run(
        &dir,
        "trap '' TERM; echo go > running; while :; do sleep 0.1; done",
    );
    let handle = handle.await.expect("the run started");
    started(&dir).await;
    handle.cancel_root();
    handle.cancel_root();

    let report = run.await.expect("the run task").expect("the run completes");
    assert_eq!(report.status, RunStatus::Cancelled);
    let decoded = host::read_events(&root_events(&dir)).expect("reads");
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&report.state.log).expect("encodes"),
    );
    assert!(
        decoded
            .log
            .events()
            .any(|e| matches!(e, engine::Event::KillRequested { .. })),
        "the kill tier is in the persisted log"
    );
}

/// Rewrite the root execution's `events.jsonl` — the log resume actually
/// reads, under the run-directory layout — keeping only the first `keep`
/// complete records, plus `extra` raw bytes: the crash simulator.
fn damage_events(dir: &RunDir, keep: usize, extra: &[u8]) {
    let path = root_events(dir);
    let bytes = fs::read(&path).expect("reads");
    let mut end = 0;
    let mut seen = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            seen += 1;
            if seen == keep {
                end = i + 1;
                break;
            }
        }
    }
    let mut out = bytes[..end].to_vec();
    out.extend_from_slice(extra);
    fs::write(&path, out).expect("writes");
}

/// A crashed run resumes from nothing but the run dir, and the file converges
/// to the resumed run's log without rewriting history.
#[tokio::test]
async fn a_crashed_run_resumes_from_the_run_dir() {
    let dir = RunDir::new("host-resume");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");
    let total = report.state.log.len();

    // Drop the final record — the second step's finish — at a line boundary:
    // the arbitrary-tail loss window, in its cleanest shape.
    damage_events(&dir, total - 1, b"");
    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(resumed.status, RunStatus::Success);
    assert!(
        resumed.observer_errors.is_empty(),
        "{:?}",
        resumed.observer_errors
    );

    let decoded = host::read_events(&root_events(&dir)).expect("reads");
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&resumed.state.log).expect("encodes"),
        "the file converged to the resumed log"
    );
}

/// A torn tail — EOF mid-record — is truncated away and the run resumes from
/// the prefix.
#[tokio::test]
async fn a_torn_tail_is_truncated_and_resumed() {
    let dir = RunDir::new("host-resume-torn");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");
    let total = report.state.log.len();

    damage_events(&dir, total - 1, b"{\"seq\":9999,\"source\":\"Ext");
    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(resumed.status, RunStatus::Success);

    let decoded = host::read_events(&root_events(&dir)).expect("reads clean");
    assert!(!decoded.torn, "the torn tail is gone");
    assert_eq!(
        serde_json::to_vec(&decoded.log).expect("encodes"),
        serde_json::to_vec(&resumed.state.log).expect("encodes"),
    );
}

/// A complete but undecodable line refuses the resume outright.
#[tokio::test]
async fn an_undecodable_record_refuses_resume() {
    let dir = RunDir::new("host-resume-refuse");
    let rt = test_runtime(&dir);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    damage_events(
        &dir,
        report.state.log.len() - 1,
        b"corrupted beyond recognition\n",
    );
    // The run-directory store refuses a complete line that is not JSON
    // before any record is decoded; a JSON line of another shape is the
    // decoder's refusal (`a_record_of_another_shape_is_rejected`).
    match host::resume(&rt).await {
        Err(HostError::Coordinator(CoordinatorError::EngineLog(
            execution::EngineLogError::Store(source),
        ))) => {
            assert!(source.to_string().contains("not JSON"), "{source}");
        }
        Ok(_) => panic!("a corrupted file resumed"),
        Err(other) => panic!("expected the record refusal, got {other}"),
    }
}

/// An observer registered on the `Runtime` builder reaches the driver it
/// builds, alongside the host's own battery.
#[derive(Default)]
struct Counting {
    seen: Mutex<Vec<u64>>,
}

#[async_trait::async_trait]
impl EventObserver for Counting {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _state: &EngineState) {
        self.seen.lock().expect("not poisoned").push(record.seq);
    }
}

#[tokio::test]
async fn a_runtime_registered_observer_reaches_the_driver() {
    let dir = RunDir::new("host-runtime-observe");
    let counting = Arc::new(Counting::default());
    let rt = test_runtime(&dir).observe(counting.clone() as Arc<dyn EventObserver>);
    let report = host::run(&rt, two_step_graph()).await.expect("runs");

    let seen = counting.seen.lock().expect("not poisoned");
    let expected: Vec<u64> = report.state.log.records().iter().map(|r| r.seq).collect();
    assert_eq!(*seen, expected);
}

/// The Docker fence across a resume, through the host: `host::resume` builds a
/// fresh executor, which finds the crashed run's sandbox by the run id
/// recorded in the run dir and the lease's workspace label, attaches it, and
/// fences it — one stop, one start — before re-dispatching the step into the
/// same workspace.
#[tokio::test]
async fn resume_fences_the_crashed_container() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("host-docker-resume");
    let mut options = RunOptions::new(dir.path());
    options.retention = Retention::Never;
    let rt = petri::runtime().options(options);
    let mut b = GraphBuilder::bare();
    let mut scope = Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container("alpine:3.20");
    let scope = b.add_scope(scope);
    // The beater runs in its own session: an aborted driver still lets the
    // orphaned step task stop its own process group as the channels close, so a
    // detached beater is what a dead *process* leaves behind — only a
    // sandbox-level fence can end it. With `done` already in the workspace the
    // step measures the heartbeat instead of beating: two sizes half a second
    // apart, equal only if the beater is dead.
    b.add_node(
        "beat",
        scope,
        StepRef::new(
            PROCESS_KIND,
            testkit::script_with(
                r#"
if [ -e done ]; then
  a=$(wc -c < heartbeat); sleep 0.5; b=$(wc -c < heartbeat)
  echo "before=$a" > "$CI_OUTPUT"; echo "after=$b" >> "$CI_OUTPUT"
  exit 0
fi
setsid sh -c 'while :; do echo tick >> heartbeat; sleep 0.05; done' &
sleep 300
"#,
                &json!({ "shell": "sh" }),
            ),
        ),
    );
    let graph = b.build();

    let run = tokio::spawn(async move { host::run(&rt, graph).await });
    // The root invocation's scope 0 is the run's first lease.
    assert!(
        wait_for_file(
            &dir.path().join(execution::RUN_FILE),
            Duration::from_secs(60)
        )
        .await,
        "the run never started"
    );
    let sandbox = testkit::sandbox_name(dir.path(), 0);
    assert!(
        testkit::wait_for_container_file(&sandbox, "/workspace/heartbeat", Duration::from_secs(60))
            .await,
        "the step never started inside the container"
    );
    // The crash: the host task is gone — coordinator, driver and lease drop,
    // release never runs, the container beats on.
    run.abort();
    let _ = run.await;
    let crashed_id = testkit::container_id(&sandbox)
        .await
        .expect("the crashed run's sandbox outlives it");
    assert!(
        testkit::container_write(&sandbox, "/workspace/done", "").await,
        "the crashed sandbox is still running"
    );

    // A resuming process builds everything fresh over the same run dir.
    let mut options = RunOptions::new(dir.path());
    options.retention = Retention::Never;
    let rt = petri::runtime().options(options);
    let resumed = host::resume(&rt).await.expect("resumes");
    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );
    let output = testkit::output_of(&resumed, "beat");
    assert_eq!(
        output["before"], output["after"],
        "the crashed container's beater kept writing: the fence missed it ({output})"
    );
    assert!(
        testkit::container_id(&sandbox).await.is_none(),
        "the run's release ended the sandbox (was {crashed_id})"
    );
    let prefix = format!("petri-{}-", testkit::recorded_run_id(dir.path()));
    let leftovers = testkit::list_containers(&prefix).await;
    assert!(
        leftovers.is_empty(),
        "containers were left behind: {leftovers:?}"
    );
}

/// A run kept by retention leaves its sandbox stopped on the daemon, with
/// its record saying so; `petri sandbox prune` deletes it, records the
/// tombstone, and refuses to run while a live process holds the run.
#[tokio::test]
async fn prune_deletes_a_kept_sandbox_and_records_the_tombstone() {
    if !testkit::is_docker_ready().await {
        return;
    }
    let dir = RunDir::new("host-docker-prune");
    let mut options = RunOptions::new(dir.path());
    options.retention = Retention::Always;
    let rt = petri::runtime().options(options);
    let mut b = GraphBuilder::bare();
    let mut scope = Scope::new(ScopeId::new(0));
    scope.runtime = RuntimeSpec::container("alpine:3.20");
    let scope = b.add_scope(scope);
    b.add_node(
        "write",
        scope,
        StepRef::new(
            PROCESS_KIND,
            testkit::script_with("echo kept > kept.txt", &json!({ "shell": "sh" })),
        ),
    );
    let report = host::run(&rt, b.build()).await.expect("runs");
    assert_eq!(
        report.status,
        RunStatus::Success,
        "{:?}",
        report.state.errors()
    );

    let sandbox = testkit::sandbox_name(dir.path(), 0);
    assert!(
        testkit::container_id(&sandbox).await.is_some()
            && !testkit::container_is_running(&sandbox).await,
        "retention kept the sandbox, stopped"
    );
    let records = execution::ResourceStore::load(&testkit::read_run_dir(dir.path()).await)
        .await
        .expect("records load");
    let record = records
        .resolve(execution::SandboxLeaseId::new(0))
        .expect("lease 0");
    assert_eq!(record.state, execution::LeaseState::Stopped);
    assert_eq!(record.provider, "docker");
    assert!(record.resource_id.is_some() && record.admission.is_some());
    assert!(record.reference.is_some());
    drop(records);

    // Prune only deletes sandboxes, even when runtime retention would
    // otherwise remove host workspaces.
    let workspace = root_workspace(&dir);
    fs::create_dir_all(&workspace).expect("create host workspace");
    let sentinel = workspace.join("sentinel");
    fs::write(&sentinel, b"keep host workspace").expect("write sentinel");
    let mut options = RunOptions::new(dir.path());
    options.retention = Retention::Never;
    let prune_rt = petri::runtime().options(options);
    let pruned = prune(&prune_rt).await.expect("prune");
    assert!(pruned.is_clean(), "{pruned:?}");
    assert_eq!(pruned.deleted.len(), 1, "{pruned:?}");
    assert_eq!(
        fs::read(&sentinel).expect("prune preserves the host workspace"),
        b"keep host workspace"
    );
    assert!(
        testkit::container_id(&sandbox).await.is_none(),
        "prune deleted the sandbox"
    );
    let records = execution::ResourceStore::load(&testkit::read_run_dir(dir.path()).await)
        .await
        .expect("records load");
    assert_eq!(
        records
            .resolve(execution::SandboxLeaseId::new(0))
            .expect("the tombstone stays")
            .state,
        execution::LeaseState::Deleted
    );
    drop(records);

    // A second prune finds only the tombstone.
    let again = prune(&prune_rt).await.expect("prune again");
    assert!(again.deleted.is_empty() && again.is_clean(), "{again:?}");

    // A held run is refused.
    let held = testkit::write_run_dir(dir.path()).await;
    let error = prune(&prune_rt)
        .await
        .expect_err("a held run is not pruned");
    assert!(matches!(error, PruneError::RunHeld(_)), "{error}");
    drop(held);
}
