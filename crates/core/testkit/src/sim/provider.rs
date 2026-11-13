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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::pending;
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use executor::ExitStatus;
use executor_sandbox::{LEASE_LABEL, LeaseState, PendingIntent, SandboxCatalog};
use sandbox_driver::contract::{
    AccessScope, Activation, ByteStream, CapabilitySet, ContainerField, Descriptor, Durability,
    Environment, EnvironmentName, Error, ExecControls, ExecOutcome, ExecSpec, Expose, Exposure,
    FileEntry, FileRange, Files, Health, LabelFilter, Locality, MutationOutcome, Namespace,
    NamespaceId, NamespaceScope, NetworkMode, OutputStream, PluginLocation, PortSupport, Ports,
    ProviderEntry, ProviderFactory, ProviderName, ProviderRuntime, ProviderSchemas, Registry,
    Resources, Sandbox, SandboxRef, SandboxSpec, SandboxState, SandboxSummary, SecretResolver,
    Source, SourceKind, StaticEnvironments, Status, Termination, WorkloadAdmission,
};
use sandbox_driver::{Isolation, ProviderKind, SandboxId};
use smol_str::SmolStr;
use tokio::sync::Notify;
use tokio::time;

use super::{Place, World, execution_of, line};

/// The provider kind, entry and environment the world serves as: its
/// sandboxes, like containers, outlive the process that created them.
pub const WORLD_KIND: &str = "docker";

/// The namespace every lifetime of the world reaches.
const WORLD_NAMESPACE: &str = "world";

/// Where every sandbox runs its processes.
const WORKING_DIRECTORY: &str = "/work";

/// A lease's record, as a provider call finds it in the run's store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseView {
    pub state:       LeaseState,
    pub pending:     Option<PendingIntent>,
    /// The allocation names the entry, namespace and spec it creates with.
    pub admitted:    bool,
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

    /// The catalog a run of the current lifetime places its scopes on: one
    /// `docker` entry and environment over [`World::factory`].
    pub fn catalog(self: &Arc<Self>) -> SandboxCatalog {
        struct NoSecrets;
        #[async_trait]
        impl SecretResolver for NoSecrets {
            async fn resolve(&self, _: &str) -> Result<Option<String>, Error> {
                Ok(None)
            }
        }
        let name = ProviderName::try_new(WORLD_KIND).expect("a constant entry name");
        let entry: ProviderEntry =
            serde_json::from_value(serde_json::json!({ "plugin": WORLD_KIND }))
                .expect("a constant entry");
        let registry = Registry::new()
            .builtin(
                ProviderKind::try_new(WORLD_KIND).expect("a constant kind"),
                Arc::new(self.factory()),
            )
            .expect("one registration");
        SandboxCatalog::new(
            [(name.clone(), entry)],
            Arc::new(StaticEnvironments(BTreeMap::from([(
                EnvironmentName::try_new(WORLD_KIND).expect("a constant environment name"),
                Environment {
                    provider: name,
                    spec:     SandboxSpec::new(Source::Image("world:runner".to_owned())),
                },
            )]))),
            registry,
            Arc::new(NoSecrets),
        )
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

fn schemas() -> ProviderSchemas {
    ProviderSchemas {
        kind:          ProviderKind::try_new(WORLD_KIND).expect("a valid provider kind"),
        version:       "1".to_owned(),
        config_schema: serde_json::from_value(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
        }))
        .expect("a constant schema"),
        spec_schema:   None,
        secrets:       Vec::new(),
    }
}

#[async_trait]
impl ProviderFactory for WorldFactory {
    async fn discover(&self, _location: &PluginLocation) -> Result<ProviderSchemas, Error> {
        Ok(schemas())
    }

    async fn activate(&self, activation: Activation) -> Result<Arc<dyn ProviderRuntime>, Error> {
        Ok(Arc::new(WorldProvider {
            world:    Arc::clone(&self.world),
            lifetime: self.lifetime,
            entry:    activation.name,
        }))
    }
}

