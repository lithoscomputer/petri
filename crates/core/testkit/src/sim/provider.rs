//! The world as a sandbox provider, behind the real lease router.
//!
//! [`WorldExecutor`](super::WorldExecutor) stands in for the executor, so a
//! run under it has no lease records to recover. This provider stands behind
//! the router instead: the lease manager creates, attaches, lists, stops,
//! starts and deletes the world's sandboxes as it does a Docker daemon's,
//! records its intents in the run's resource log first, and recovers what a
//! crash left. A sandbox's processes run on the paused clock and outlive a
//! crash, as a container's do; stopping or deleting the sandbox ends them,
//! which is the fence.
//!
//! A dead lifetime's provider does nothing: every call it makes, and every
//! call on a sandbox it handed out, waits forever, as a call from a process
//! that died never returns. The world checks each call that changes a
//! sandbox, and each process start, against the lease's record in the run's
//! store ([`LeaseRecords`]), and can crash the lifetime at a chosen call,
//! before its effect or after it ([`CallCrash`]).

use std::collections::BTreeMap;
use std::fmt;
use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use executor::ExitStatus;
use executor_sandbox::{
    LEASE_LABEL, LeaseState, PendingIntent, ProviderContext, ProviderFactory, ProviderNetwork,
    fingerprint,
};
use sandbox_driver::{
    Capabilities, DirEntry, Error, EventContext, Exec, ExecControls, ExecResult, ExecSpec,
    ExecStreamingResult, FileMetadata, Filesystem, HealthStatus, Isolation, OutputStream,
    PlatformInfo, ProviderError, ProviderHealth, ProviderKind, ResourceKind, Sandbox,
    SandboxFilter, SandboxId, SandboxProvider, SandboxSpec, SandboxState, SandboxStatus,
    Termination,
};
use smol_str::SmolStr;
use tokio::sync::Notify;
use tokio::time;

use super::{Place, World, execution_of, line};

/// The provider kind the world serves as: its sandboxes, like containers,
/// outlive the process that created them.
pub const WORLD_KIND: &str = "docker";

/// Where every sandbox runs its processes.
const WORKING_DIRECTORY: &str = "/work";

/// A lease's record, as a provider call finds it in the run's store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseView {
    pub state:       LeaseState,
    pub pending:     Option<PendingIntent>,
    /// The allocation names the backend it creates on.
    pub fingerprint: bool,
    pub resource_id: Option<String>,
}

/// The run's lease records, as the world reads them: the simulation reads
/// its store, so the world can check that each call follows the intent the
/// lease manager recorded before it.
#[async_trait]
pub trait LeaseRecords: Send + Sync + fmt::Debug {
    /// The latest record of the lease its label names, if one is stored.
    async fn lease(&self, lease: &str) -> Option<LeaseView>;
}

/// A provider call that changes a sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Call {
    Create,
    Stop,
    Start,
    Delete,
}

/// When a crash lands on a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moment {
    /// Before its effect: the call never happened.
    Before,
    /// After its effect, before it returns: the caller never learns.
    After,
}

/// Crash the lifetime at the `nth` call of a kind in it, counting from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallCrash {
    pub call:   Call,
    pub nth:    usize,
    pub moment: Moment,
}

/// One sandbox the provider created, as the world last saw it.
#[derive(Clone, Debug)]
pub struct WorldSandbox {
    pub id:          String,
    pub labels:      BTreeMap<String, String>,
    pub state:       SandboxState,
    /// Counts the sandboxes created under this id.
    pub incarnation: u32,
    /// The lifetime that created it.
    pub lifetime:    u32,
    /// Someone outside the run deleted it.
    pub lost:        bool,
    pub stops:       u32,
    pub starts:      u32,
}

impl WorldSandbox {
    /// The lease its label names.
    pub fn lease(&self) -> Option<&str> {
        self.labels.get(LEASE_LABEL).map(String::as_str)
    }

    /// The key its processes run under.
    fn key(&self) -> SmolStr {
        SmolStr::new(&self.id)
    }
}

/// One call that changed a sandbox.
#[derive(Clone, Debug)]
pub struct CallRecord {
    pub call:       Call,
    pub sandbox:    String,
    pub lifetime:   u32,
    /// A delete of a sandbox its lease's record does not name: the sweep of
    /// a create whose record was never written.
    pub unrecorded: bool,
    /// The call failed, as the world's faults decided.
    pub failed:     bool,
}

