//! ACP dispatch on the Daytona capability combination, without a cloud or
//! model.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use attractor_steps::acp::{Client, Stage};
use executor::{ExecEnv, Masker, ProcessSpec, StdinMode};
use ir::{Attempt, FiringId, ScopeId};
use sandbox_driver::{
    Capabilities, Capability, Exec, ExecControls, ExecResult, ExecSpec, ExecStreamingResult,
    Filesystem, Isolation, PlatformInfo, Sandbox, SandboxId, SandboxStatus, SpawnSpec, StderrTail,
    StdioProcess, StdioProcessHandle, Termination,
};
use serde_json::{Value, json};
use steps::ProgressSender;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, duplex};
use tokio::sync::{Mutex as AsyncMutex, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{self, timeout};
use tokio_util::sync::CancellationToken;

use crate::env::SandboxEnv;
use crate::gate::RunGate;

const BUDGET: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
enum Peer {
    Acp,
    Exit,
    Silent,
}

#[derive(Default)]
struct Seen {
    specs:      Mutex<Vec<SpawnSpec>>,
    mkdir:      Mutex<Vec<Vec<String>>>,
    methods:    Mutex<Vec<String>>,
    streaming:  AtomicUsize,
    terminated: AtomicUsize,
    waits:      AtomicUsize,
}

struct TextOnlySandbox {
    id:           SandboxId,
    capabilities: Capabilities,
    peer:         Peer,
    seen:         Arc<Seen>,
}

#[async_trait]
impl Exec for TextOnlySandbox {
    async fn run(&self, spec: &ExecSpec) -> sandbox_driver::Result<ExecResult> {
        assert_eq!(spec.program, "mkdir");
        self.seen
            .mkdir
            .lock()
            .expect("mkdir")
            .push(spec.args.clone());
        Ok(ExecResult::new(
            Termination::Exited,
            Some(0),
            Duration::ZERO,
        ))
    }

    async fn run_streaming(
        &self,
        _: &ExecSpec,
        _: ExecControls,
    ) -> sandbox_driver::Result<ExecStreamingResult> {
        self.seen.streaming.fetch_add(1, Ordering::SeqCst);
        Err(sandbox_driver::Error::unsupported(
            Capability::ExecStdinStream,
        ))
    }

    async fn spawn_stdio(&self, spec: &SpawnSpec) -> sandbox_driver::Result<StdioProcess> {
        self.seen.specs.lock().expect("specs").push(spec.clone());
        let (stdin, input) = duplex(4096);
        let (mut output, stdout) = duplex(4096);
        let tail = StderrTail::new(8192);
        let stderr_tail = tail.clone();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let (outcome, status) = watch::channel(None);
        let seen = self.seen.clone();
        let peer = self.peer;
        let mut workers = JoinSet::new();
        workers.spawn(async move {
            let mut input = BufReader::new(input).lines();
            loop {
                let line = tokio::select! {
                    () = stopped.cancelled() => break,
                    line = input.next_line() => match line.expect("read request") {
                        Some(line) => line,
                        None => break,
                    },
                };
                let request: Value = serde_json::from_str(&line).expect("JSON request");
                let method = request["method"].as_str().expect("method");
                seen.methods
                    .lock()
                    .expect("methods")
                    .push(method.to_owned());
                match peer {
                    Peer::Silent => continue,
                    Peer::Exit => {
                        tail.push(b"launch failed: diagnostic-secret\n");
                        outcome
                            .send(Some((Termination::Exited, Some(127))))
                            .expect("outcome");
                        return;
                    }
                    Peer::Acp => {}
                }
                let result = match method {
                    "initialize" => json!({ "protocolVersion": 1 }),
                    "session/new" => json!({ "sessionId": "test-session" }),
                    "session/prompt" => {
                        let update = json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": { "sessionId": "test-session", "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": { "type": "text", "text": "hello 世界" },
                            } },
                        });
                        output
                            .write_all(format!("{update}\n").as_bytes())
                            .await
                            .expect("update");
                        json!({ "stopReason": "end_turn" })
                    }
                    other => panic!("unexpected request: {other}"),
                };
                let response = json!({ "jsonrpc": "2.0", "id": request["id"], "result": result });
                output
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .expect("response");
            }
            let _ = outcome.send(Some((Termination::Cancelled, None)));
        });
        Ok(StdioProcess {
            stdin: Box::pin(stdin),
            stdout: Box::pin(stdout),
            stderr_tail,
            handle: Box::new(PeerHandle {
                stop,
                status,
                seen: self.seen.clone(),
                transport: AsyncMutex::new(()),
                _workers: workers,
            }),
        })
    }
}

