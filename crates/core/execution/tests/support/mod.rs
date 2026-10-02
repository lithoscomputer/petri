//! A simulated host for the execution layer: the world from
//! [`testkit::sim`] as the sandbox provider behind the runtime's own lease
//! router, a recording clock, step logs and routing draws from the
//! simulation, and an in-memory store that outlives a crash and that a
//! lifetime can crash on, on a single-threaded runtime whose clock starts
//! paused.

#![allow(
    dead_code,
    reason = "each test binary compiles this module in full but uses only some of its helpers"
)]

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use std::{fmt, fs};

use driver::lifecycle::{ExecutionHooks, HookContext, Note, RunFinished, ScopeReleased};
use execution::{
    AttemptAdmission, CallSite, CoordinatorInvocationClient, GraphDigest, InterviewReply,
    InterviewRequest, Interviewer, InvocationClient as _, InvocationRequest, InvocationResult,
    LeaseState, LogId, MemoryRunStore, PendingIntent, ResourceLogRecord, RunKey, RunStore as _,
    SandboxMode, SandboxResourceRecord, SecretBindings,
};
use executor::Retention;
use executor_sandbox::{InProcessProviders, LostSandbox, SandboxBackend, SandboxOptions};
use ir::{FailureInfo, Graph, Outcome, RunStatus, Status, StepKindId};
use runtime::{RunOptions, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use steps::{Answer, Step, StepCtx};
use store::{Access, Digest, Record, RunLogs, StoreError};
use testkit::RunDir;
use testkit::sim::{Dice, Faults, LeaseRecords, LeaseView, MemoryLogs, World, sandboxed_registry};
use tokio::runtime::Builder;
use tokio::sync::Notify;
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

/// The recording clock's reading at the simulation's start, in milliseconds
/// since the Unix epoch.
pub(crate) const SIMULATED_EPOCH_MS: u64 = 1_800_000_000_000;

pub(crate) const GRACE: Duration = Duration::from_millis(100);
pub(crate) const HARD_DEADLINE_SLACK: Duration = Duration::from_millis(300);
pub(crate) const CLEANUP_GRACE: Duration = Duration::from_millis(500);

/// The run's key in the store: fixed, so the logs do not depend on a minted
/// one.
pub(crate) fn run_key() -> RunKey {
    RunKey::new("sim")
}

/// A graph's digest, as the coordinator registers it: over its JSON.
pub(crate) fn digest_of(graph: &Graph) -> GraphDigest {
    GraphDigest::of(
        serde_json::to_string(graph)
            .expect("a graph always encodes")
            .as_bytes(),
    )
}

// ── A step that calls other graphs ────────────────────────────────────────

pub(crate) const INVOKE: StepKindId = StepKindId::new_static("invoke");

/// One child invocation an invoke step starts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Call {
    pub graph:   GraphDigest,
    /// Run in the caller's sandbox instead of one of its own.
    pub inherit: bool,
    /// A fork gate the child waits on for a slot: its name and its slots.
    pub gate:    Option<(String, u32)>,
}

#[derive(Deserialize)]
pub(crate) struct InvokeConfig {
    calls: Vec<Call>,
}

/// What a caller records about one child's result, in its own output: what
/// the simulation checks against the child's recorded result.
pub(crate) fn received(result: &InvocationResult) -> Value {
    json!({
        "status": result.status,
        "final_execution": result.final_execution,
        "output": result.output,
    })
}

/// Starts each call's child invocation, then waits for every result in
/// call order, passing a stop signal on to the children. Its output is what
/// it received; it fails when a child did not succeed.
pub(crate) struct InvokeStep;

#[async_trait::async_trait]
impl Step for InvokeStep {
    type Config = InvokeConfig;

    const NAME: &'static str = "invoke";

