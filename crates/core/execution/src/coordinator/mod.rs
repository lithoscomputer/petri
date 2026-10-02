//! The run coordinator: one run's lifecycle log, its engines, its
//! resource boundary and its observers, driven from one loop.

mod gate;
mod handle;
mod leases;
mod live;
mod result;
mod start;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::{fs, io};

use driver::{ExecutionSlot, HookContext, ParentLink};
use engine::{EngineExit, EngineStart, EntryPoint, Event, MiddlewareKey};
use ir::{Graph, RunStatus, Value};
use runtime::{RunAccess, RunRuntime};
use smol_str::SmolStr;
use store::{LogId, RunLogs, execution_relative_dir};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, OwnedSemaphorePermit, mpsc, watch};
use tokio::task::{JoinError, JoinSet};

use self::gate::{AdmittedSlot, ForkGate, GateKey};
pub use self::handle::CoordinatorHandle;
use self::handle::{ControlRequest, PauseRequest};
use self::leases::{ExecutionLeases, InvocationReleased, validate_resources};
use self::live::LiveInvocations;
use self::result::{project_result, rebuild_middleware};
use crate::client::StartRequest;
use crate::{
    CancelRequest, CoordinatorEvent, CoordinatorInvocationClient, CoordinatorRecord,
    CoordinatorStore, EngineLogError, ExecutionId, ExecutionLogWriter, ExecutionObserver,
    GraphDigest, InvocationId, InvocationResult, InvocationSecrets, InvocationStatus, Middleware,
    MiddlewarePipeline, MiddlewareState, ResourceError, ResourceLedger, ResourceStore,
    SandboxBinding, SecretBindings, StoreError, StoreWriter, initial_middleware_state,
    read_execution_log,
};

pub const DEFAULT_MAX_INVOCATIONS: u32 = 1024;

/// The hard ceiling on invocations in one run: the root plus every nested
/// and branch invocation, finished, failed and cancelled ones included. No
/// option raises it and none disables it; a lower limit is allowed.
pub const MAX_INVOCATIONS: u32 = 10_000;