impl World {
    /// The factory a run of the current lifetime builds its provider with.
    pub fn factory(self: &Arc<Self>) -> WorldFactory {
        WorldFactory {
            world:    Arc::clone(self),
            lifetime: self.lock().lifetime,
        }
    }

    /// Check every provider call against `records`.
    pub fn check_leases(&self, records: Arc<dyn LeaseRecords>) {
        self.lock().records = Some(records);
    }

    /// Crash the current lifetime at a call, by notifying `crash`.
    pub fn crash_at(&self, at: CallCrash, crash: Arc<Notify>) {
        self.lock().crash_at = Some((at, crash));
    }

    /// Delete, from outside the run, each sandbox a roll picks with `percent`
    /// chance: what a person or a reaper does between two lifetimes. Returns
    /// how many were lost.
    pub fn lose_sandboxes(&self, percent: u64) -> usize {
        let mut state = self.lock();
        let mut lost = 0;
        for index in 0..state.sandboxes.len() {
            if state.sandboxes[index].state == SandboxState::Deleted || !state.dice.chance(percent)
            {
                continue;
            }
            let (key, generation) = {
                let sandbox = &mut state.sandboxes[index];
                sandbox.state = SandboxState::Deleted;
                sandbox.lost = true;
                (sandbox.key(), sandbox.incarnation)
            };
            for process in &state.processes {
                if process.key == key && process.generation == generation {
                    process.state.kill(9);
                }
            }
            lost += 1;
        }
        lost
    }

    pub fn sandboxes(&self) -> Vec<WorldSandbox> {
        self.lock().sandboxes.clone()
    }

    pub fn calls(&self) -> Vec<CallRecord> {
        self.lock().calls.clone()
    }
}

/// Builds the world's provider for one lifetime's run.
pub struct WorldFactory {
    world:    Arc<World>,
    lifetime: u32,
}

#[async_trait]
impl ProviderFactory for WorldFactory {
    fn kind(&self) -> &str {
        WORLD_KIND
    }

    fn fingerprint_seed(&self, _context: &ProviderContext) -> String {
        fingerprint::docker(Some("world"))
    }

    fn network(&self) -> ProviderNetwork {
        ProviderNetwork::docker(None, None)
    }

    async fn connect(
        &self,
        _context: &ProviderContext,
    ) -> sandbox_driver::Result<Arc<dyn SandboxProvider>> {
        Ok(Arc::new(WorldProvider {
            world:        Arc::clone(&self.world),
            lifetime:     self.lifetime,
            kind:         ProviderKind::try_new(WORLD_KIND).expect("a valid provider kind"),
            capabilities: Capabilities::minimal(Isolation::Container),
        }))
    }
}

/// The world's sandboxes, as one lifetime reaches them.
struct WorldProvider {
    world:        Arc<World>,
    lifetime:     u32,
    kind:         ProviderKind,
    capabilities: Capabilities,
}

/// What every handle on a lifetime's side of the provider shares.
#[derive(Clone)]
struct Side {
    world:    Arc<World>,
    lifetime: u32,
    kind:     ProviderKind,
}

impl Side {
    fn dead(&self) -> bool {
        let state = self.world.lock();
        state.lifetime > self.lifetime && !state.zombies.contains(&self.lifetime)
    }

    /// Whether this lifetime's successor took the run. A zombie's call is
    /// counted, the sandbox it changes remembered, and the rules that
    /// assume a live owner skip it.
    fn superseded(&self, sandbox: &str) -> bool {
        let mut state = self.world.lock();
        let superseded = state.lifetime > self.lifetime;
        if superseded {
            state.zombie_calls += 1;
            state.zombie_touched.insert(sandbox.to_owned());
        }
        superseded
    }

    /// A dead lifetime's call never returns.
    async fn alive(&self) {
        if self.dead() {
            pending::<()>().await;
        }
    }

    /// Count a call; the crash plan's moment when this is the one it names.
    fn count(&self, call: Call) -> Option<(Moment, Arc<Notify>)> {
        let mut state = self.world.lock();
        let count = {
            let count = state.counts.entry(call).or_insert(0);
            *count += 1;
            *count
        };
        match &state.crash_at {
            Some((at, crash)) if at.call == call && at.nth == count => {
                Some((at.moment, Arc::clone(crash)))
            }
            _ => None,
        }
    }