    async fn run(&self, config: InvokeConfig, mut ctx: StepCtx) -> Outcome {
        let client = match ctx.require_capability::<CoordinatorInvocationClient>() {
            Ok(client) => client,
            Err(error) => return error.into(),
        };
        let mut handles = Vec::new();
        for (index, call) in config.calls.iter().enumerate() {
            let request = InvocationRequest {
                site:      CallSite {
                    firing:  ctx.firing,
                    attempt: ctx.attempt,
                    slot:    format!("c{index}").into(),
                },
                graph:     call.graph,
                context:   BTreeMap::new(),
                secrets:   SecretBindings::None,
                sandbox:   if call.inherit {
                    SandboxMode::Inherit { scope: ctx.scope }
                } else {
                    SandboxMode::Isolated
                },
                admission: call
                    .gate
                    .as_ref()
                    .map(|(gate, max_parallel)| AttemptAdmission {
                        gate:         gate.as_str().into(),
                        max_parallel: *max_parallel,
                    }),
            };
            match client.start_or_attach(request).await {
                Ok(handle) => handles.push(handle),
                Err(error) => return Outcome::failure(format!("call {index}: {error}")),
            }
        }
        let mut results = Vec::new();
        let mut all_succeeded = true;
        for handle in &mut handles {
            match handle.settled_with_control(&mut ctx.control).await {
                Ok(result) => {
                    all_succeeded &= result.status == RunStatus::Success;
                    results.push(received(&result));
                }
                // The coordinator owns the children's shutdown from here.
                Err(_) => return Outcome::cancelled(),
            }
        }
        let output = Value::Array(results);
        if all_succeeded {
            Outcome::success(output)
        } else {
            Outcome::new(
                Status::Failure(FailureInfo::new("a child invocation did not succeed")),
                output,
            )
        }
    }
}

// ── The simulated host ────────────────────────────────────────────────────

/// How the run keeps its sandboxes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Leases {
    pub retention: Retention,
    pub lost:      LostSandbox,
}

/// What outlives a crash: the store, the world and its step logs.
pub(crate) struct SimHost {
    pub dir:     RunDir,
    pub store:   Arc<MemoryRunStore>,
    /// The store as the runs open it: watched, so a lifetime can crash on a
    /// resource record.
    pub watched: Arc<WatchedStore>,
    pub world:   Arc<World>,
    pub logs:    Arc<MemoryLogs>,
    pub seed:    u64,
    pub leases:  Leases,
    /// The host's run-level hooks, shared by every lifetime.
    pub hooks:   Arc<SimHooks>,
    /// The runtime's clock at the simulation's start.
    pub epoch:   Instant,
}

impl SimHost {
    /// A fresh host, on the runtime whose clock is paused: call inside it.
    /// The world checks every provider call against the store's lease
    /// records.
    pub(crate) fn new(name: &str, seed: u64, faults: Faults) -> Self {
        let store = Arc::new(MemoryRunStore::new());
        let world = World::new(seed, faults);
        world.check_leases(Arc::new(StoredLeases(Arc::clone(&store))));
        Self {
            dir: RunDir::new(name),
            watched: Arc::new(WatchedStore::new(Arc::clone(&store))),
            store,
            world,
            logs: Arc::new(MemoryLogs::default()),
            seed,
            leases: Leases::default(),
            hooks: Arc::new(SimHooks::default()),
            epoch: Instant::now(),
        }
    }

    /// The runtime one coordinator lifetime runs on: the runtime's own lease
    /// router over the world's provider, as that lifetime reaches it, the
    /// invoke step beside the world's step, the simulated clock, the
    /// in-memory step logs and the seed's draws.
    pub(crate) fn runtime(&self) -> Runtime {
        let mut registry = sandboxed_registry();
        registry.register(InvokeStep);
        let epoch = self.epoch;
        let logs = Arc::clone(&self.logs);
        let options = RunOptions {
            grace: GRACE,
            hard_deadline_slack: HARD_DEADLINE_SLACK,
            cleanup_grace: CLEANUP_GRACE,
            run_key: Some(run_key()),
            retention: self.leases.retention,
            sandbox: SandboxOptions {
                backend: SandboxBackend::Docker,
                lost_sandbox: self.leases.lost,
                ..SandboxOptions::default()
            },
            ..RunOptions::new(self.dir.path())
        };
        Runtime::bare()
            .steps(registry)
            .in_process_providers(InProcessProviders::new().with(Arc::new(self.world.factory())))
            .store(Arc::clone(&self.watched) as Arc<dyn store::RunStore>)
            .options(options)
            .recording_clock(driver::RecordingClock::new(move || {
                SIMULATED_EPOCH_MS + u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
            }))
            .step_logs(move |_| Arc::clone(&logs) as Arc<dyn driver::StepLogStore>)
            .decision_seed(self.seed)
    }

    /// [`Self::stored`], or nothing when the run was never stored.
    pub(crate) async fn stored_or_empty(&self) -> StoredLogs {
        match self.store.open(&run_key(), Access::Read).await {
            Ok(_) => self.stored().await,
            Err(_) => StoredLogs {
                coordinator: Vec::new(),
                resources:   Vec::new(),
                executions:  BTreeMap::new(),
            },
        }
    }