/// A requested invocation limit the coordinator refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvocationLimitError {
    #[error("the invocation limit cannot be disabled; it must be at least 1")]
    Disabled,
    #[error("the invocation limit {requested} is above the hard ceiling of {ceiling}")]
    AboveCeiling { requested: u32, ceiling: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoordinatorOptions {
    pub max_invocations: u32,
    pub max_executions:  u32,
}

impl CoordinatorOptions {
    /// Lower the invocation limit. Zero (no limit) and anything above
    /// [`MAX_INVOCATIONS`] are refused.
    pub fn with_max_invocations(mut self, limit: u32) -> Result<Self, InvocationLimitError> {
        Self::check_limit(limit)?;
        self.max_invocations = limit;
        Ok(self)
    }

    fn check_limit(limit: u32) -> Result<(), InvocationLimitError> {
        if limit == 0 {
            return Err(InvocationLimitError::Disabled);
        }
        if limit > MAX_INVOCATIONS {
            return Err(InvocationLimitError::AboveCeiling {
                requested: limit,
                ceiling:   MAX_INVOCATIONS,
            });
        }
        Ok(())
    }
}

impl Default for CoordinatorOptions {
    fn default() -> Self {
        Self {
            max_invocations: DEFAULT_MAX_INVOCATIONS,
            max_executions:  engine::DEFAULT_MAX_EXECUTIONS,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The run could not be opened in its store.
    #[error(transparent)]
    Open(#[from] store::StoreError),
    #[error(transparent)]
    EngineLog(#[from] EngineLogError),
    #[error(transparent)]
    Resume(#[from] driver::ResumeError),
    #[error(transparent)]
    Resource(#[from] ResourceError),
    #[error("unknown graph {0}")]
    UnknownGraph(GraphDigest),
    #[error("the root invocation is already declared with a different request")]
    RootRequestMismatch,
    #[error(
        "the run declares {total} invocations, above its limit of {limit}; the limit cannot be \
         lowered below what the run already holds"
    )]
    InvocationLimit { total: u64, limit: u32 },
    #[error(transparent)]
    InvalidInvocationLimit(#[from] InvocationLimitError),
    #[error("the calling firing has no inheritable sandbox")]
    NoInheritableSandbox,
    #[error("sandbox lease {lease} does not match its invocation graph")]
    InvalidResource { lease: crate::SandboxLeaseId },
    #[error("invocation {0} has no execution")]
    MissingExecution(InvocationId),
    #[error("execution {0} finished without an engine exit")]
    MissingExit(ExecutionId),
    #[error("invocation {invocation} reached its execution limit")]
    ExecutionLimit { invocation: InvocationId },
    #[error("execution {execution} log disagrees with the coordinator log")]
    ConflictingExit { execution: ExecutionId },
    #[error("execution {execution} event writer failed: {message}")]
    EventWriter {
        execution: ExecutionId,
        message:   String,
    },
    #[error("execution task failed: {0}")]
    ExecutionTask(#[from] JoinError),
    /// A write to the run's store failed: this lifetime ends, recording
    /// nothing more, and the next resumes from what the store holds.
    #[error("the run's store failed: {0}")]
    StoreFailed(String),
    #[error("a resolved secret appears in durable invocation data")]
    SecretInDurableData,
    #[error("could not inspect durable invocation data: {0}")]
    EncodeDurableData(#[source] serde_json::Error),
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
}

struct PreparedExecution {
    driver:            driver::Driver,
    pipeline:          Arc<MiddlewarePipeline>,
    cancel_before_run: bool,
    /// The fork slot a gated child holds while it is live, released once
    /// its `ExecutionFinished` is recorded.
    slot:              Option<ExecutionSlot>,
    /// What the execution's isolated scopes acquire, for a child that
    /// inherits one of them.
    leases:            Option<ExecutionLeases>,
}

/// A driver has stopped; its invocation can finish after its descendants
/// settle.
struct CompletedExecution {
    invocation:       InvocationId,
    execution:        ExecutionId,
    graph:            Arc<Graph>,
    report:           driver::ExecutionReport,
    middleware_state: MiddlewareState,
    slot:             Option<ExecutionSlot>,
}

/// Owns one run's lifecycle log, engines, resource boundary, and observers.
pub struct Coordinator {
    store:            CoordinatorStore,
    /// The run's store writer: every execution's records go through it.
    writer:           Arc<StoreWriter>,
    runtime:          RunRuntime,
    options:          CoordinatorOptions,
    observers:        Vec<Arc<dyn ExecutionObserver>>,
    start_tx:         mpsc::Sender<StartRequest>,
    start_rx:         mpsc::Receiver<StartRequest>,
    cancel_tx:        mpsc::UnboundedSender<CancelRequest>,
    cancel_rx:        mpsc::UnboundedReceiver<CancelRequest>,
    control_tx:       mpsc::UnboundedSender<ControlRequest>,
    control_rx:       mpsc::UnboundedReceiver<ControlRequest>,
    pause_tx:         mpsc::UnboundedSender<PauseRequest>,
    pause_rx:         mpsc::UnboundedReceiver<PauseRequest>,
    admit_tx:         mpsc::UnboundedSender<AdmittedSlot>,
    admit_rx:         mpsc::UnboundedReceiver<AdmittedSlot>,
    /// The status of every invocation a start request attached to; it
    /// outlives the invocation's liveness, so a late attach still finds it.
    statuses:         BTreeMap<InvocationId, watch::Sender<InvocationStatus>>,
    /// The invocations between their dispatch and their release.
    live:             LiveInvocations,
    middleware:       Vec<Arc<dyn Middleware>>,
    last_root_report: Option<driver::ExecutionReport>,
    /// The durable lease records, shared with the executor's lease manager
    /// as its ledger.
    resources:        Arc<AsyncMutex<ResourceStore>>,
    /// Fork gates by parent execution and gate name: the slots every child
    /// invocation declared under that name shares, and the children queued
    /// for one. Rebuilt on demand, so a resume recovers the accounting from
    /// the children it redispatches.
    gates:            BTreeMap<GateKey, ForkGate>,
    /// Whether this coordinator continues a stored run: an observer that
    /// attaches then is handed the replayed state first.
    resumed:          bool,
    #[cfg(test)]
    release_gate:     Option<Arc<tests::ReleaseGate>>,
}

impl Coordinator {
    /// Start a fresh run. `middleware` may be empty; the configured chain's
    /// keys are recorded durably either way. A run that cannot start is
    /// finished: its services and providers are torn down.
    pub async fn create(
        runtime: RunRuntime,
        middleware: Vec<Arc<dyn Middleware>>,
        options: CoordinatorOptions,
    ) -> Result<Self, CoordinatorError> {
        match Self::open_created(&runtime, &middleware, &options).await {
            Ok((store, resources)) => Ok(Self::assemble(
                store, resources, runtime, middleware, options, false,
            )),
            Err(error) => {
                runtime.finish().await;
                Err(error)
            }
        }
    }

    async fn open_created(
        runtime: &RunRuntime,
        middleware: &[Arc<dyn Middleware>],
        options: &CoordinatorOptions,
    ) -> Result<(CoordinatorStore, ResourceStore), CoordinatorError> {
        CoordinatorOptions::check_limit(options.max_invocations)?;
        let keys = middleware.iter().map(|item| item.key()).collect();
        // A crash can cut a creation short before `run.started`: the key is
        // stored with an empty log, and creating under it takes it over.
        let logs = match runtime.open(RunAccess::Create).await {
            Err(exists @ store::StoreError::Exists { .. }) => {
                let logs = runtime.open(RunAccess::Write).await?;
                if !logs.read(&LogId::Coordinator).await?.is_empty() {
                    return Err(exists.into());
                }
                logs
            }
            created => created?,
        };
        let clock = runtime.recording_clock();
        let store = CoordinatorStore::create_with_clock(
            logs.clone(),
            runtime.run_key().clone(),
            keys,
            clock.clone(),
        )
        .await?;
        let resources = ResourceStore::load(&logs).await?.with_clock(clock);
        Ok((store, resources))
    }

    /// Resume a crashed run. `middleware` must match the recorded chain. A
    /// run that cannot resume is finished: its services and providers are
    /// torn down, and its leases are left as recorded.
    pub async fn resume(
        runtime: RunRuntime,
        middleware: Vec<Arc<dyn Middleware>>,
        options: CoordinatorOptions,
    ) -> Result<Self, CoordinatorError> {
        let (store, resources) = match Self::open_resumed(&runtime, &middleware, &options).await {
            Ok(opened) => opened,
            Err(error) => {
                runtime.finish().await;
                return Err(error);
            }
        };
        let coordinator = Self::assemble(store, resources, runtime, middleware, options, true);
        coordinator.reconcile_leases().await;
        // A lease record reconciliation could not write ends the lifetime
        // before anything runs.
        if let Err(error) = coordinator.check_store() {
            coordinator.runtime.finish().await;
            return Err(error);
        }
        Ok(coordinator)
    }

    async fn open_resumed(
        runtime: &RunRuntime,
        middleware: &[Arc<dyn Middleware>],
        options: &CoordinatorOptions,
    ) -> Result<(CoordinatorStore, ResourceStore), CoordinatorError> {
        CoordinatorOptions::check_limit(options.max_invocations)?;
        let logs = runtime.open(RunAccess::Write).await?;
        let clock = runtime.recording_clock();
        let mut store = CoordinatorStore::resume(logs.clone(), runtime.run_key().clone())
            .await?
            .with_clock(clock.clone());
        let resources = ResourceStore::load(&logs).await?.with_clock(clock);
        let keys: Vec<MiddlewareKey> = middleware.iter().map(|item| item.key()).collect();
        if store.state().middleware_chain != keys {
            return Err(StoreError::State(crate::StateError::MiddlewareChain).into());
        }
        let total = store.state().invocations.len() as u64;
        if total > u64::from(options.max_invocations) {
            return Err(CoordinatorError::InvocationLimit {
                total,
                limit: options.max_invocations,
            });
        }
        validate_resources(&mut store, &resources).await?;
        // A finished invocation is never dispatched again, so its inherited
        // lease need not be in this run's ledger: a forked run carries the
        // source's finished children, whose leases were the source's
        // (`FORK.md`).
        for lease in store
            .state()
            .invocations
            .values()
            .filter(|invocation| invocation.result.is_none())
            .filter_map(|invocation| match invocation.declaration.sandbox {
                SandboxBinding::Inherited { lease } => Some(lease),
                SandboxBinding::Isolated => None,
            })
        {
            resources.resolve(lease)?;
        }
        Ok((store, resources))
    }

    fn assemble(
        store: CoordinatorStore,
        resources: ResourceStore,
        runtime: RunRuntime,
        middleware: Vec<Arc<dyn Middleware>>,
        options: CoordinatorOptions,
        resumed: bool,
    ) -> Self {
        let (start_tx, start_rx) = mpsc::channel(128);
        let (cancel_tx, cancel_rx) = mpsc::unbounded_channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (pause_tx, pause_rx) = mpsc::unbounded_channel();
        let (admit_tx, admit_rx) = mpsc::unbounded_channel();
        // The records are the executor's ledger from here on: every
        // container scope it allocates is written here before it exists.
        let writer = StoreWriter::start(store.logs());
        let resources = Arc::new(AsyncMutex::new(resources.with_failure(writer.failure())));
        runtime.attach_lease_ledger(Arc::new(ResourceLedger::new(resources.clone())));
        Self {
            store,
            writer,
            runtime,
            options,
            observers: Vec::new(),
            start_tx,
            start_rx,
            cancel_tx,
            cancel_rx,
            control_tx,
            control_rx,
            pause_tx,
            pause_rx,
            admit_tx,
            admit_rx,
            statuses: BTreeMap::new(),
            live: LiveInvocations::default(),
            middleware,
            last_root_report: None,
            resources,
            gates: BTreeMap::new(),
            resumed,
            #[cfg(test)]
            release_gate: None,
        }
    }

    /// The invocation limit this run enforces.
    pub fn max_invocations(&self) -> u32 {
        self.options.max_invocations
    }

    /// Register an observer of every execution's records and every
    /// coordinator record. A fresh run's opening record (`RunStarted`) was
    /// appended before any observer could attach; it is delivered here, so
    /// the observer sees the coordinator log from its first record.
    #[must_use]
    pub fn observe(mut self, observer: Arc<dyn ExecutionObserver>) -> Self {
        for record in self.store.opening_records() {
            observer.on_lifecycle(record);
        }
        if self.resumed {
            observer.on_resumed(self.store.state());
        }
        self.observers.push(observer);
        self
    }

    pub fn store(&self) -> &CoordinatorStore {
        &self.store
    }

    /// A registered graph, decoded and validated once and cached by digest.
    pub async fn load_graph(
        &mut self,
        digest: GraphDigest,
    ) -> Result<Arc<Graph>, CoordinatorError> {
        Ok(self.store.load_graph(digest).await?)
    }

    /// The directory an execution's own files live under: its step output
    /// and, in the run-directory store, its engine log.
    pub fn execution_dir(&self, execution: ExecutionId) -> PathBuf {
        self.runtime
            .run_dir()
            .join(execution_relative_dir(execution))
    }

    pub fn take_root_report(&mut self) -> Option<driver::ExecutionReport> {
        self.last_root_report.take()
    }

    pub async fn register_graph(&mut self, graph: &Graph) -> Result<GraphDigest, CoordinatorError> {
        let encoded = serde_json::to_string(graph).map_err(CoordinatorError::EncodeDurableData)?;
        self.refuse_secret_bytes(&encoded)?;
        let (digest, record) = self.store.register_graph_bytes(encoded.as_bytes()).await?;
        if let Some(record) = record {
            for observer in &self.observers {
                observer.on_lifecycle(&record);
            }
        }
        Ok(digest)
    }

    pub async fn run_root(
        &mut self,
        graph: GraphDigest,
        context: BTreeMap<SmolStr, Value>,
    ) -> Result<InvocationResult, CoordinatorError> {
        if !self.store.state().graphs.contains(&graph) {
            return Err(CoordinatorError::UnknownGraph(graph));
        }
        self.refuse_secret(&context)?;
        if self
            .store
            .state()
            .invocations
            .contains_key(&InvocationId::ROOT)
        {
            let declaration = &self.store.state().invocations[&InvocationId::ROOT].declaration;
            if declaration.graph != graph || declaration.context != context {
                return Err(CoordinatorError::RootRequestMismatch);
            }
        } else {
            self.append(CoordinatorEvent::InvocationDeclared {
                invocation: InvocationId::ROOT,
                call: None,
                graph,
                context,
                secret_bindings: SecretBindings::None,
                sandbox: SandboxBinding::Isolated,
                admission: None,
            })
            .await?;
        }

        if let Some(result) = self.store.state().invocations[&InvocationId::ROOT]
            .result
            .clone()
        {
            let execution = result.final_execution;
            let declaration = self.store.state().executions[&execution]
                .declaration
                .clone();
            let registered = self.store.load_graph(graph).await?;
            let recorded = self.store.state().executions[&execution]
                .exit
                .clone()
                .ok_or(CoordinatorError::MissingExit(execution))?;
            let PreparedExecution { driver, .. } = self
                .prepare_execution(
                    InvocationId::ROOT,
                    execution,
                    &declaration.start,
                    declaration.middleware_state,
                    &registered,
                    None,
                )
                .await?;
            let report = driver.run().await;
            Self::check_report(execution, &report)?;
            self.append_run_notes(execution, &report).await?;
            if report.exit != recorded {
                return Err(CoordinatorError::ConflictingExit { execution });
            }
            self.last_root_report = Some(report);
            self.run_invocations().await?;
            // A crash can land between the root's result and the run's end:
            // the resumed run ends it.
            self.finish_run(result.status).await?;
            return Ok(result);
        }
        let result = self.run_invocations().await?;
        self.finish_run(result.status).await?;
        Ok(result)
    }

    /// Append the run's end, unless the log already has it. Every lease
    /// still holding a sandbox is released first, so each release is
    /// recorded and `run.finished` stays the log's last record.
    async fn finish_run(&mut self, status: RunStatus) -> Result<(), CoordinatorError> {
        if self.store.state().run_status.is_none() {
            self.release_remaining(status).await;
            self.append(CoordinatorEvent::RunFinished { status })
                .await?;
        }
        Ok(())
    }

    /// Release every lease still holding a sandbox — one whose invocation
    /// finished before a crash, one whose release failed or never ran, or
    /// one whose invocation never finished — each with its owner's status,
    /// or `status` for an owner with none, and record each release, and
    /// each one a crash cut off before its record.
    async fn release_remaining(&mut self, status: RunStatus) {
        let remaining: Vec<_> = self
            .resources()
            .await
            .records()
            .filter(|record| record.needs_release())
            .map(|record| {
                let owner_status = self
                    .store
                    .state()
                    .invocations
                    .get(&record.allocation.invocation)
                    .and_then(|invocation| invocation.result.as_ref())
                    .map_or(status, |result| result.status);
                (record.lease, owner_status)
            })
            .collect();
        for (lease, owner_status) in remaining {
            let release = self.release_lease(lease, owner_status).await;
            let invocation = self
                .resources()
                .await
                .resolve(lease)
                .map(|record| record.allocation.invocation)
                .ok();
            if let Some(invocation) = invocation
                && let Err(error) = self.append_scope_released(invocation, release).await
            {
                tracing::warn!(%error, lease = lease.raw(), "the scope's release was not recorded");
            }
        }
        self.record_cut_off_releases(status).await;
    }

    /// End the run: tear the run services down and stop the store writer.
    /// A run that ended released its leases before `run.finished`; one that
    /// stopped short of its end (an error, the host's stop) releases them
    /// here, and records each release. The run's store handle comes back,
    /// still holding the lease, so the caller can read the finished run
    /// through it.
    pub async fn finish(mut self) -> Arc<dyn RunLogs> {
        if self.store.state().run_status.is_none() {
            self.release_remaining(RunStatus::Cancelled).await;
        }
        self.runtime.finish().await;
        if let Err(error) = self.writer.shutdown().await {
            tracing::warn!(%error, "the store writer stopped with an error");
        }
        self.store.logs().clone()
    }

    async fn resources(&self) -> MutexGuard<'_, ResourceStore> {
        self.resources.lock().await
    }

    /// Service every execution from one loop. A request can come from any live
    /// driver, so scheduling it must not make it a child of another sibling.
    async fn run_invocations(&mut self) -> Result<InvocationResult, CoordinatorError> {
        let mut running = JoinSet::new();
        let mut releasing = JoinSet::new();
        let result = self.drive_invocations(&mut running, &mut releasing).await;
        // On an error, wait for the aborted driver futures to drop before
        // the caller continues with run cleanup.
        running.shutdown().await;
        releasing.shutdown().await;
        self.live.clear();
        result
    }

    async fn drive_invocations(
        &mut self,
        running: &mut JoinSet<CompletedExecution>,
        releasing: &mut JoinSet<InvocationReleased>,
    ) -> Result<InvocationResult, CoordinatorError> {
        let mut completed = BTreeMap::new();
        self.finish_cut_off_cancels().await?;
        if self.store.state().invocations[&InvocationId::ROOT]
            .result
            .is_some()
        {
            self.settle_descendants(InvocationId::ROOT, running).await?;
        } else {
            self.start_invocation(InvocationId::ROOT, running).await?;
        }

        let failure = self.writer.failure();
        loop {
            if self.live.is_empty()
                && let Some(result) = self.store.state().invocations[&InvocationId::ROOT]
                    .result
                    .clone()
            {
                return Ok(result);
            }
            // In a fixed order, so the same inputs append the same records: a
            // failed store write first, since it ends the lifetime, then the
            // host's commands, then what ended, then new work.
            tokio::select! {
                biased;
                first = failure.wait() => {
                    return Err(CoordinatorError::StoreFailed(first.to_owned()));
                }
                cancelled = self.cancel_rx.recv() => {
                    if let Some(cancelled) = cancelled {
                        self.handle_cancel(cancelled).await?;
                    }
                }
                request = self.pause_rx.recv() => {
                    if let Some(request) = request {
                        self.handle_pause(request).await?;
                    }
                }
                request = self.control_rx.recv() => {
                    if let Some(request) = request {
                        self.handle_control(request);
                    }
                }
                result = running.join_next(), if !running.is_empty() => {
                    let done = result.expect("the execution set is not empty")?;
                    let done = self.on_execution_done(done, running).await?;
                    completed.insert(done.invocation, done);
                }
                result = releasing.join_next(), if !releasing.is_empty() => {
                    let released = result.expect("the release set is not empty")?;
                    self.on_invocation_released(released).await?;
                }
                admitted = self.admit_rx.recv() => {
                    if let Some(admitted) = admitted {
                        self.dispatch_admitted(admitted, running).await?;
                    }
                }
                request = self.start_rx.recv() => {
                    if let Some(request) = request
                        && let Some(invocation) = self.handle_start(request).await?
                    {
                        self.start_invocation(invocation, running).await?;
                    }
                }
            }

            // Finishing a descendant can make a waiting parent ready too.
            while let Some(invocation) = completed.keys().copied().find(|invocation| {
                !self.live.invocations().any(|candidate| {
                    candidate != *invocation && self.is_descendant_or_same(candidate, *invocation)
                })
            }) {
                let done = completed.remove(&invocation).expect("completed invocation");
                self.complete_execution(done, running, releasing).await?;
            }
        }
    }

    /// A driver stopped. Its report is checked and its run notes recorded,
    /// its unfinished descendants are settled, and the gates nothing uses
    /// any more are dropped. The completion itself comes back to the loop,
    /// which applies it once no descendant is live.
    async fn on_execution_done(
        &mut self,
        done: CompletedExecution,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<CompletedExecution, CoordinatorError> {
        self.live.stop(done.invocation, done.execution);
        Self::check_report(done.execution, &done.report)?;
        self.append_run_notes(done.execution, &done.report).await?;
        self.settle_descendants(done.invocation, running).await?;
        self.drop_idle_gates();
        Ok(done)
    }

    /// An invocation's leases are released: every release is recorded, the
    /// invocation is no longer live, and its status says it finished.
    async fn on_invocation_released(
        &mut self,
        released: InvocationReleased,
    ) -> Result<(), CoordinatorError> {
        let InvocationReleased {
            invocation,
            releases,
        } = released;
        for release in releases {
            self.append_scope_released(invocation, release).await?;
        }
        self.live.remove(invocation);
        if let Some(sender) = self.statuses.get(&invocation) {
            let result = self.store.state().invocations[&invocation]
                .result
                .clone()
                .expect("release follows the durable invocation result");
            sender.send_replace(InvocationStatus::Finished(result));
        }
        Ok(())
    }

    /// A terminal replay need not reissue its old calls. Settle all durable
    /// descendants before releasing the parent's resources or restarting it.
    async fn settle_descendants(
        &mut self,
        invocation: InvocationId,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<(), CoordinatorError> {
        let descendants: Vec<_> = self
            .store
            .state()
            .invocations
            .iter()
            .filter_map(|(candidate, state)| {
                (*candidate != invocation
                    && state.result.is_none()
                    && self.is_descendant_or_same(*candidate, invocation))
                .then_some((*candidate, state.cancelled))
            })
            .collect();
        let uncancelled = descendants
            .iter()
            .copied()
            .filter(|(_, cancelled)| !cancelled)
            .collect();
        self.cancel_invocations(uncancelled, InvocationId::ROOT, None, false)
            .await?;
        for (descendant, _) in descendants {
            self.start_invocation(descendant, running).await?;
        }
        Ok(())
    }

    /// Start an unfinished invocation that is not already live. A fork's
    /// child (one declared under a gate) is queued for a slot instead, so the
    /// fork has at most `max_parallel` live children; its declaration is
    /// durable already, and only its driver waits.
    async fn start_invocation(
        &mut self,
        invocation: InvocationId,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<(), CoordinatorError> {
        let state = &self.store.state().invocations[&invocation];
        if state.result.is_some() || self.live.contains(invocation) {
            return Ok(());
        }
        if let Some((key, max_parallel)) = self.fork_admission(invocation) {
            self.live.queue(invocation);
            self.queue_for_slot(invocation, key, max_parallel);
            return Ok(());
        }
        self.dispatch_invocation(invocation, None, running).await
    }

    /// Declare the invocation's first execution when it has none, then run
    /// its driver. `slot` is the fork slot a gated child was admitted on.
    async fn dispatch_invocation(
        &mut self,
        invocation: InvocationId,
        slot: Option<OwnedSemaphorePermit>,
        running: &mut JoinSet<CompletedExecution>,
    ) -> Result<(), CoordinatorError> {
        let state = &self.store.state().invocations[&invocation];
        if state.executions.is_empty() {
            self.declare_first_execution(invocation).await?;
        }
        let execution = *self.store.state().invocations[&invocation]
            .executions
            .last()
            .ok_or(CoordinatorError::MissingExecution(invocation))?;
        if let Some(status) = self.statuses.get(&invocation) {
            status.send_replace(InvocationStatus::Running { execution });
        }
        let declaration = self.store.state().executions[&execution]
            .declaration
            .clone();
        let graph = self
            .store
            .load_graph(
                self.store.state().invocations[&invocation]
                    .declaration
                    .graph,
            )
            .await?;
        let PreparedExecution {
            driver,
            pipeline,
            cancel_before_run,
            slot,
            leases,
        } = self
            .prepare_execution(
                invocation,
                execution,
                &declaration.start,
                declaration.middleware_state,
                &graph,
                slot,
            )
            .await?;
        let handle = driver.handle();
        self.live.run(invocation, execution, handle.clone(), leases);
        running.spawn(async move {
            if cancel_before_run {
                handle.cancel(ir::CancelScopeId::ROOT).await;
            }
            let report = driver.run().await;
            CompletedExecution {
                invocation,
                execution,
                graph,
                report,
                middleware_state: pipeline.checkpoint(),
                slot,
            }
        });
        Ok(())
    }

    async fn complete_execution(
        &mut self,
        done: CompletedExecution,
        running: &mut JoinSet<CompletedExecution>,
        releasing: &mut JoinSet<InvocationReleased>,
    ) -> Result<(), CoordinatorError> {
        let CompletedExecution {
            invocation,
            execution,
            graph,
            report,
            middleware_state,
            slot,
        } = done;
        let exit = report.exit.clone();
        if let Some(recorded) = &self.store.state().executions[&execution].exit {
            if recorded != &exit {
                return Err(CoordinatorError::ConflictingExit { execution });
            }
        } else {
            self.append(CoordinatorEvent::ExecutionFinished {
                execution,
                exit: exit.clone(),
            })
            .await?;
        }
        // The fork slot goes back only now, after the end is recorded, so the
        // next queued child's `ExecutionDeclared` follows this
        // `ExecutionFinished`.
        drop(slot);
        match exit {
            EngineExit::Restart { target, .. } => {
                self.live.remove(invocation);
                if self.store.state().successor_of(execution).is_none() {
                    self.declare_successor(
                        invocation,
                        execution,
                        target,
                        // Only the registered graph survives a restart.
                        // Dynamic NodeIds can be reused by different splices.
                        report
                            .state
                            .prior_firings()
                            .into_iter()
                            .filter(|(node, _)| graph.node(*node).is_some())
                            .collect(),
                        middleware_state,
                    )
                    .await?;
                }
                self.start_invocation(invocation, running).await?;
            }
            EngineExit::Terminal { status } => {
                let result = project_result(execution, status, &graph, &report.state);
                self.refuse_secret(&result)?;
                self.append(CoordinatorEvent::InvocationFinished {
                    invocation,
                    result: result.clone(),
                })
                .await?;
                self.live.mark_releasing(invocation);
                self.release_invocation_leases(invocation, result.status, releasing)
                    .await;
                if invocation == InvocationId::ROOT {
                    self.last_root_report = Some(report);
                }
            }
        }
        Ok(())
    }

    /// Record what the execution's run-level hook points noted, in their
    /// order, before the run's own finish is recorded.
    async fn append_run_notes(
        &mut self,
        execution: ExecutionId,
        report: &driver::ExecutionReport,
    ) -> Result<(), CoordinatorError> {
        for note in &report.run_notes {
            self.append(CoordinatorEvent::RunNoteRecorded {
                execution: Some(execution),
                kind:      note.kind.clone(),
                payload:   note.payload.clone(),
            })
            .await?;
        }
        Ok(())
    }

    fn check_report(
        execution: ExecutionId,
        report: &driver::ExecutionReport,
    ) -> Result<(), CoordinatorError> {
        if let Some(message) = &report.store_failure {
            return Err(CoordinatorError::StoreFailed(message.clone()));
        }
        if let Some(error) = report.observer_errors.first() {
            return Err(CoordinatorError::EventWriter {
                execution,
                message: error.to_string(),
            });
        }
        Ok(())
    }

    async fn declare_first_execution(
        &mut self,
        invocation: InvocationId,
    ) -> Result<ExecutionId, CoordinatorError> {
        let execution = self.store.state().next_execution_id();
        let declaration = &self.store.state().invocations[&invocation].declaration;
        let start = EngineStart {
            entry:           EntryPoint::GraphEntries,
            context:         declaration.context.clone(),
            prior_firings:   BTreeMap::new(),
            execution_index: 0,
            max_executions:  self.options.max_executions,
        };
        self.append(CoordinatorEvent::ExecutionDeclared {
            execution,
            invocation,
            predecessor: None,
            start,
            middleware_state: initial_middleware_state(&self.middleware),
        })
        .await?;
        Ok(execution)
    }

    async fn declare_successor(
        &mut self,
        invocation: InvocationId,
        predecessor: ExecutionId,
        target: ir::NodeId,
        prior_firings: BTreeMap<ir::NodeId, u32>,
        middleware_state: MiddlewareState,
    ) -> Result<ExecutionId, CoordinatorError> {
        let count = self.store.state().invocations[&invocation].executions.len();
        if count >= self.options.max_executions as usize {
            return Err(CoordinatorError::ExecutionLimit { invocation });
        }
        let execution = self.store.state().next_execution_id();
        self.append(CoordinatorEvent::ExecutionDeclared {
            execution,
            invocation,
            predecessor: Some(predecessor),
            start: EngineStart {
                entry: EntryPoint::Node(target),
                context: BTreeMap::new(),
                prior_firings,
                execution_index: u32::try_from(count)
                    .expect("the execution limit is represented by u32"),
                max_executions: self.options.max_executions,
            },
            middleware_state,
        })
        .await?;
        Ok(execution)
    }

    async fn prepare_execution(
        &mut self,
        invocation: InvocationId,
        execution: ExecutionId,
        start: &EngineStart,
        middleware_state: MiddlewareState,
        graph: &Graph,
        slot: Option<OwnedSemaphorePermit>,
    ) -> Result<PreparedExecution, CoordinatorError> {
        let mut cancel_before_run = self.store.state().invocations[&invocation].cancelled;
        let directory = self.execution_dir(execution);
        fs::create_dir_all(&directory).map_err(|source| CoordinatorError::Io {
            action: "create",
            path: directory.clone(),
            source,
        })?;
        let secrets = self.invocation_secrets(invocation);
        let pipeline = Arc::new(
            MiddlewarePipeline::new(
                invocation,
                execution,
                self.middleware.clone(),
                middleware_state,
            )
            .map_err(|error| CoordinatorError::EventWriter {
                execution,
                message: error.to_string(),
            })?
            .with_rolls(self.runtime.decision_rolls(execution)),
        );
        let decoded = read_execution_log(&**self.store.logs(), execution).await?;
        let context = self.hook_context(invocation, execution);
        let (driver, writer, leases) = if decoded.log.is_empty() {
            let writer = Arc::new(ExecutionLogWriter::new(self.writer.clone(), execution, 0));
            let (sandbox, leases) = self
                .prepare_sandbox(invocation, execution, writer.clone())
                .await?;
            let driver = self.runtime.driver(
                (*graph).clone(),
                start.clone(),
                &directory,
                context,
                sandbox,
                secrets,
            );
            (driver, writer, leases)
        } else {
            // Replaying an existing root cancellation already restores it.
            // Sending it again would ask the driver to escalate to a kill.
            cancel_before_run &= !decoded.log.events().any(|event| {
                matches!(
                    event,
                    Event::CancelRequested {
                        target: engine::CancelTarget::Scope(scope)
                    }
                    | Event::KillRequested { scope }
                        if *scope == ir::CancelScopeId::ROOT
                )
            });
            if !pipeline.is_empty() {
                rebuild_middleware(&pipeline, graph, &decoded.log).map_err(|error| {
                    CoordinatorError::EventWriter {
                        execution,
                        message: error.to_string(),
                    }
                })?;
            }
            let high_water = decoded.log.len() as u64;
            let writer = Arc::new(ExecutionLogWriter::new(
                self.writer.clone(),
                execution,
                high_water,
            ));
            let (sandbox, leases) = self
                .prepare_sandbox(invocation, execution, writer.clone())
                .await?;
            let (driver, _) = self.runtime.resume_driver(
                (*graph).clone(),
                decoded.log,
                &directory,
                context,
                sandbox,
                secrets,
            )?;
            (driver.with_engine_start(start.clone()), writer, leases)
        };
        let client = CoordinatorInvocationClient::new(execution, self.start_tx.clone());
        let identity = crate::ExecutionIdentity {
            run: self.runtime.run_key().clone(),
            invocation,
            execution,
        };
        let fold = Arc::new(pipeline.fold_observer());
        let mut driver = driver
            .with_run_owner(invocation == InvocationId::ROOT)
            .observe_run_log(writer.clone())
            .observe(fold)
            .with_decision_resolver(pipeline.clone())
            .with_capability(client)
            .with_capability(identity);
        let mut execution_slot = None;
        if let Some(slots) = self.attempt_slots(invocation) {
            let held = slot.map_or_else(ExecutionSlot::empty, ExecutionSlot::holding);
            driver = driver.with_attempt_slots(slots, held.clone());
            execution_slot = Some(held);
        }
        for observer in &self.observers {
            driver = driver.observe(Arc::new(crate::AddressedObserver::new(
                execution,
                observer.clone(),
            )));
        }
        Ok(PreparedExecution {
            driver,
            pipeline,
            cancel_before_run,
            slot: execution_slot,
            leases,
        })
    }

    /// What the hooks are told about an execution: the run, the invocation,
    /// the execution, and the call that started a nested invocation.
    fn hook_context(&self, invocation: InvocationId, execution: ExecutionId) -> HookContext {
        let context = HookContext::new(self.runtime.run_key().clone(), invocation, execution);
        match &self.store.state().invocations[&invocation].declaration.call {
            Some(call) => context.with_parent(ParentLink::from(call)),
            None => context,
        }
    }

    fn invocation_secrets(&self, invocation: InvocationId) -> Arc<dyn executor::SecretProvider> {
        if invocation == InvocationId::ROOT {
            return self.runtime.secret_provider();
        }
        let declaration = &self.store.state().invocations[&invocation].declaration;
        let call = declaration
            .call
            .as_ref()
            .expect("a non-root invocation has a parent call");
        let parent = self.store.state().executions[&call.parent]
            .declaration
            .invocation;
        Arc::new(InvocationSecrets::new(
            self.invocation_secrets(parent),
            declaration.secret_bindings.clone(),
        ))
    }

    fn is_descendant_or_same(&self, mut invocation: InvocationId, ancestor: InvocationId) -> bool {
        loop {
            if invocation == ancestor {
                return true;
            }
            let Some(call) = self.store.state().invocations[&invocation]
                .declaration
                .call
                .as_ref()
            else {
                return false;
            };
            let Some(parent) = self.store.state().executions.get(&call.parent) else {
                return false;
            };
            invocation = parent.declaration.invocation;
        }
    }

    /// Append a record, unless a write to the run's store has failed: the
    /// lifetime ends at the first failure, and records nothing after it.
    async fn append(
        &mut self,
        event: CoordinatorEvent,
    ) -> Result<CoordinatorRecord, CoordinatorError> {
        self.check_store()?;
        let record = match self.store.append(event).await {
            Ok(record) => record,
            // The backend failed the write: the lifetime ends, as at any
            // failed store write. Any other refusal is the record's own.
            Err(StoreError::Store(error)) => {
                let message = format!("could not append to the coordinator log: {error}");
                self.writer.failure().trip(message.clone());
                return Err(CoordinatorError::StoreFailed(message));
            }
            Err(error) => return Err(error.into()),
        };
        for observer in &self.observers {
            observer.on_lifecycle(&record);
        }
        Ok(record)
    }

    /// A crash can cut a cancel's cascade short: the cancelled invocation is
    /// recorded, some of its descendants are not. Record the rest before
    /// anything runs, so no descendant of a cancelled invocation resumes
    /// uncancelled.
    async fn finish_cut_off_cancels(&mut self) -> Result<(), CoordinatorError> {
        let state = self.store.state();
        let cancelled: Vec<InvocationId> = state
            .invocations
            .iter()
            .filter(|(_, invocation)| invocation.cancelled)
            .map(|(id, _)| *id)
            .collect();
        let cut_off: Vec<InvocationId> = state
            .invocations
            .iter()
            .filter(|(id, invocation)| {
                invocation.result.is_none()
                    && !invocation.cancelled
                    && cancelled
                        .iter()
                        .any(|ancestor| self.is_descendant_or_same(**id, *ancestor))
            })
            .map(|(id, _)| *id)
            .collect();
        for invocation in cut_off {
            self.append(CoordinatorEvent::InvocationCancelRequested {
                invocation,
                reason: None,
            })
            .await?;
        }
        Ok(())
    }

    /// The run's first failed write, as the error that ends the lifetime.
    fn check_store(&self) -> Result<(), CoordinatorError> {
        match self.writer.failure().get() {
            Some(first) => Err(CoordinatorError::StoreFailed(first.to_owned())),
            None => Ok(()),
        }
    }

    fn refuse_secret<T: serde::Serialize>(&self, value: &T) -> Result<(), CoordinatorError> {
        let encoded = serde_json::to_string(value).map_err(CoordinatorError::EncodeDurableData)?;
        self.refuse_secret_bytes(&encoded)
    }

    fn refuse_secret_bytes(&self, encoded: &str) -> Result<(), CoordinatorError> {
        if self.runtime.masker().contains_secret(encoded) {
            return Err(CoordinatorError::SecretInDurableData);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
