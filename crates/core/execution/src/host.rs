//! The standalone host: run and resume a durable run through the
//! coordinator.
//!
//! When petri runs a workflow with no product store behind it, the run
//! directory is the record, in the run-directory store's layout
//! (`store::RunDirStore`): `run.json` (identity and lease),
//! `coordinator.jsonl` (the lifecycle log), `graphs/<digest>.json` (every
//! registered graph, byte-exact), `resources.jsonl` (the sandbox leases) and
//! one engine log per execution at `executions/<execution>/events.jsonl`,
//! streamed as it happens. A host with a store of its own installs it with
//! `Runtime::store`, and the same coordinator writes there instead.
//! Everything else under the run dir — workspaces, logs, the executors' own
//! records — identifies the processes and containers of *this* run and
//! fences them on resume.
//!
//! Graphs persist byte-exact and are never masked; the contract that makes
//! that safe (§11) is that raw secret values never belong in a recorded
//! graph, and the coordinator refuses to persist durable data the masker
//! already recognizes. A lazily resolving provider can defeat the check; the
//! contract, not the check, is the rule.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use driver::ExecutionReport;
use executor::SecretProvider;
use ir::Graph;
use runtime::{RunAccess, Runtime};
/// Each execution's engine-log file name under its execution directory, in
/// the run-directory store.
pub use store::EVENTS_FILE;
use store::RunLogs;

use crate::breaker::CircuitBreaker;
use crate::events::{ExportError, verify_export};
/// Forking a stored run at a position: the design is in `FORK.md`.
pub use crate::fork::{ForkError, ForkOptions, ForkOrigin, ForkPosition, ForkedRun, fork_from};
use crate::store::{decode_graph, graph_bytes, read_coordinator_log};
use crate::{
    Coordinator, CoordinatorError, CoordinatorHandle, CoordinatorOptions, CoordinatorState,
    ExecutionObserver, GraphDigest, InvocationId, Middleware,
};

/// What kept the standalone host from running or resuming.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("could not {action} `{path}`: {source}")]
    Io {
        action: &'static str,
        path:   PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("`{path}`: {source}")]
    Events {
        path:   PathBuf,
        #[source]
        source: EventsDecodeError,
    },
    #[error(
        "a value the masker recognizes appears in the serialized graph; refusing to \
         persist it and start the run — raw secret values never belong in `Graph.params` \
         or a step config (§11)"
    )]
    SecretInGraph,
    #[error(transparent)]
    Replay(#[from] engine::ReplayMismatch),
    /// The run's public stream does not export its logs: the determinism
    /// canary's second half, checked with `verify_replay`.
    #[error(transparent)]
    Export(#[from] ExportError),
    #[error(transparent)]
    Coordinator(#[from] CoordinatorError),
    /// The run could not be opened in its store.
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error("the coordinator finished the root invocation without a final execution report")]
    MissingExecutionReport,
    /// The run's creation was cut short: it is stored, but its root
    /// invocation was never declared, so there is nothing to resume. Start
    /// it again under the same key: the start finishes the creation.
    #[error(
        "the run was stored but never started: its root invocation was never declared; start \
         it again"
    )]
    NotStarted,
    /// A fork position the source run cannot be forked at.
    #[error(transparent)]
    Fork(#[from] ForkError),
}

/// Why `events.jsonl` bytes could not become an [`engine::EventLog`]: this
/// crate owns the framing, and the host reads and writes the same format.
pub type EventsDecodeError = crate::EngineLogDecodeError;

/// One successful `events.jsonl` decode.
pub type DecodedEvents = crate::DecodedEngineFile;

/// Decode `events.jsonl` bytes: one record per line, strict torn-line rule.
pub fn decode_events(bytes: &[u8]) -> Result<DecodedEvents, EventsDecodeError> {
    crate::decode_engine_log(bytes)
}