/// The world's sandboxes, as one lifetime reaches them.
struct WorldProvider {
    world:    Arc<World>,
    lifetime: u32,
    entry:    ProviderName,
}

/// What every handle on a lifetime's side of the provider shares.
#[derive(Clone)]
struct Side {
    world:    Arc<World>,
    lifetime: u32,
    entry:    ProviderName,
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

    fn failure(what: &str) -> Error {
        Error::Unavailable {
            message: format!("simulated {what} failure"),
            outcome: MutationOutcome::Unknown,
        }
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

    fn reference(&self, id: &str) -> SandboxRef {
        SandboxRef {
            provider:  self.entry.clone(),
            namespace: NamespaceId::try_new(WORLD_NAMESPACE).expect("a constant namespace"),
            id:        SandboxId::try_new(id).expect("the world's sandbox ids are valid"),
        }
    }

    fn gone(id: &str) -> Error {
        Error::NotFound {
            resource: "sandbox".to_owned(),
            id:       id.to_owned(),
        }
    }

    /// End every process running in the sandbox `id`'s `incarnation`,
    /// whichever lifetime ran it.
    fn end_processes(&self, id: &str, incarnation: u32) {
        let mut state = self.world.lock();
        let lifetime = state.lifetime;
        let now = time::Instant::now();
        let mut fenced = 0;
        for process in &state.processes {
            if process.key == id && process.generation == incarnation && process.running(now) {
                if process.lifetime < lifetime {
                    fenced += 1;
                }
                process.state.kill(9);
            }
        }
        state.fenced += fenced;
    }

    /// Stop, start or destroy the sandbox at `index`: check the lease's
    /// intent, crash where planned, apply `effect`, and note the call.
    async fn change(
        &self,
        index: usize,
        call: Call,
        effect: impl FnOnce(&mut WorldSandbox),
    ) -> Result<(), Error> {
        let (id, incarnation, lease) = {
            let state = self.world.lock();
            let sandbox = &state.sandboxes[index];
            (
                sandbox.id.clone(),
                sandbox.incarnation,
                sandbox.lease().map(ToOwned::to_owned),
            )
        };
        let superseded = self.superseded(&id);
        let mut unrecorded = false;
        if let Some(view) = self.view(lease.as_deref()).await {
            let names = view
                .as_ref()
                .and_then(|view| view.resource_id.as_deref())
                .is_some_and(|resource| resource == id);
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
                self.world.violation(format!(
                    "{call:?} of {id} before its lease recorded the intent ({view:?})"
                ));
            }
        }
        let crash = self.count(call);
        if let Some((Moment::Before, crash)) = &crash {
            Self::crash(crash).await;
        }
        let failed = matches!(call, Call::Stop | Call::Delete)
            && self.fails(|faults| faults.release_failure);
        if !failed {
            if matches!(call, Call::Stop | Call::Delete) {
                self.end_processes(&id, incarnation);
            }
            let mut state = self.world.lock();
            effect(&mut state.sandboxes[index]);
        }
        self.record(call, &id, unrecorded, failed);
        if let Some((Moment::After, crash)) = &crash {
            Self::crash(crash).await;
        }
        self.delay(|faults| faults.release_ms).await;
        if failed {
            return Err(Self::failure(&format!("{call:?}").to_lowercase()));
        }
        Ok(())
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
            entry:    self.entry.clone(),
        }
    }

    fn handle(&self, index: usize) -> Box<dyn Sandbox> {
        let (id, incarnation) = {
            let state = self.world.lock();
            let sandbox = &state.sandboxes[index];
            (sandbox.id.clone(), sandbox.incarnation)
        };
        Box::new(WorldSandboxHandle {
            side: self.side(),
            id,
            incarnation,
            env: BTreeMap::new(),
        })
    }
}