    /// Every log the store holds, each record as the line the store keeps.
    pub(crate) async fn stored(&self) -> StoredLogs {
        let logs = self
            .store
            .open(&run_key(), Access::Read)
            .await
            .expect("the run is in the store");
        let lines = |records: Vec<store::Record>| -> Vec<String> {
            records
                .into_iter()
                .map(|record| serde_json::to_string(&record.record).expect("a record encodes"))
                .collect()
        };
        let coordinator = lines(
            logs.read(&LogId::Coordinator)
                .await
                .expect("the coordinator log reads"),
        );
        let resources = lines(
            logs.read(&LogId::Resources)
                .await
                .expect("the resource log reads"),
        );
        let mut executions = BTreeMap::new();
        for record in execution::read_coordinator_log(&*logs)
            .await
            .expect("the coordinator log decodes")
        {
            if let execution::CoordinatorEvent::ExecutionDeclared { execution, .. } = record.body {
                let records = logs
                    .read(&LogId::Execution(execution))
                    .await
                    .expect("an execution log reads");
                executions.insert(execution.raw(), lines(records));
            }
        }
        StoredLogs {
            coordinator,
            resources,
            executions,
        }
    }
}

/// The host's person, as the seed plays them. Most answer after a short
/// delay. Some take longer than the stall budget, so the watchdog must stay
/// parked. For a question with a deadline, some never answer, so the step's
/// deadline expires. The roll is per execution, question and ask, so it does
/// not depend on the order questions arrive in, and a question asked again
/// after a crash is treated the same way.
pub(crate) struct SimInterviewer {
    pub seed: u64,
}

#[async_trait::async_trait]
impl Interviewer for SimInterviewer {
    async fn reply(&self, request: InterviewRequest, cancel: CancellationToken) -> InterviewReply {
        let mut roll = self.seed ^ request.execution.raw().wrapping_mul(0x9E37_79B9_7F4A_7C15);
        for byte in request.question.id.bytes() {
            roll = (roll ^ u64::from(byte)).wrapping_mul(0x0100_0000_01B3);
        }
        let roll = Dice(roll ^ u64::from(request.ask)).roll(100);
        if request.question.timeout_ms.is_some() && roll < 20 {
            cancel.cancelled().await;
            return InterviewReply::Cancelled;
        }
        let delay = if roll >= 75 {
            150 + (roll - 75) * 8
        } else {
            roll % 60
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => InterviewReply::Cancelled,
            () = time::sleep(Duration::from_millis(delay)) => {
                InterviewReply::Answered(Answer::text("go"))
            }
        }
    }
}

/// One run-level hook call: the point, the execution that ran it, and the
/// scope it released.
pub(crate) type HookCall = (&'static str, u64, Option<u64>);

/// The host's run-level hooks: `run_finished` and `scope_released` each
/// return one note naming its execution (and scope), and every call is
/// counted, across lifetimes. A call from a lifetime whose successor took
/// the run is a zombie's, kept apart: its note can never be recorded.
#[derive(Default)]
pub(crate) struct SimHooks {
    calls:   Mutex<Vec<HookCall>>,
    zombies: Mutex<Vec<HookCall>>,
}

impl SimHooks {
    pub(crate) fn calls(&self) -> Vec<HookCall> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The calls zombies made after their successor took the run.
    pub(crate) fn zombie_calls(&self) -> Vec<HookCall> {
        self.zombies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The hooks one lifetime's runtime installs.
    pub(crate) fn for_lifetime(
        self: &Arc<Self>,
        world: Arc<World>,
        lifetime: u32,
    ) -> Arc<dyn ExecutionHooks> {
        Arc::new(LifetimeHooks {
            hooks: Arc::clone(self),
            world,
            lifetime,
        })
    }

    fn note(&self, call: HookCall, zombie: bool) -> Vec<Note> {
        let calls = if zombie { &self.zombies } else { &self.calls };
        calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
        let (kind, execution, scope) = call;
        vec![Note::new(
            kind,
            json!({"execution": execution, "scope": scope}),
        )]
    }
}

/// One lifetime's view of the hooks: it knows when its successor took the
/// run.
struct LifetimeHooks {
    hooks:    Arc<SimHooks>,
    world:    Arc<World>,
    lifetime: u32,
}

impl LifetimeHooks {
    fn note(&self, call: HookCall) -> Vec<Note> {
        let zombie = self.world.lifetime() > self.lifetime;
        self.hooks.note(call, zombie)
    }
}

#[async_trait::async_trait]
impl ExecutionHooks for LifetimeHooks {
    async fn run_finished(&self, context: &HookContext, _finished: RunFinished) -> Vec<Note> {
        self.note(("sim.run", context.execution.raw(), None))
    }

    async fn scope_released(&self, context: &HookContext, released: ScopeReleased) -> Vec<Note> {
        self.note((
            "sim.scope",
            context.execution.raw(),
            Some(u64::from(released.scope.raw())),
        ))
    }
}

/// The run's lease records, as the world checks its provider calls against
/// them: the latest record of each lease in the store's resource log.
pub(crate) struct StoredLeases(pub Arc<MemoryRunStore>);

impl fmt::Debug for StoredLeases {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StoredLeases")
    }
}

#[async_trait::async_trait]
impl LeaseRecords for StoredLeases {
    async fn lease(&self, lease: &str) -> Option<LeaseView> {
        let logs = self.0.open(&run_key(), Access::Read).await.ok()?;
        let records = logs.read(&LogId::Resources).await.ok()?;
        records
            .iter()
            .rev()
            .filter_map(|record| record.decode::<ResourceLogRecord>().ok())
            .find(|record| record.body.lease.raw().to_string() == lease)
            .map(|record| LeaseView {
                state:       record.body.state,
                pending:     record.body.pending,
                fingerprint: record.body.fingerprint.is_some(),
                resource_id: record.body.resource_id.map(|id| id.to_string()),
            })
    }
}

/// A resource record's kind, as a crash trigger names it.
pub(crate) fn resource_kind(record: &SandboxResourceRecord) -> &'static str {
    match (record.state, record.pending) {
        (_, Some(PendingIntent::Stop)) => "pending stop",
        (_, Some(PendingIntent::Delete)) => "pending delete",
        (LeaseState::Allocating, None) if record.fingerprint.is_some() => "allocating",
        (LeaseState::Allocating, None) => "reserved",
        (LeaseState::Live, None) => "live",
        (LeaseState::Stopped, None) => "stopped",
        (LeaseState::Deleted, None) => "deleted",
    }
}

