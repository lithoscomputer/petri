//! Run controls a host drives while a run is live: pause and unpause at node
//! admission, steering into an active stage, interrupting an agent's model
//! turn, and cancellation.
//!
//! One [`ControlService`] serves the terminal (`petri run --control <path>`)
//! and an embedded host alike. It is built from pieces that already exist:
//! the coordinator's handle (cancellation and delivery), the driver's awaited
//! [`ExecutionHooks::before_attempt`] admission point (the pause), an
//! [`ExecutionObserver`] that keeps the live firing of each node so a steer
//! can name a stage instead of a firing, and the [`LiveTurns`] capability
//! an agent step marks while one of its model turns runs, so an interrupt
//! knows whether there is a turn to stop.
//!
//! # Pause
//!
//! A paused run admits no new attempt: the service's `before_attempt` waits
//! until the run is unpaused, so a firing that was about to start keeps its
//! one identity, starts no attempt, and does not count as a visit twice.
//! Work already running keeps running, and cancellation stays responsive: a
//! cancel settles a firing that is waiting on admission as `Cancelled`, the
//! way the engine always has.
//!
//! The pause is durable. Each pause and unpause is a coordinator record
//! (`RunPaused`, `RunUnpaused`), so `replay_run` carries `run_paused` and
//! `run_unpaused`, `petri inspect` reports `paused`, and a resume whose last
//! recorded control was a pause starts with admission held: the service is
//! handed the replayed state ([`ExecutionObserver::on_resumed`]) before the
//! first attempt is admitted, and nothing runs until an unpause arrives. The
//! two controls order themselves against the record differently. A pause
//! holds admission at once and records after: holding early is safe. An
//! unpause records first and releases after, so a crash right after an
//! unpause never resumes paused; [`ControlService::unpause`] is therefore
//! `async` and returns once the record is durable.
//!
//! # Steering
//!
//! A steer is a [`Steer`] payload delivered to a live firing. It is never an
//! answer: a human gate ignores it and keeps its question open, and an agent
//! step queues it as guidance for its session. Control input therefore cannot
//! consume a pending question's answer.
//!
//! # Interrupting
//!
//! An interrupt is an [`Interrupt`] payload delivered to a live firing whose
//! agent stage has a model turn in flight. The stage stops the turn (the
//! model request and the tool calls it is running), keeps its session, and
//! continues with its next input: the `steer` text given with the interrupt,
//! else the next text delivered to the stage. A stage with no turn in flight
//! (a human gate, a command, an agent between turns) refuses it with
//! [`ControlError::NoLiveTurn`]; so does a backend that cannot stop a turn,
//! because it never marks one live. The check reads [`LiveTurns`], which the
//! host installs as a capability ([`ControlService::turns`]) beside the
//! service's hooks; without it no turn is ever live and every interrupt is
//! refused.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use driver::lifecycle::{
    AdmitAttempt, AttemptDecision, ExecutionHooks, HookContext, Note, PrepareError, PrepareResult,
    Prepared, Recorded, RunFinished, ScopeAcquired, ScopeAcquiredError, ScopeReleased, Transition,
    TransitionError, TransitionReport,
};
use engine::{EngineState, Event, EventRecord};
use ir::FiringId;
use smol_str::SmolStr;
use steps::{Interrupt, Steer};
use tokio::sync::{Notify, watch};

use crate::{
    CancelReason, CoordinatorEvent, CoordinatorHandle, CoordinatorRecord, CoordinatorState,
    ExecutionId, ExecutionObserver, InvocationId,
};

/// The service's [`ExecutionHooks`]: `before_attempt` holds while paused and
/// every other point delegates to the host's own hooks, when it has some.
/// Install it with [`Runtime::hooks`](runtime::Runtime::hooks).
pub struct PauseHooks {
    paused: watch::Receiver<bool>,
    gate:   Arc<Gate>,
    inner:  Option<Arc<dyn ExecutionHooks>>,
}

/// What held attempts wait on. One notifier wakes them in the order they
/// began to wait, so a run released from a pause admits its held attempts
/// the same way every time; a `watch` channel wakes its waiters in an order
/// it picks at random.
#[derive(Default)]
struct Gate {
    released: Notify,
    /// The service is gone: nothing will ever release the gate.
    closed:   AtomicBool,
}