/// Render a log in the `events.jsonl` framing, produced in one piece, with
/// each record's recording time.
pub fn encode_events(log: &engine::EventLog, recorded_at: &[u64]) -> Vec<u8> {
    crate::encode_engine_log(log, recorded_at)
}

/// Read and decode an execution's `events.jsonl`.
pub fn read_events(path: &Path) -> Result<DecodedEvents, HostError> {
    let bytes = fs::read(path).map_err(|e| HostError::Io {
        action: "read",
        path:   path.to_path_buf(),
        source: e,
    })?;
    decode_events(&bytes).map_err(|e| HostError::Events {
        path:   path.to_path_buf(),
        source: e,
    })
}

/// Everything a host hands the coordinator for one fresh run: the root graph,
/// the pre-lowered child graphs a nested-workflow step may invoke (every one
/// is registered before the root starts, so an invoke by digest always
/// resolves), and the observers that see every execution's records.
pub struct HostRun {
    pub graph:      Graph,
    pub children:   Vec<Graph>,
    pub observers:  Vec<Arc<dyn ExecutionObserver>>,
    /// Decision middleware the host adds after the graph's own policy chain
    /// ([`policy_middleware`]): a pause gate, a product's routing hooks. A
    /// resume must install the same list.
    pub middleware: Vec<Arc<dyn Middleware>>,
}

impl From<Graph> for HostRun {
    fn from(graph: Graph) -> Self {
        Self::new(graph)
    }
}