#[async_trait]
impl ProviderRuntime for WorldProvider {
    async fn describe(&self) -> Result<Descriptor, Error> {
        Ok(Descriptor {
            schemas:      schemas(),
            namespace:    Namespace {
                id:    NamespaceId::try_new(WORLD_NAMESPACE).expect("a constant namespace"),
                scope: NamespaceScope::Machine,
            },
            isolation:    Isolation::Container,
            durability:   Durability::Machine,
            access:       AccessScope::Shared,
            sources:      [SourceKind::Image].into(),
            locality:     Locality::default(),
            resources:    BTreeSet::new(),
            network:      [NetworkMode::ProviderDefault].into(),
            ports:        PortSupport::None,
            capabilities: CapabilitySet {
                container_fields: [ContainerField::Init].into(),
                ..CapabilitySet::default()
            },
        })
    }

    async fn health(&self) -> Result<Health, Error> {
        Ok(Health {
            healthy: true,
            message: None,
        })
    }

    /// A sandbox named as the spec asks, carrying its labels. It exists from
    /// the call's start, whether or not the call returns.
    async fn create(&self, admission: WorkloadAdmission) -> Result<Box<dyn Sandbox>, Error> {
        let spec = admission.spec;
        let side = self.side();
        side.alive().await;
        let id = spec.name.clone().ok_or_else(|| Error::InvalidSpec {
            path:    "name".to_owned(),
            message: "the world names every sandbox".to_owned(),
        })?;
        let superseded = side.superseded(&id);
        if let Some(Some(view)) = side
            .view(spec.labels.get(LEASE_LABEL).map(String::as_str))
            .await
            && !superseded
            && (view.state != LeaseState::Allocating || !view.admitted || view.pending.is_some())
        {
            self.world.violation(format!(
                "created {id} before its lease recorded an admitted allocation ({view:?})"
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
                return Err(Error::AlreadyExists {
                    name: id.clone(),
                    id,
                });
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
            return Err(Side::failure("create"));
        }
        Ok(self.handle(index))
    }

    async fn attach(&self, id: &SandboxId) -> Result<Box<dyn Sandbox>, Error> {
        self.side().alive().await;
        let index = live_index(&self.world.lock().sandboxes, id.as_str());
        index
            .map(|index| self.handle(index))
            .ok_or_else(|| Side::gone(id.as_str()))
    }

    async fn list(&self, filter: &LabelFilter) -> Result<Vec<SandboxSummary>, Error> {
        let side = self.side();
        side.alive().await;
        let state = self.world.lock();
        Ok(state
            .sandboxes
            .iter()
            .filter(|sandbox| sandbox.state != SandboxState::Deleted)
            .filter(|sandbox| filter.matches(&sandbox.labels))
            .map(|sandbox| SandboxSummary {
                reference: side.reference(&sandbox.id),
                name:      Some(sandbox.id.clone()),
                state:     sandbox.state,
                labels:    sandbox.labels.clone(),
            })
            .collect())
    }

    /// Deletes the live sandbox `id`; one already gone is deleted.
    async fn destroy(&self, id: &SandboxId) -> Result<(), Error> {
        let side = self.side();
        side.alive().await;
        let Some(index) = live_index(&self.world.lock().sandboxes, id.as_str()) else {
            return Ok(());
        };
        side.change(index, Call::Delete, |sandbox| {
            sandbox.state = SandboxState::Deleted;
        })
        .await
    }

    async fn shutdown(&self) -> Result<(), Error> {
        Ok(())
    }
}

/// A sandbox, as the lifetime that attached or created it holds it.
struct WorldSandboxHandle {
    side:        Side,
    id:          String,
    incarnation: u32,
    /// Empty: a simulated process reads its parameters from its own
    /// environment only.
    env:         BTreeMap<String, String>,
}

impl WorldSandboxHandle {
    /// The sandbox this handle names, if it still exists.
    fn index(&self) -> Option<usize> {
        let state = self.side.world.lock();
        state.sandboxes.iter().position(|sandbox| {
            sandbox.id == self.id
                && sandbox.incarnation == self.incarnation
                && sandbox.state != SandboxState::Deleted
        })
    }

    async fn change(
        &self,
        call: Call,
        effect: impl FnOnce(&mut WorldSandbox),
    ) -> Result<(), Error> {
        self.side.alive().await;
        let Some(index) = self.index() else {
            return Err(Side::gone(&self.id));
        };
        self.side.change(index, call, effect).await
    }

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

    /// One simulated process, from the step's `SIM_*` parameters: its lines
    /// go to the sink, and it runs for its work time unless the caller's
    /// `term` or `kill`, or the sandbox's stop, ends it first.
    async fn process(&self, spec: &ExecSpec, controls: ExecControls) -> Result<ExecOutcome, Error> {
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
            return Err(Error::Busy {
                message: format!("{} is not running", self.id),
            });
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
                        ended_by = Termination::Terminated;
                        process.kill(15);
                    }
                }
            }
        };
        Ok(ExecOutcome {
            termination: if exit.signal.is_some() {
                ended_by
            } else {
                Termination::Exited
            },
            exit_code:   exit.code,
            signal:      exit.signal,
            output_loss: sandbox_driver::OutputLoss::default(),
        })
    }
}