#[async_trait::async_trait]
impl ExecutionHooks for PauseHooks {
    async fn before_attempt(
        &self,
        context: &HookContext,
        request: AdmitAttempt,
    ) -> AttemptDecision {
        let mut paused = self.paused.clone();
        if *paused.borrow() {
            tracing::info!(
                firing = request.view.firing.raw(),
                attempt = request.view.attempt.raw(),
                node = request.view.node_name(),
                "admission held: the run is paused"
            );
            // A dropped service releases every held attempt rather than hold
            // the run hostage to a controller that is gone.
            loop {
                let released = self.gate.released.notified();
                tokio::pin!(released);
                released.as_mut().enable();
                if !*paused.borrow_and_update() || self.gate.closed.load(Ordering::Acquire) {
                    break;
                }
                released.await;
            }
            tracing::info!(
                firing = request.view.firing.raw(),
                node = request.view.node_name(),
                "admission released"
            );
        }
        match &self.inner {
            Some(inner) => inner.before_attempt(context, request).await,
            None => AttemptDecision::admit(),
        }
    }

    async fn prepare_result(
        &self,
        context: &HookContext,
        request: PrepareResult,
    ) -> Result<Prepared, PrepareError> {
        match &self.inner {
            Some(inner) => inner.prepare_result(context, request).await,
            None => Ok(Prepared::unchanged()),
        }
    }

    async fn after_record(&self, context: &HookContext, recorded: Recorded) -> Vec<Note> {
        match &self.inner {
            Some(inner) => inner.after_record(context, recorded).await,
            None => Vec::new(),
        }
    }

    async fn transition(
        &self,
        context: &HookContext,
        transition: Transition,
    ) -> Result<TransitionReport, TransitionError> {
        match &self.inner {
            Some(inner) => inner.transition(context, transition).await,
            None => Ok(TransitionReport::default()),
        }
    }

    fn requires_run_finalization(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.requires_run_finalization())
    }

    async fn finalize_run(
        &self,
        context: &HookContext,
        finished: RunFinished,
    ) -> Result<(), ir::FinalizationFailure> {
        match &self.inner {
            Some(inner) => inner.finalize_run(context, finished).await,
            None => Ok(()),
        }
    }

    async fn run_finished(&self, context: &HookContext, finished: RunFinished) -> Vec<Note> {
        match &self.inner {
            Some(inner) => inner.run_finished(context, finished).await,
            None => Vec::new(),
        }
    }

    async fn scope_released(&self, context: &HookContext, released: ScopeReleased) -> Vec<Note> {
        match &self.inner {
            Some(inner) => inner.scope_released(context, released).await,
            None => Vec::new(),
        }
    }

    async fn scope_acquired(
        &self,
        context: &HookContext,
        acquired: ScopeAcquired,
    ) -> Result<(), ScopeAcquiredError> {
        match &self.inner {
            Some(inner) => inner.scope_acquired(context, acquired).await,
            None => Ok(()),
        }
    }
}

/// Where a live firing runs, for a steer by node name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveStage {
    pub invocation: InvocationId,
    pub execution:  ExecutionId,
    pub firing:     FiringId,
}

#[derive(Default)]
struct Live {
    executions: BTreeMap<ExecutionId, InvocationId>,
    /// Live firings by node instance name; a name running in two executions
    /// keeps the latest.
    stages:     BTreeMap<SmolStr, LiveStage>,
    firings:    BTreeMap<(ExecutionId, FiringId), SmolStr>,
}

/// Why a control could not be applied.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ControlError {
    #[error("no stage named `{0}` is running")]
    NoSuchStage(String),
    #[error("the stage is no longer live")]
    NotLive,
    /// The stage has no model turn in flight to interrupt: it is not an
    /// agent, its agent is between turns, or its backend cannot stop one.
    #[error("the stage has no model turn to interrupt")]
    NoLiveTurn,
    #[error("the run has finished")]
    Finished,
}

/// The model turns in flight, by execution and firing: what an interrupt can
/// reach. An agent step holds a [`LiveTurn`] from [`LiveTurns::begin`] while
/// a turn runs; the guard's drop ends the mark. Installed on the runtime as a
/// capability so the step finds it; clones share one set.
#[derive(Clone, Default)]
pub struct LiveTurns {
    turns: Arc<Mutex<BTreeSet<(ExecutionId, FiringId)>>>,
}

impl LiveTurns {
    pub fn new() -> Self {
        Self::default()
    }

    fn set(&self) -> MutexGuard<'_, BTreeSet<(ExecutionId, FiringId)>> {
        self.turns.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mark a turn of `firing` live until the guard drops.
    pub fn begin(&self, execution: ExecutionId, firing: FiringId) -> LiveTurn {
        self.set().insert((execution, firing));
        LiveTurn {
            turns: self.clone(),
            key:   (execution, firing),
        }
    }