/// A crash armed for one lifetime: after the `nth` resource record of a
/// kind, counting from 0.
struct Armed {
    kind:  &'static str,
    nth:   usize,
    seen:  usize,
    crash: Arc<Notify>,
}

/// The log kind a store fault lands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FaultLog {
    /// A coordinator record after the run's creation (`run.started` and
    /// `graph.registered` are not counted).
    Coordinator,
    Resources,
    /// Any execution's engine log.
    Execution,
}

/// A store fault for one lifetime: the `nth` append to a log of `log`'s
/// kind, counting from 0, fails before its records are stored or, for a
/// lost reply, after. A store that goes `down` fails every later write of
/// the lifetime too.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StoreFault {
    pub log:        FaultLog,
    pub nth:        usize,
    pub lost_reply: bool,
    pub down:       bool,
}

/// What one lifetime's store handles watch for.
#[derive(Default)]
struct Plan {
    crash: Option<Armed>,
    fault: Option<StoreFault>,
    seen:  usize,
    fired: bool,
}

/// Where a fault lands on one append.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Landing {
    Before,
    After,
}

impl Plan {
    /// Count an append for the fault, and say where the fault lands on it.
    fn landing(&mut self, log: &LogId, records: &[Record]) -> Option<Landing> {
        let fault = self.fault?;
        if self.fired {
            return fault.down.then_some(Landing::Before);
        }
        let counted = match (fault.log, log) {
            (FaultLog::Coordinator, LogId::Coordinator) => records.iter().any(|record| {
                !matches!(
                    record.record["body"]["event"].as_str(),
                    Some("run.started" | "graph.registered")
                )
            }),
            (FaultLog::Resources, LogId::Resources)
            | (FaultLog::Execution, LogId::Execution(_)) => true,
            _ => false,
        };
        if !counted {
            return None;
        }
        self.seen += 1;
        if self.seen - 1 != fault.nth {
            return None;
        }
        self.fired = true;
        Some(if fault.lost_reply {
            Landing::After
        } else {
            Landing::Before
        })
    }
}

/// The in-memory store as the runs open it, watched: a lifetime can crash
/// right after a resource record of a chosen kind is durable, and a planned
/// store fault can fail one of its appends. Each handle keeps the plan of
/// the lifetime that opened it, so a lifetime still running after its
/// successor took the run does not share the successor's.
pub(crate) struct WatchedStore {
    inner:   Arc<MemoryRunStore>,
    plan:    Mutex<Arc<Mutex<Plan>>>,
    /// Each append waits up to this long, from these draws.
    latency: Arc<Mutex<Option<(Dice, u64)>>>,
}

impl WatchedStore {
    fn new(inner: Arc<MemoryRunStore>) -> Self {
        Self {
            inner,
            plan: Mutex::new(Arc::new(Mutex::new(Plan::default()))),
            latency: Arc::new(Mutex::new(None)),
        }
    }