#[async_trait]
impl Sandbox for WorldSandboxHandle {
    fn reference(&self) -> SandboxRef {
        self.side.reference(&self.id)
    }

    fn working_directory(&self) -> &Path {
        Path::new(WORKING_DIRECTORY)
    }

    fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    async fn status(&self) -> Result<Status, Error> {
        self.side.alive().await;
        let state = self.side.world.lock();
        let sandbox = state
            .sandboxes
            .iter()
            .rev()
            .find(|sandbox| sandbox.id == self.id && sandbox.incarnation == self.incarnation)
            .ok_or_else(|| Side::gone(&self.id))?;
        Ok(Status {
            reference: self.reference(),
            state:     sandbox.state,
            name:      Some(sandbox.id.clone()),
            labels:    sandbox.labels.clone(),
            image:     None,
            snapshot:  None,
            resources: Resources::default(),
            network:   None,
            services:  BTreeMap::new(),
            web_url:   None,
        })
    }

    async fn start(&self) -> Result<(), Error> {
        self.change(Call::Start, |sandbox| {
            sandbox.state = SandboxState::Running;
            sandbox.starts += 1;
        })
        .await
    }

    async fn stop(&self) -> Result<(), Error> {
        self.change(Call::Stop, |sandbox| {
            sandbox.state = SandboxState::Stopped;
            sandbox.stops += 1;
        })
        .await
    }

    /// The executor's `mkdir -p` of a step's working directory succeeds and
    /// does nothing; every other command is a simulated process.
    async fn exec(&self, spec: ExecSpec, controls: ExecControls) -> Result<ExecOutcome, Error> {
        self.side.alive().await;
        if spec.argv.first().map(String::as_str) == Some("mkdir") {
            return Ok(ExecOutcome {
                termination: Termination::Exited,
                exit_code:   Some(0),
                signal:      None,
                output_loss: sandbox_driver::OutputLoss::default(),
            });
        }
        self.process(&spec, controls).await
    }

    fn files(&self) -> &dyn Files {
        &WorldFiles
    }

    fn ports(&self) -> &dyn Ports {
        &WorldFiles
    }
}

/// The files facet: nothing is kept, and every write succeeds. No port is
/// exposed.
struct WorldFiles;

#[async_trait]
impl Files for WorldFiles {
    async fn get(&self, path: &str, _range: Option<FileRange>) -> Result<ByteStream, Error> {
        Err(Error::NotFound {
            resource: "file".to_owned(),
            id:       path.to_owned(),
        })
    }

    async fn put(&self, _path: &str, _bytes: ByteStream) -> Result<(), Error> {
        Ok(())
    }

    async fn list(&self, _path: &str, _depth: u32) -> Result<Vec<FileEntry>, Error> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl Ports for WorldFiles {
    async fn expose(&self, _port: NonZeroU16, _options: Expose) -> Result<Exposure, Error> {
        Err(Error::Unsupported {
            capability: "ports".to_owned(),
        })
    }

    async fn release(&self, _port: NonZeroU16) -> Result<(), Error> {
        Ok(())
    }
}