    /// Whether `firing` has a turn in flight.
    pub fn is_live(&self, execution: ExecutionId, firing: FiringId) -> bool {
        self.set().contains(&(execution, firing))
    }
}

/// A turn marked live; dropping it ends the mark.
#[must_use = "the turn is live only while the guard is held"]
pub struct LiveTurn {
    turns: LiveTurns,
    key:   (ExecutionId, FiringId),
}

impl Drop for LiveTurn {
    fn drop(&mut self) {
        self.turns.set().remove(&self.key);
    }
}

struct Inner {
    paused: watch::Sender<bool>,
    gate:   Arc<Gate>,
    handle: Mutex<Option<CoordinatorHandle>>,
    live:   Mutex<Live>,
    turns:  LiveTurns,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.gate.closed.store(true, Ordering::Release);
        self.gate.released.notify_waiters();
    }
}

impl Inner {
    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The one control service. Construct before the run, install its
/// [`hooks`](Self::hooks) and its [`turns`](Self::turns) on the runtime,
/// observe a clone, [`wire`](Self::wire) the handle once the coordinator
/// exists, then drive it from wherever controls come from.
#[derive(Clone)]
pub struct ControlService {
    inner: Arc<Inner>,
}

impl Default for ControlService {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlService {
    pub fn new() -> Self {
        let (paused, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                paused,
                gate: Arc::new(Gate::default()),
                handle: Mutex::new(None),
                live: Mutex::new(Live::default()),
                turns: LiveTurns::new(),
            }),
        }
    }

    /// The live-turn set the agent step marks, for the runtime's
    /// capabilities (`Runtime::capability`). Without it installed, no turn
    /// is ever live and `interrupt` refuses every stage.
    pub fn turns(&self) -> LiveTurns {
        self.inner.turns.clone()
    }

    /// The execution hooks that enforce pauses at `before_attempt`, over the
    /// host's own hooks when it has some. Without them installed, `pause` is
    /// recorded but holds nothing.
    pub fn hooks(&self, inner: Option<Arc<dyn ExecutionHooks>>) -> Arc<dyn ExecutionHooks> {
        Arc::new(PauseHooks {
            paused: self.inner.paused.subscribe(),
            gate: Arc::clone(&self.inner.gate),
            inner,
        })
    }

    /// Hand the service the run's handle. A pause taken before this point is
    /// recorded now; a redundant record (the run resumed paused) is skipped
    /// by the coordinator. The request is queued before this returns, so an
    /// unpause asked for next is recorded after it.
    pub fn wire(&self, handle: CoordinatorHandle) {
        if self.is_paused() {
            handle.request_paused(true);
        }
        *self
            .inner
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(handle);
    }

    fn handle(&self) -> Result<CoordinatorHandle, ControlError> {
        self.inner
            .handle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or(ControlError::Finished)
    }

    pub fn is_paused(&self) -> bool {
        *self.inner.paused.borrow()
    }

    /// Every change of the paused state, for a projector that publishes
    /// `run_paused` and `run_unpaused`.
    pub fn paused_changes(&self) -> watch::Receiver<bool> {
        self.inner.paused.subscribe()
    }

    /// Hold every attempt not yet admitted. Running work is not interrupted.
    /// The hold is immediate; the durable record follows through the
    /// coordinator when the service is wired (else when it is).
    pub fn pause(&self) {
        if self.inner.paused.send_replace(true) {
            return;
        }
        tracing::info!("run paused: new attempts are held at admission");
        if let Ok(handle) = self.handle() {
            handle.request_paused(true);
        }
    }

    /// Let held and future attempts through. The unpause is recorded first
    /// and admission is released once the record is durable, so a crash in
    /// between resumes paused, never the other way round. Without a live
    /// coordinator the release is immediate.
    pub async fn unpause(&self) {
        if !*self.inner.paused.borrow() {
            return;
        }
        if let Ok(handle) = self.handle() {
            handle.set_paused(false).await;
        }
        if self.inner.paused.send_replace(false) {
            tracing::info!("run resumed");
        }
        self.inner.gate.released.notify_waiters();
    }

    /// The live firing of a node instance, by name.
    pub fn stage(&self, node: &str) -> Option<LiveStage> {
        self.inner.live().stages.get(node).cloned()
    }

    /// Every live stage, by node name.
    pub fn stages(&self) -> BTreeMap<SmolStr, LiveStage> {
        self.inner.live().stages.clone()
    }