impl HostRun {
    /// A root graph alone: no children, no observers.
    pub fn new(graph: Graph) -> Self {
        Self {
            graph,
            children: Vec::new(),
            observers: Vec::new(),
            middleware: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_children(mut self, children: Vec<Graph>) -> Self {
        self.children = children;
        self
    }

    #[must_use]
    pub fn observe(mut self, observer: Arc<dyn ExecutionObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    #[must_use]
    pub fn with_middleware(mut self, middleware: Arc<dyn Middleware>) -> Self {
        self.middleware.push(middleware);
        self
    }
}

/// The decision middleware a root graph's [`ir::RunPolicy`] asks for: the
/// failure circuit breaker when `loop_restart_signature_limit` is set. The
/// standalone host installs it ahead of the host's own middleware on run and
/// on resume, so the recorded chain matches.
pub fn policy_middleware(graph: &Graph) -> Vec<Arc<dyn Middleware>> {
    let mut chain: Vec<Arc<dyn Middleware>> = Vec::new();
    if let Some(limit) = graph.policy.loop_restart_signature_limit {
        chain.push(Arc::new(CircuitBreaker::reference_for_graph(limit, graph)));
    }
    chain
}

/// Run a graph in the runtime's store, to completion. Every log's `finish`
/// is awaited inside the run, so the run's record is complete when this
/// returns. With the runtime's `verify_replay` on (the default), the final
/// execution's log is replayed afterwards and any divergence is the error.
pub async fn run(rt: &Runtime, graph: Graph) -> Result<ExecutionReport, HostError> {
    run_with_handle(rt, graph, |_| {}).await
}

/// [`run`], handing the coordinator's handle to `with_handle` before the run
/// starts — the hook for cancellation wiring (Ctrl-C, a deadline). The first
/// [`CoordinatorHandle::cancel_root`] cancels politely; a second reaches the
/// drivers' kill tier.
pub async fn run_with_handle(
    rt: &Runtime,
    graph: Graph,
    with_handle: impl FnOnce(CoordinatorHandle),
) -> Result<ExecutionReport, HostError> {
    run_configured(rt, HostRun::new(graph), |handle, _| with_handle(handle)).await
}

/// The general form of [`run`]: children and observers from `run`, and the
/// handle plus the run's secret provider handed to `with_handle` before the
/// run starts — the provider is how an answerer registers a dynamic secret
/// (`answer:<id>`) before delivering its reference into a live firing.
///
/// A run whose creation was cut short under the same key
/// ([`HostError::NotStarted`] on resume) is started again: its stored prefix is
/// taken over and the creation finished. A run that declared an invocation
/// already started, and is refused as existing.
pub async fn run_configured(
    rt: &Runtime,
    run: HostRun,
    with_handle: impl FnOnce(CoordinatorHandle, Arc<dyn SecretProvider>),
) -> Result<ExecutionReport, HostError> {
    let run_dir = rt.run_options().run_dir.clone();
    let run_runtime = rt.prepare_run(&run_dir);
    let mut secrets = run_runtime.secret_provider();
    let mut chain = policy_middleware(&run.graph);
    chain.extend(run.middleware);
    let options = coordinator_options(&run.graph)?;
    // The coordinator is a large value held across every await below; one
    // allocation keeps a host's own future small.
    let mut coordinator =
        match Box::pin(Coordinator::create(run_runtime, chain.clone(), options)).await {
            Err(exists @ CoordinatorError::Open(store::StoreError::Exists { .. })) => {
                let (coordinator, provider) =
                    Box::pin(start_again(rt, &run_dir, chain, options, exists)).await?;
                secrets = provider;
                coordinator
            }
            created => created?,
        };
    for observer in run.observers {
        coordinator = coordinator.observe(observer);
    }
    let digest = register(&mut coordinator, &run.graph).await?;
    for child in &run.children {
        register(&mut coordinator, child).await?;
    }
    with_handle(coordinator.handle(), secrets);
    Box::pin(finish_root(rt, coordinator, digest, run.graph)).await
}

/// Take over a run whose creation was cut short: its stored log holds
/// `run.started` and graphs, and no invocation. The stored state is read
/// without the lease first, so a run that started is refused with
/// `exists` and never touched.
async fn start_again(
    rt: &Runtime,
    run_dir: &Path,
    chain: Vec<Arc<dyn Middleware>>,
    options: CoordinatorOptions,
    exists: CoordinatorError,
) -> Result<(Coordinator, Arc<dyn SecretProvider>), HostError> {
    let run_runtime = rt.prepare_run(run_dir);
    let started = {
        let logs = run_runtime.open(RunAccess::Read).await?;
        !stored_state(&*logs).await?.invocations.is_empty()
    };
    if started {
        run_runtime.finish().await;
        return Err(exists.into());
    }
    let secrets = run_runtime.secret_provider();
    let coordinator = Box::pin(Coordinator::resume(run_runtime, chain, options)).await?;
    Ok((coordinator, secrets))
}

/// The coordinator options a root graph's run policy asks for: its
/// invocation limit when it declares one, else the default. A limit above
/// the hard ceiling or a disabled limit is refused before the run starts.
pub fn coordinator_options(graph: &Graph) -> Result<CoordinatorOptions, HostError> {
    let options = CoordinatorOptions::default();
    match graph.policy.max_invocations {
        Some(limit) => Ok(options
            .with_max_invocations(limit.get())
            .map_err(CoordinatorError::from)?),
        None => Ok(options),
    }
}

async fn register(coordinator: &mut Coordinator, graph: &Graph) -> Result<GraphDigest, HostError> {
    match coordinator.register_graph(graph).await {
        Err(CoordinatorError::SecretInDurableData) => Err(HostError::SecretInGraph),
        result => Ok(result?),
    }
}

/// Continue the run in the runtime's store, to completion — the crash side
/// of [`run`]. Same guarantees, same replay verification. A record that
/// decodes wrongly refuses the resume outright; a torn tail in a run
/// directory is the store's own business and is dropped before the run is
/// read.
///
/// Dynamic secrets (`answer:<id>`) are not in any log by design: re-register
/// them on the provider before delivering again, or the resumed step fails
/// with `secret_unavailable`.
pub async fn resume(rt: &Runtime) -> Result<ExecutionReport, HostError> {
    resume_configured(rt, Vec::new(), Vec::new(), |_, _| {}).await
}

/// [`resume`] with the host's own middleware (the same list the run was
/// started with, after the graph's policy chain, which is rebuilt from the
/// stored root graph before the coordinator checks the recorded chain), its
/// observers attached before the first record (a resumed execution replays
/// its regenerated suffix to them before it dispatches pending work, and the
/// coordinator's own records reach them as they are appended), and the
/// handle hook.
pub async fn resume_configured(
    rt: &Runtime,
    middleware: Vec<Arc<dyn Middleware>>,
    observers: Vec<Arc<dyn ExecutionObserver>>,
    with_handle: impl FnOnce(CoordinatorHandle, Arc<dyn SecretProvider>),
) -> Result<ExecutionReport, HostError> {
    let run_dir = rt.run_options().run_dir.clone();
    let run_runtime = rt.prepare_run(&run_dir);
    let root_graph = {
        let logs = run_runtime.open(RunAccess::Read).await?;
        stored_root_graph(&*logs).await?
    };
    let Some(root_graph) = root_graph else {
        run_runtime.finish().await;
        return Err(HostError::NotStarted);
    };
    let mut chain = policy_middleware(&root_graph);
    chain.extend(middleware);
    let options = coordinator_options(&root_graph)?;
    let secrets = run_runtime.secret_provider();
    let mut coordinator = Box::pin(Coordinator::resume(run_runtime, chain, options)).await?;
    for observer in observers {
        coordinator = coordinator.observe(observer);
    }
    let digest = coordinator.store().state().invocations[&InvocationId::ROOT]
        .declaration
        .graph;
    let graph = (*coordinator.load_graph(digest).await?).clone();
    with_handle(coordinator.handle(), secrets);
    Box::pin(finish_root(rt, coordinator, digest, graph)).await
}

/// The run's replayed coordinator state, read through a handle that holds
/// no lease: what a host checks before it resumes (has the run finished, is
/// it paused) and what a resume needs before the coordinator exists. A
/// record that does not decode is the error.
pub async fn stored_state(logs: &dyn RunLogs) -> Result<CoordinatorState, HostError> {
    let records = read_coordinator_log(logs)
        .await
        .map_err(CoordinatorError::from)?;
    CoordinatorState::replay(&records)
        .map_err(|error| CoordinatorError::from(crate::StoreError::State(error)).into())
}

/// The root invocation's registered graph, read without taking the run
/// lease. `None` when the log has no root invocation yet, or no record at
/// all: a crash can cut a run's creation short before `run.started`.
pub async fn stored_root_graph(logs: &dyn RunLogs) -> Result<Option<Graph>, HostError> {
    if logs
        .read(&store::LogId::Coordinator)
        .await
        .map_err(CoordinatorError::from)?
        .is_empty()
    {
        return Ok(None);
    }
    let state = stored_state(logs).await?;
    let Some(root) = state.invocations.get(&InvocationId::ROOT) else {
        return Ok(None);
    };
    let digest = root.declaration.graph;
    let bytes = graph_bytes(logs, digest)
        .await
        .map_err(CoordinatorError::from)?;
    Ok(Some(
        decode_graph(digest, &bytes).map_err(CoordinatorError::from)?,
    ))
}

/// The shared tail of [`run`] and [`resume`]: run the root invocation to its
/// result, verify replay when the runtime asks for it, and tear the run
/// services down.
async fn finish_root(
    rt: &Runtime,
    mut coordinator: Coordinator,
    digest: GraphDigest,
    graph: Graph,
) -> Result<ExecutionReport, HostError> {
    coordinator.run_root(digest, BTreeMap::default()).await?;
    let report = coordinator
        .take_root_report()
        .ok_or(HostError::MissingExecutionReport)?;
    if rt.run_options().verify_replay {
        engine::verify_replay(graph, &report.state.log)?;
    }
    let logs = coordinator.finish().await;
    if rt.run_options().verify_replay {
        // The other half of the canary: the records the public stream
        // carries are the stored logs, and they replay. Read after
        // `finish`, once every log is closed.
        verify_export(&*logs).await?;
    }
    Ok(report)
}