struct PeerHandle {
    stop:      CancellationToken,
    status:    watch::Receiver<Option<(Termination, Option<i32>)>>,
    seen:      Arc<Seen>,
    transport: AsyncMutex<()>,
    _workers:  JoinSet<()>,
}

impl PeerHandle {
    async fn outcome(&self) -> (Termination, Option<i32>) {
        let mut status = self.status.clone();
        let outcome = status
            .wait_for(Option::is_some)
            .await
            .expect("peer outcome");
        outcome.expect("present outcome")
    }
}

#[async_trait]
impl StdioProcessHandle for PeerHandle {
    async fn terminate(&self) {
        self.seen.terminated.fetch_add(1, Ordering::SeqCst);
        let transport = self.transport.lock().await;
        self.stop.cancel();
        drop(transport);
        self.outcome().await;
    }

    async fn wait(&self) -> (Termination, Option<i32>) {
        self.seen.waits.fetch_add(1, Ordering::SeqCst);
        // Like Daytona's status request, a pending wait briefly owns
        // transport state that terminate also needs. It must keep progressing.
        let transport = self.transport.lock().await;
        time::sleep(Duration::from_millis(25)).await;
        drop(transport);
        self.outcome().await
    }
}

#[async_trait]
impl Sandbox for TextOnlySandbox {
    fn id(&self) -> &SandboxId {
        &self.id
    }
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    fn working_directory(&self) -> &'static str {
        "/workspace"
    }
    fn exec(&self) -> &dyn Exec {
        self
    }
    fn fs(&self) -> &dyn Filesystem {
        panic!("stdio does not use filesystem facet")
    }
    async fn describe(&self) -> sandbox_driver::Result<SandboxStatus> {
        panic!("no describe")
    }
    async fn platform_info(&self) -> sandbox_driver::Result<PlatformInfo> {
        panic!("no platform probe")
    }
    async fn start(&self) -> sandbox_driver::Result<()> {
        panic!("already started")
    }
    async fn stop(&self) -> sandbox_driver::Result<()> {
        panic!("stop the process only")
    }
    async fn delete(&self) -> sandbox_driver::Result<()> {
        panic!("delete is lease owned")
    }
}

fn environment(peer: Peer) -> (SandboxEnv, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let mut capabilities = Capabilities::minimal(Isolation::Container);
    capabilities.exec.stdio_process = true;
    capabilities.exec.stdin_stream = false;
    let sandbox = Arc::new(TextOnlySandbox {
        id: SandboxId::try_new("text-only").expect("id"),
        capabilities,
        peer,
        seen: seen.clone(),
    });
    let env = SandboxEnv {
        sandbox,
        host: false,
        workspace: "/workspace".into(),
        ambient: BTreeMap::new(),
        env: BTreeMap::from([
            ("SCOPE".into(), "scope".into()),
            ("OVERRIDE".into(), "scope".into()),
        ]),
        grace: BUDGET,
        host_address: None,
        gate: RunGate::default(),
    };
    (env, seen)
}

async fn client(env: &dyn ExecEnv, spec: ProcessSpec) -> Client {
    let (logs, _progress) = ProgressSender::channel(32);
    let masker = Masker::new();
    masker.register("diagnostic-secret");
    Client::spawn(env, spec, logs, Stage {
        node: "agent".into(),
        firing: FiringId::new(0),
        attempt: Attempt::FIRST,
        scope: ScopeId::new(0),
        masker,
    })
    .await
    .expect("ACP spawn")
}