    /// Deliver guidance to the named stage's live firing.
    pub async fn steer(&self, node: &str, text: impl Into<String>) -> Result<(), ControlError> {
        let stage = self
            .stage(node)
            .ok_or_else(|| ControlError::NoSuchStage(node.to_owned()))?;
        self.steer_firing(stage.execution, stage.firing, text).await
    }

    /// Deliver guidance to one firing.
    pub async fn steer_firing(
        &self,
        execution: ExecutionId,
        firing: FiringId,
        text: impl Into<String>,
    ) -> Result<(), ControlError> {
        let handle = self.handle()?;
        match handle
            .deliver(execution, firing, Steer::new(text).to_control())
            .await
        {
            driver::DeliverDisposition::Delivered => Ok(()),
            driver::DeliverDisposition::NotLive => Err(ControlError::NotLive),
        }
    }

    /// Stop the named stage's current model turn and keep its session; the
    /// next text delivered to the stage is its next input.
    pub async fn interrupt(&self, node: &str) -> Result<(), ControlError> {
        let stage = self
            .stage(node)
            .ok_or_else(|| ControlError::NoSuchStage(node.to_owned()))?;
        self.interrupt_firing(stage.execution, stage.firing, Interrupt::new())
            .await
    }

    /// Stop the named stage's current model turn and make `text` its next
    /// input, in one control.
    pub async fn interrupt_and_steer(
        &self,
        node: &str,
        text: impl Into<String>,
    ) -> Result<(), ControlError> {
        let stage = self
            .stage(node)
            .ok_or_else(|| ControlError::NoSuchStage(node.to_owned()))?;
        self.interrupt_firing(stage.execution, stage.firing, Interrupt::and_steer(text))
            .await
    }

    /// Deliver an interrupt to one firing. Refused with
    /// [`ControlError::NoLiveTurn`] when the firing has no model turn in
    /// flight; a turn that ends between the check and the delivery receives
    /// the control anyway, and the stage treats the steer text, if any, as
    /// ordinary guidance.
    pub async fn interrupt_firing(
        &self,
        execution: ExecutionId,
        firing: FiringId,
        interrupt: Interrupt,
    ) -> Result<(), ControlError> {
        let handle = self.handle()?;
        if !self.inner.turns.is_live(execution, firing) {
            return Err(ControlError::NoLiveTurn);
        }
        match handle
            .deliver(execution, firing, interrupt.to_control())
            .await
        {
            driver::DeliverDisposition::Delivered => Ok(()),
            driver::DeliverDisposition::NotLive => Err(ControlError::NotLive),
        }
    }

    /// Cancel the whole run politely; a second call reaches the kill tier.
    pub fn cancel(&self) -> Result<(), ControlError> {
        self.handle()?.cancel_root_for(CancelReason::Control);
        Ok(())
    }
}

impl ExecutionObserver for ControlService {
    fn on_engine_record(
        &self,
        execution: ExecutionId,
        record: &EventRecord,
        _recorded_at: u64,
        state: &EngineState,
    ) {
        match &record.event {
            Event::StepStarted { firing, .. } => {
                let Some(name) = state
                    .firing_node(*firing)
                    .and_then(|id| state.graph().node(id))
                    .map(|node| node.name.clone())
                else {
                    return;
                };
                let mut live = self.inner.live();
                let invocation = live
                    .executions
                    .get(&execution)
                    .copied()
                    .unwrap_or(InvocationId::ROOT);
                live.stages.insert(name.clone(), LiveStage {
                    invocation,
                    execution,
                    firing: *firing,
                });
                live.firings.insert((execution, *firing), name);
            }
            Event::StepFinished { firing, .. } => {
                let mut live = self.inner.live();
                if let Some(name) = live.firings.remove(&(execution, *firing))
                    && live.stages.get(&name).is_some_and(|stage| {
                        stage.execution == execution && stage.firing == *firing
                    })
                {
                    live.stages.remove(&name);
                }
            }
            _ => {}
        }
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        if let CoordinatorEvent::ExecutionDeclared {
            execution,
            invocation,
            ..
        } = &record.body
        {
            self.inner.live().executions.insert(*execution, *invocation);
        }
    }

    /// Start where the log left the run: held at admission when the last
    /// recorded control was a pause, and knowing every declared execution.
    fn on_resumed(&self, state: &CoordinatorState) {
        {
            let mut live = self.inner.live();
            for (execution, declared) in &state.executions {
                live.executions
                    .insert(*execution, declared.declaration.invocation);
            }
        }
        if state.paused && !self.inner.paused.send_replace(true) {
            tracing::info!("resumed paused: attempts are held until an unpause");
        }
    }
}