    /// The crash lands here: the lifetime is told, and the call never
    /// returns.
    async fn crash(crash: &Notify) {
        crash.notify_one();
        pending::<()>().await;
    }

    /// Wait out a call's delay, from the world's faults; a lifetime that
    /// died meanwhile never hears back.
    async fn delay(&self, most: impl Fn(&super::Faults) -> u64) {
        let delay = {
            let mut state = self.world.lock();
            let most = most(&state.faults);
            state.dice.roll(most + 1)
        };
        time::sleep(Duration::from_millis(delay)).await;
        self.alive().await;
    }

    fn fails(&self, percent: impl Fn(&super::Faults) -> u64) -> bool {
        let mut state = self.world.lock();
        let percent = percent(&state.faults);
        state.dice.chance(percent)
    }

    fn failure(&self, what: &str) -> Error {
        Error::Provider(ProviderError::new(
            self.kind.clone(),
            format!("simulated {what} failure"),
        ))
    }

    fn record(&self, call: Call, sandbox: &str, unrecorded: bool, failed: bool) {
        self.world.lock().calls.push(CallRecord {
            call,
            sandbox: sandbox.to_owned(),
            lifetime: self.lifetime,
            unrecorded,
            failed,
        });
    }

    /// The lease's record, when the simulation lets the world read them.
    async fn view(&self, lease: Option<&str>) -> Option<Option<LeaseView>> {
        let records = self.world.lock().records.clone()?;
        let lease = lease?;
        Some(records.lease(lease).await)
    }
}

/// The world's sandbox with `id` that is not deleted.
fn live_index(sandboxes: &[WorldSandbox], id: &str) -> Option<usize> {
    sandboxes
        .iter()
        .rposition(|sandbox| sandbox.id == id && sandbox.state != SandboxState::Deleted)
}

impl WorldProvider {
    fn side(&self) -> Side {
        Side {
            world:    Arc::clone(&self.world),
            lifetime: self.lifetime,
            kind:     self.kind.clone(),
        }
    }

    fn handle(&self, index: usize) -> Arc<dyn Sandbox> {
        let (id, incarnation) = {
            let state = self.world.lock();
            let sandbox = &state.sandboxes[index];
            (sandbox.id.clone(), sandbox.incarnation)
        };
        Arc::new(WorldSandboxHandle {
            side: self.side(),
            id: SandboxId::try_new(id.as_str()).expect("the world's sandbox ids are valid"),
            incarnation,
            capabilities: self.capabilities.clone(),
            exec: WorldExec {
                side: self.side(),
                id,
                incarnation,
            },
        })
    }
}