#[tokio::test]
async fn acp_uses_text_stdio_when_general_streamed_stdin_is_unsupported() {
    let (env, seen) = environment(Peer::Acp);
    let spec = ProcessSpec::new("agent", &["literal ; argument"])
        .with_cwd(Some("/workspace/stage directory".into()))
        .with_env(BTreeMap::from([("OVERRIDE".into(), "process".into())]));
    let mut client = client(&env, spec).await;
    timeout(BUDGET, client.open_session(env.workspace_path()))
        .await
        .expect("handshake deadline")
        .expect("handshake");
    let (_control, mut control) = mpsc::channel(4);
    let turn = timeout(BUDGET, client.prompt("hi", &mut control, env.grace()))
        .await
        .expect("prompt deadline")
        .expect("turn");
    assert_eq!(turn.text, "hello 世界");
    assert_eq!(*seen.methods.lock().expect("methods"), [
        "initialize",
        "session/new",
        "session/prompt"
    ]);
    assert_eq!(seen.streaming.load(Ordering::SeqCst), 0);
    {
        let specs = seen.specs.lock().expect("specs");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].program, "agent");
        assert_eq!(specs[0].args, ["literal ; argument"]);
        assert_eq!(
            specs[0].working_dir.as_deref(),
            Some("/workspace/stage directory")
        );
        assert_eq!(specs[0].env["SCOPE"], "scope");
        assert_eq!(specs[0].env["OVERRIDE"], "process");
    }
    assert_eq!(*seen.mkdir.lock().expect("mkdir"), [vec![
        "-p",
        "/workspace/stage directory"
    ]]);
    timeout(BUDGET, client.terminate(env.grace()))
        .await
        .expect("termination");
    assert_eq!(seen.terminated.load(Ordering::SeqCst), 1);
    assert!(env.gate.close(BUDGET).await);
    assert!(
        env.spawn_text_stdio(ProcessSpec::new("agent", &[]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn ordinary_piped_execution_keeps_its_streaming_contract() {
    let (env, seen) = environment(Peer::Silent);
    let mut process = env
        .spawn(ProcessSpec::new("cat", &[]).with_stdin(StdinMode::Piped))
        .await
        .expect("handle");
    assert!(
        timeout(BUDGET, process.wait())
            .await
            .expect("wait deadline")
            .is_err()
    );
    assert_eq!(seen.streaming.load(Ordering::SeqCst), 1);
    assert!(seen.specs.lock().expect("specs").is_empty());
}

#[tokio::test]
async fn unexpected_acp_exit_reports_status_and_masks_the_stderr_tail() {
    let (env, _) = environment(Peer::Exit);
    let mut client = client(&env, ProcessSpec::new("missing-agent", &[])).await;
    let error = timeout(BUDGET, client.open_session(env.workspace_path()))
        .await
        .expect("deadline")
        .expect_err("exit")
        .to_string();
    assert!(error.contains("exit code 127"), "{error}");
    assert!(error.contains("launch failed:"), "{error}");
    assert!(!error.contains("diagnostic-secret"), "{error}");
}

#[tokio::test]
async fn stdio_deadline_and_dropped_handle_terminate_before_gate_drains() {
    for deadline in [true, false] {
        let (env, seen) = environment(Peer::Silent);
        let mut spec = ProcessSpec::new("agent", &[]);
        if deadline {
            spec = spec.with_timeout(Some(Duration::from_millis(10)));
        }
        let mut process = env.spawn_text_stdio(spec).await.expect("stdio process");
        // Keep both streams after dropping the owner: EOF on stdin must not
        // be mistaken for the adapter requesting remote termination.
        let stdin = process.stdin().expect("stdin");
        let lines = process.lines().expect("lines");
        if deadline {
            let status = timeout(BUDGET, process.wait())
                .await
                .expect("wait deadline")
                .expect("status");
            assert!(status.timed_out);
            assert_eq!(process.wait().await.expect("cached wait"), status);
        }
        drop(process);
        assert!(env.gate.close(BUDGET).await);
        assert_eq!(seen.terminated.load(Ordering::SeqCst), 1);
        assert_eq!(
            seen.waits.load(Ordering::SeqCst),
            1,
            "termination preserves the pending wait"
        );
        drop((stdin, lines));
    }
}