    /// Make every append wait up to `most` milliseconds, as a store under
    /// load does.
    pub(crate) fn slow(&self, seed: u64, most: u64) {
        *self.latency.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Dice(seed ^ 0x5_10E5), most));
    }

    /// The next lifetime's plan: crash it right after the `nth` record of a
    /// kind, by notifying `crash`, or never, for `None`; and fail one of its
    /// appends as `fault` says.
    pub(crate) fn arm(
        &self,
        trigger: Option<(&'static str, usize)>,
        crash: Arc<Notify>,
        fault: Option<StoreFault>,
    ) {
        *self.plan.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(Mutex::new(Plan {
            crash: trigger.map(|(kind, nth)| Armed {
                kind,
                nth,
                seen: 0,
                crash,
            }),
            fault,
            ..Plan::default()
        }));
    }

    /// Whether the current lifetime's store fault has landed.
    pub(crate) fn fault_fired(&self) -> bool {
        self.plan
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fired
    }
}

#[async_trait::async_trait]
impl store::RunStore for WatchedStore {
    async fn open(&self, key: &RunKey, access: Access) -> Result<Arc<dyn RunLogs>, StoreError> {
        let inner = self.inner.open(key, access).await?;
        let plan = Arc::clone(&self.plan.lock().unwrap_or_else(PoisonError::into_inner));
        Ok(Arc::new(WatchedLogs {
            inner,
            plan,
            latency: Arc::clone(&self.latency),
        }))
    }
}

struct WatchedLogs {
    inner:   Arc<dyn RunLogs>,
    plan:    Arc<Mutex<Plan>>,
    latency: Arc<Mutex<Option<(Dice, u64)>>>,
}

impl WatchedLogs {
    fn plan(&self) -> MutexGuard<'_, Plan> {
        self.plan.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn failure(&self, action: &'static str) -> StoreError {
        StoreError::backend(self.inner.locator(), action, "simulated store failure")
    }
}

#[async_trait::async_trait]
impl RunLogs for WatchedLogs {
    fn locator(&self) -> String {
        self.inner.locator()
    }

    async fn append(&self, log: &LogId, records: &[Record]) -> Result<(), StoreError> {
        let delay = self
            .latency
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .map(|(dice, most)| dice.roll(*most + 1));
        if let Some(delay) = delay {
            time::sleep(Duration::from_millis(delay)).await;
        }
        let landing = self.plan().landing(log, records);
        if landing == Some(Landing::Before) {
            return Err(self.failure("append"));
        }
        self.inner.append(log, records).await?;
        if *log == LogId::Resources {
            let mut plan = self.plan();
            if let Some(armed) = plan.crash.as_mut() {
                for record in records {
                    let Ok(line) = record.decode::<ResourceLogRecord>() else {
                        continue;
                    };
                    if resource_kind(&line.body) != armed.kind {
                        continue;
                    }
                    if armed.seen == armed.nth {
                        armed.crash.notify_one();
                    }
                    armed.seen += 1;
                }
            }
        }
        if landing == Some(Landing::After) {
            return Err(self.failure("append"));
        }
        Ok(())
    }

    async fn read(&self, log: &LogId) -> Result<Vec<Record>, StoreError> {
        self.inner.read(log).await
    }

    async fn read_from(&self, log: &LogId, seq: u64) -> Result<Vec<Record>, StoreError> {
        self.inner.read_from(log, seq).await
    }

    async fn put_blob(&self, bytes: &[u8]) -> Result<Digest, StoreError> {
        let down = {
            let plan = self.plan();
            plan.fired && plan.fault.is_some_and(|fault| fault.down)
        };
        if down {
            return Err(self.failure("put blob"));
        }
        self.inner.put_blob(bytes).await
    }

    async fn get_blob(&self, digest: Digest) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get_blob(digest).await
    }
}

/// Every file under `dir`, recursively.
pub(crate) fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Every log of a run, as its stored lines.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StoredLogs {
    pub coordinator: Vec<String>,
    pub resources:   Vec<String>,
    /// Each execution's engine log, by execution id.
    pub executions:  BTreeMap<u64, Vec<String>>,
}

/// Run `future` on a fresh single-threaded runtime whose clock starts paused
/// and moves only when every task waits.
pub(crate) fn run_paused<F: Future>(future: F) -> F::Output {
    Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("a current-thread runtime builds")
        .block_on(future)
}

/// Sleep until `at` after `epoch`, on the paused clock.
pub(crate) async fn sleep_until(epoch: Instant, at: Duration) {
    time::sleep_until(epoch + at).await;
}