#[async_trait]
impl SandboxProvider for WorldProvider {
    fn kind(&self) -> &ProviderKind {
        &self.kind
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// A sandbox named as the spec asks, carrying its labels. It exists from
    /// the call's start, whether or not the call returns.
    async fn create(
        &self,
        spec: &SandboxSpec,
        _events: Option<EventContext>,
    ) -> sandbox_driver::Result<Arc<dyn Sandbox>> {
        let side = self.side();
        side.alive().await;
        let id = spec
            .name
            .clone()
            .ok_or_else(|| Error::invalid_spec("name", "the world names every sandbox"))?;
        let superseded = side.superseded(&id);
        if let Some(Some(view)) = side
            .view(spec.labels.get(LEASE_LABEL).map(String::as_str))
            .await
            && !superseded
            && (view.state != LeaseState::Allocating || !view.fingerprint || view.pending.is_some())
        {
            self.world.violation(format!(
                "created {id} before its lease recorded an allocation with a fingerprint \
                 ({view:?})"
            ));
        }
        let crash = side.count(Call::Create);
        if let Some((Moment::Before, crash)) = &crash {
            Side::crash(crash).await;
        }
        let failed = side.fails(|faults| faults.acquire_failure);
        let index = {
            let mut state = self.world.lock();
            if live_index(&state.sandboxes, &id).is_some() {
                return Err(Error::Provider(ProviderError::new(
                    self.kind.clone(),
                    format!("a sandbox named {id} already exists"),
                )));
            }
            let incarnation = u32::try_from(
                state
                    .sandboxes
                    .iter()
                    .filter(|sandbox| sandbox.id == id)
                    .count(),
            )
            .unwrap_or(u32::MAX)
                + 1;
            state.sandboxes.push(WorldSandbox {
                id: id.clone(),
                labels: spec.labels.clone(),
                state: if failed {
                    SandboxState::Deleted
                } else {
                    SandboxState::Running
                },
                incarnation,
                lifetime: self.lifetime,
                lost: false,
                stops: 0,
                starts: 0,
            });
            state.sandboxes.len() - 1
        };
        side.record(Call::Create, &id, false, failed);
        if let Some((Moment::After, crash)) = &crash {
            Side::crash(crash).await;
        }
        side.delay(|faults| faults.acquire_ms).await;
        if failed {
            return Err(side.failure("create"));
        }
        Ok(self.handle(index))
    }

    async fn attach(
        &self,
        id: &SandboxId,
        _events: Option<EventContext>,
    ) -> sandbox_driver::Result<Arc<dyn Sandbox>> {
        self.side().alive().await;
        let index = live_index(&self.world.lock().sandboxes, id.as_str());
        index
            .map(|index| self.handle(index))
            .ok_or_else(|| Error::NotFound {
                resource: ResourceKind::Sandbox,
                id:       id.as_str().to_owned(),
            })
    }

    async fn list(&self, filter: &SandboxFilter) -> sandbox_driver::Result<Vec<SandboxStatus>> {
        self.side().alive().await;
        let state = self.world.lock();
        Ok(state
            .sandboxes
            .iter()
            .filter(|sandbox| sandbox.state != SandboxState::Deleted)
            .filter(|sandbox| {
                filter
                    .labels
                    .iter()
                    .all(|(key, value)| sandbox.labels.get(key) == Some(value))
            })
            .map(status)
            .collect())
    }

    async fn health(&self) -> sandbox_driver::Result<ProviderHealth> {
        Ok(ProviderHealth::new(HealthStatus::Ok))
    }
}

fn status(sandbox: &WorldSandbox) -> SandboxStatus {
    let mut status = SandboxStatus::new(
        SandboxId::try_new(sandbox.id.as_str()).expect("the world's sandbox ids are valid"),
        sandbox.state,
    );
    status.name = Some(sandbox.id.clone());
    status.labels = sandbox.labels.clone();
    status
}

/// A sandbox, as the lifetime that attached or created it holds it.
struct WorldSandboxHandle {
    side:         Side,
    id:           SandboxId,
    incarnation:  u32,
    capabilities: Capabilities,
    exec:         WorldExec,
}

impl WorldSandboxHandle {
    /// The sandbox this handle names, if it still exists.
    fn index(&self) -> Option<usize> {
        let state = self.side.world.lock();
        state.sandboxes.iter().position(|sandbox| {
            sandbox.id == self.id.as_str()
                && sandbox.incarnation == self.incarnation
                && sandbox.state != SandboxState::Deleted
        })
    }

    fn gone(&self) -> Error {
        Error::NotFound {
            resource: ResourceKind::Sandbox,
            id:       self.id.as_str().to_owned(),
        }
    }

    /// End every process running in this sandbox, whichever lifetime ran it.
    fn end_processes(&self) {
        let mut state = self.side.world.lock();
        let lifetime = state.lifetime;
        let now = time::Instant::now();
        let mut fenced = 0;
        for process in &state.processes {
            if process.key == self.id.as_str()
                && process.generation == self.incarnation
                && process.running(now)
            {
                if process.lifetime < lifetime {
                    fenced += 1;
                }
                process.state.kill(9);
            }
        }
        state.fenced += fenced;
    }

    /// Stop or delete: check the lease's intent, crash where planned, apply
    /// `effect`, and note the call.
    async fn change(
        &self,
        call: Call,
        effect: impl FnOnce(&mut WorldSandbox),
    ) -> sandbox_driver::Result<()> {
        self.side.alive().await;
        let Some(index) = self.index() else {
            return Err(self.gone());
        };
        let superseded = self.side.superseded(self.id.as_str());
        let lease = self.side.world.lock().sandboxes[index]
            .lease()
            .map(ToOwned::to_owned);
        let mut unrecorded = false;
        if let Some(view) = self.side.view(lease.as_deref()).await {
            let names = view
                .as_ref()
                .and_then(|view| view.resource_id.as_deref())
                .is_some_and(|resource| resource == self.id.as_str());
            let pending = view.as_ref().and_then(|view| view.pending);
            let allowed = match call {
                Call::Stop => pending == Some(PendingIntent::Stop),
                Call::Delete => {
                    unrecorded = !names;
                    pending == Some(PendingIntent::Delete) || !names
                }
                Call::Start | Call::Create => true,
            };
            if !allowed && !superseded {
                self.side.world.violation(format!(
                    "{call:?} of {} before its lease recorded the intent ({view:?})",
                    self.id.as_str()
                ));
            }
        }
        let crash = self.side.count(call);
        if let Some((Moment::Before, crash)) = &crash {
            Side::crash(crash).await;
        }
        let failed = matches!(call, Call::Stop | Call::Delete)
            && self.side.fails(|faults| faults.release_failure);
        if !failed {
            if matches!(call, Call::Stop | Call::Delete) {
                self.end_processes();
            }
            let mut state = self.side.world.lock();
            effect(&mut state.sandboxes[index]);
        }
        self.side.record(call, self.id.as_str(), unrecorded, failed);
        if let Some((Moment::After, crash)) = &crash {
            Side::crash(crash).await;
        }
        self.side.delay(|faults| faults.release_ms).await;
        if failed {
            return Err(self.side.failure(&format!("{call:?}").to_lowercase()));
        }
        Ok(())
    }
}

#[async_trait]
impl Sandbox for WorldSandboxHandle {
    fn id(&self) -> &SandboxId {
        &self.id
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn describe(&self) -> sandbox_driver::Result<SandboxStatus> {
        self.side.alive().await;
        let state = self.side.world.lock();
        state
            .sandboxes
            .iter()
            .rev()
            .find(|sandbox| {
                sandbox.id == self.id.as_str() && sandbox.incarnation == self.incarnation
            })
            .map(status)
            .ok_or_else(|| self.gone())
    }

    fn working_directory(&self) -> &str {
        WORKING_DIRECTORY
    }

    /// Empty: a simulated process reads its parameters from its own
    /// environment only.
    async fn environment(&self) -> sandbox_driver::Result<BTreeMap<String, String>> {
        self.side.alive().await;
        Ok(BTreeMap::new())
    }

    async fn platform_info(&self) -> sandbox_driver::Result<PlatformInfo> {
        Err(Error::Provider(ProviderError::new(
            self.side.kind.clone(),
            "a world sandbox reports no platform",
        )))
    }

    async fn start(&self) -> sandbox_driver::Result<()> {
        self.change(Call::Start, |sandbox| {
            sandbox.state = SandboxState::Running;
            sandbox.starts += 1;
        })
        .await
    }

    async fn stop(&self) -> sandbox_driver::Result<()> {
        self.change(Call::Stop, |sandbox| {
            sandbox.state = SandboxState::Stopped;
            sandbox.stops += 1;
        })
        .await
    }

    async fn delete(&self) -> sandbox_driver::Result<()> {
        self.change(Call::Delete, |sandbox| {
            sandbox.state = SandboxState::Deleted;
        })
        .await
    }

    fn exec(&self) -> &dyn Exec {
        &self.exec
    }

    fn fs(&self) -> &dyn Filesystem {
        &WorldFilesystem
    }
}

/// Runs the world's processes in one sandbox.
struct WorldExec {
    side:        Side,
    id:          String,
    incarnation: u32,
}

impl WorldExec {
    /// The sandbox runs: not stopped, not deleted.
    fn running(&self) -> bool {
        self.side.world.lock().sandboxes.iter().any(|sandbox| {
            sandbox.id == self.id
                && sandbox.incarnation == self.incarnation
                && sandbox.state == SandboxState::Running
        })
    }

    fn lease(&self) -> Option<String> {
        self.side
            .world
            .lock()
            .sandboxes
            .iter()
            .find(|sandbox| sandbox.id == self.id && sandbox.incarnation == self.incarnation)
            .and_then(WorldSandbox::lease)
            .map(ToOwned::to_owned)
    }
}

#[async_trait]
impl Exec for WorldExec {
    /// A buffered command (the executor's `mkdir -p` of a step's working
    /// directory) succeeds and does nothing.
    async fn run(&self, _spec: &ExecSpec) -> sandbox_driver::Result<ExecResult> {
        self.side.alive().await;
        Ok(ExecResult::new(
            Termination::Exited,
            Some(0),
            Duration::ZERO,
        ))
    }

    /// One simulated process, from the step's `SIM_*` parameters: its lines
    /// go to the sink, and it runs for its work time unless the caller's
    /// `term` or `kill`, or the sandbox's stop, ends it first.
    async fn run_streaming(
        &self,
        spec: &ExecSpec,
        controls: ExecControls,
    ) -> sandbox_driver::Result<ExecStreamingResult> {
        self.side.alive().await;
        // A zombie's start, or a start in a sandbox a zombie changed, is
        // the zombie's doing: counted, not a violation.
        let excused = {
            let state = self.side.world.lock();
            state.lifetime > self.side.lifetime || state.zombie_touched.contains(&self.id)
        };
        if !self.running() {
            if !excused {
                self.side.world.violation(format!(
                    "a process started in {}, which is not running",
                    self.id
                ));
            }
            return Err(Error::Provider(ProviderError::new(
                self.side.kind.clone(),
                format!("{} is not running", self.id),
            )));
        }
        if let Some(Some(view)) = self.side.view(self.lease().as_deref()).await
            && !excused
            && (view.state != LeaseState::Live
                || view.resource_id.as_deref() != Some(self.id.as_str()))
        {
            self.side.world.violation(format!(
                "a process started in {} while its lease's record reads {view:?}",
                self.id
            ));
        }
        let execution = execution_of(spec.env.get("SIM_EXECUTION").map_or("", String::as_str));
        let started = self.side.world.start_process(
            Place {
                key: SmolStr::new(&self.id),
                generation: self.incarnation,
                execution,
                current: true,
                lifetime: Some(self.side.lifetime),
            },
            |key| spec.env.get(key).cloned(),
        );
        if let Some(sink) = &controls.sink {
            for index in 0..started.lines {
                let text = format!("{}\n", line(started.firing, started.attempt, index));
                sink(OutputStream::Stdout, text.into_bytes()).await?;
            }
        }
        let process = started.state;
        let mut term_sent = false;
        let mut ended_by = Termination::Exited;
        let exit = loop {
            let changed = process.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(exit) = process.exit() {
                break exit;
            }
            let term = controls.term.clone();
            let kill = controls.kill.clone();
            // In a fixed order: a process that reaches its end as a signal
            // lands has ended.
            tokio::select! {
                biased;
                () = time::sleep_until(process.ends_at) => {
                    process.end(ExitStatus::code(process.code));
                }
                () = &mut changed => {}
                () = async move {
                    match kill {
                        Some(kill) => kill.cancelled().await,
                        None => pending().await,
                    }
                } => {
                    ended_by = Termination::Killed;
                    process.kill(9);
                }
                () = async move {
                    match term {
                        Some(term) => term.cancelled().await,
                        None => pending().await,
                    }
                }, if !term_sent => {
                    term_sent = true;
                    if process.honor_term {
                        ended_by = Termination::Cancelled;
                        process.kill(15);
                    }
                }
            }
        };
        let mut result = ExecResult::new(
            if exit.signal.is_some() {
                ended_by
            } else {
                Termination::Exited
            },
            exit.code,
            Duration::ZERO,
        );
        result.signal = exit.signal;
        Ok(ExecStreamingResult::new(result))
    }
}

/// The filesystem facet: nothing is kept, and every write succeeds.
struct WorldFilesystem;

fn not_found(path: &str) -> Error {
    Error::NotFound {
        resource: ResourceKind::File,
        id:       path.to_owned(),
    }
}

#[async_trait]
impl Filesystem for WorldFilesystem {
    async fn read(&self, path: &str) -> sandbox_driver::Result<Vec<u8>> {
        Err(not_found(path))
    }

    async fn write(&self, _path: &str, _content: &[u8]) -> sandbox_driver::Result<()> {
        Ok(())
    }

    async fn delete(&self, _path: &str, _recursive: bool) -> sandbox_driver::Result<()> {
        Ok(())
    }

    async fn exists(&self, _path: &str) -> sandbox_driver::Result<bool> {
        Ok(false)
    }

    async fn metadata(&self, path: &str) -> sandbox_driver::Result<FileMetadata> {
        Err(not_found(path))
    }

    async fn list_dir(&self, _path: &str, _depth: usize) -> sandbox_driver::Result<Vec<DirEntry>> {
        Ok(Vec::new())
    }

    async fn create_dir(&self, _path: &str) -> sandbox_driver::Result<()> {
        Ok(())
    }

    async fn rename(&self, from: &str, _to: &str) -> sandbox_driver::Result<()> {
        Err(not_found(from))
    }
}
