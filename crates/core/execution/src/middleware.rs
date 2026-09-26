use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use driver::{
    AdmissionResolution, AdmitRequest, DecisionError, DecisionResolver, DefaultDecisionResolver,
    RoutingRequest, RoutingResolution, default_group_decision,
};
use engine::{
    Admission, DecisionId, Event, EventRecord, GroupDecision, Intervention, MiddlewareKey,
    RouteDecision, RoutingProposal,
};
use ir::{Attempt, FiringId, NodeId, Outcome, Value};
use smol_str::SmolStr;
use tokio::task::JoinSet;

use crate::{ExecutionId, InvocationId};

pub type MiddlewareState = BTreeMap<MiddlewareKey, (u32, Value)>;

#[derive(Clone, Debug)]
pub enum FoldEvent<'a> {
    ExecutionStarted,
    FinalOutcome {
        firing:  FiringId,
        node:    NodeId,
        outcome: &'a Outcome,
    },
    RouteApplied {
        firing:   FiringId,
        decision: RouteDecision,
        /// The applied edge restarts the execution (`EdgeTransition::Restart`).
        restart:  bool,
    },
}

/// The middleware fold projection of one engine event.
///
/// The one definition both fold paths share: the live observer derives from the
/// post-apply state, and the resume rebuild derives from a replayed prefix —
/// only their finality and node lookups differ. Checkpointed middleware state
/// is validated across resume, so the two paths must fold identically.
pub(crate) fn derive_fold_event(
    event: &Event,
    is_final_attempt: impl Fn(FiringId, Attempt) -> bool,
    node_of: impl Fn(FiringId) -> Option<NodeId>,
    is_restart_edge: impl Fn(ir::EdgeId) -> bool,
) -> Option<FoldEvent<'_>> {
    match event {
        Event::ExecutionStarted { .. } => Some(FoldEvent::ExecutionStarted),
        Event::StepFinished {
            firing,
            attempt,
            outcome,
        } if is_final_attempt(*firing, *attempt) => {
            node_of(*firing).map(|node| FoldEvent::FinalOutcome {
                firing: *firing,
                node,
                outcome,
            })
        }
        Event::RouteApplied { applied } => {
            let decision = applied.decision();
            let restart = matches!(decision, RouteDecision::Emit(edge) if is_restart_edge(edge));
            Some(FoldEvent::RouteApplied {
                firing: applied.firing(),
                decision,
                restart,
            })
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecisionAddress {
    pub invocation: InvocationId,
    pub execution:  ExecutionId,
    pub decision:   DecisionId,
}

#[derive(Clone, Debug)]
pub struct AdmitCall {
    pub address: DecisionAddress,
    pub state:   Value,
}

#[derive(Clone, Debug)]
pub struct RouteCall {
    pub address:  DecisionAddress,
    pub firing:   FiringId,
    pub proposal: Arc<RoutingProposal>,
    pub state:    Value,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct MiddlewareError {
    message: SmolStr,
}

impl MiddlewareError {
    pub fn new(message: impl Into<SmolStr>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

type DecisionFuture<'a, D> = Pin<Box<dyn Future<Output = Result<D, MiddlewareError>> + Send + 'a>>;
type LayerFuture<D, T> =
    Pin<Box<dyn Future<Output = Result<Resolved<D, T>, MiddlewareError>> + Send>>;

/// The rest of the chain below one layer, as the layer calls it.
pub struct Next<'a, D> {
    call: Box<dyn Fn() -> DecisionFuture<'a, D> + Send + Sync + 'a>,
}

impl<D> Next<'_, D> {
    pub async fn run(&self) -> Result<D, MiddlewareError> {
        (self.call)().await
    }

    /// A next that returns one fixed decision — the seam a middleware
    /// test drives a layer over, as the coordinator's inner chain would.
    #[cfg(test)]
    pub(crate) fn from_decision(decision: Result<D, MiddlewareError>) -> Self
    where
        D: Clone + Send + Sync + 'static,
    {
        Self {
            call: Box::new(move || Box::pin(std::future::ready(decision.clone()))),
        }
    }
}

pub type AdmitNext<'a> = Next<'a, Admission>;
pub type RouteNext<'a> = Next<'a, RouteDecision>;

#[async_trait::async_trait]
pub trait Middleware: Send + Sync {
    fn key(&self) -> MiddlewareKey;
    fn state_version(&self) -> u32;
    fn initial_state(&self) -> Value;

    fn fold(&self, state: &mut Value, event: &FoldEvent<'_>) -> Result<(), MiddlewareError>;

    async fn admit(
        &self,
        _call: AdmitCall,
        next: AdmitNext<'_>,
    ) -> Result<Admission, MiddlewareError> {
        next.run().await
    }

    async fn route(
        &self,
        _call: RouteCall,
        next: RouteNext<'_>,
    ) -> Result<RouteDecision, MiddlewareError> {
        next.run().await
    }
}

pub fn initial_middleware_state(chain: &[Arc<dyn Middleware>]) -> MiddlewareState {
    chain
        .iter()
        .map(|middleware| {
            (
                middleware.key(),
                (middleware.state_version(), middleware.initial_state()),
            )
        })
        .collect()
}

pub fn validate_middleware_state(
    chain: &[Arc<dyn Middleware>],
    state: &MiddlewareState,
) -> Result<(), MiddlewareError> {
    for middleware in chain {
        let key = middleware.key();
        let Some((version, _)) = state.get(&key) else {
            return Err(MiddlewareError::new(format!(
                "middleware state for `{key}` is missing"
            )));
        };
        if *version != middleware.state_version() {
            return Err(MiddlewareError::new(format!(
                "middleware `{key}` state version is {version}; expected {}",
                middleware.state_version()
            )));
        }
    }
    if state.len() != chain.len() {
        return Err(MiddlewareError::new(
            "middleware state contains an unconfigured key",
        ));
    }
    Ok(())
}

/// A chain layer's decision with the interventions recorded below it.
#[derive(Clone)]
struct Resolved<D, T> {
    decision: D,
    trace:    Vec<T>,
}

/// Combine a layer's decision with the downstream result: the downstream trace
/// carries forward, and the layer is prepended only when it changed the
/// decision.
fn resolve_layer<D: PartialEq, T>(
    downstream: Option<Resolved<D, T>>,
    decision: D,
    entry: impl FnOnce(&D) -> T,
) -> Resolved<D, T> {
    let diverged = downstream
        .as_ref()
        .is_none_or(|value| value.decision != decision);
    let mut trace = downstream.map_or_else(Vec::new, |value| value.trace);
    if diverged {
        trace.insert(0, entry(&decision));
    }
    Resolved { decision, trace }
}

/// One execution-bound middleware chain and its externally owned state.
pub struct MiddlewarePipeline {
    invocation: InvocationId,
    execution:  ExecutionId,
    chain:      Arc<[Arc<dyn Middleware>]>,
    state:      Arc<RwLock<MiddlewareState>>,
}

impl MiddlewarePipeline {
    pub fn new(
        invocation: InvocationId,
        execution: ExecutionId,
        chain: Vec<Arc<dyn Middleware>>,
        state: MiddlewareState,
    ) -> Result<Self, MiddlewareError> {
        validate_middleware_state(&chain, &state)?;
        Ok(Self {
            invocation,
            execution,
            chain: Arc::from(chain),
            state: Arc::new(RwLock::new(state)),
        })
    }

    pub fn checkpoint(&self) -> MiddlewareState {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }

    pub fn fold_observer(&self) -> MiddlewareFoldObserver {
        MiddlewareFoldObserver {
            chain:   self.chain.clone(),
            state:   self.state.clone(),
            failure: Arc::new(Mutex::new(None)),
        }
    }

    pub fn fold(&self, event: &FoldEvent<'_>) -> Result<(), MiddlewareError> {
        let mut states = self.state.write().unwrap_or_else(PoisonError::into_inner);
        fold_states(&self.chain, &mut states, event)
    }

    fn address(&self, decision: DecisionId) -> DecisionAddress {
        DecisionAddress {
            invocation: self.invocation,
            execution: self.execution,
            decision,
        }
    }
}

#[async_trait::async_trait]
impl DecisionResolver for MiddlewarePipeline {
    async fn admit(&self, request: AdmitRequest) -> Result<AdmissionResolution, DecisionError> {
        let resolved = admit_at(
            self.chain.clone(),
            self.state.clone(),
            self.address(request.decision_id),
        )
        .await
        .map_err(|error| DecisionError::new(error.message()))?;
        Ok(AdmissionResolution {
            decision: resolved.decision,
            trace:    resolved.trace,
        })
    }

    async fn route(&self, request: RoutingRequest) -> Result<RoutingResolution, DecisionError> {
        let DecisionId::Route { firing, .. } = request.decision_id else {
            return Err(DecisionError::new(
                "a routing request must carry a route decision id",
            ));
        };
        let decision_id = request.decision_id;
        let restart_allowed = request.restart_allowed;
        let group_count = request.groups.len();
        let mut tasks = JoinSet::new();
        for (index, proposal) in request.groups.into_iter().enumerate() {
            let baseline = default_group_decision(&proposal, restart_allowed)?;
            let proposal = Arc::new(proposal);
            let chain = self.chain.clone();
            let state = self.state.clone();
            let address = self.address(decision_id);
            tasks.spawn(async move {
                let resolved = route_at(
                    chain,
                    state,
                    address,
                    firing,
                    proposal.clone(),
                    baseline.decision,
                )
                .await?;
                let decision =
                    engine::enforce_restart_limit(restart_allowed, &proposal, resolved.decision);
                Ok::<_, MiddlewareError>((index, GroupDecision {
                    group: proposal.group,
                    draw: baseline.draw,
                    trace: resolved.trace,
                    decision,
                }))
            });
        }
        let mut groups: Vec<Option<GroupDecision>> = (0..group_count).map(|_| None).collect();
        while let Some(result) = tasks.join_next().await {
            let (index, group) = result
                .map_err(|error| {
                    DecisionError::new(format!("routing middleware task failed: {error}"))
                })?
                .map_err(|error| DecisionError::new(error.message()))?;
            groups[index] = Some(group);
        }
        let groups = groups
            .into_iter()
            .map(|group| group.expect("every routing task returns one group"))
            .collect();
        Ok(RoutingResolution { groups })
    }

    fn admit_now(&self, request: &AdmitRequest) -> Option<AdmissionResolution> {
        if self.chain.is_empty() {
            DefaultDecisionResolver.admit_now(request)
        } else {
            None
        }
    }

    fn route_now(&self, request: &RoutingRequest) -> Option<RoutingResolution> {
        if self.chain.is_empty() {
            DefaultDecisionResolver.route_now(request)
        } else {
            None
        }
    }
}

/// One layer's call: build the typed call payload and invoke the layer's
/// trait method with the rest of the chain behind `next`.
type Invoke<D> =
    dyn for<'a> Fn(Arc<dyn Middleware>, Value, Next<'a, D>) -> DecisionFuture<'a, D> + Send + Sync;

/// How a layer's decision renders into the trace.
type TraceEntry<D, T> = dyn Fn(MiddlewareKey, &D) -> T + Send + Sync;

fn admit_at(
    chain: Arc<[Arc<dyn Middleware>]>,
    state: Arc<RwLock<MiddlewareState>>,
    address: DecisionAddress,
) -> LayerFuture<Admission, MiddlewareKey> {
    layer_at(
        0,
        chain,
        state,
        Admission::Admit,
        Arc::new(move |middleware, state, next| {
            Box::pin(async move { middleware.admit(AdmitCall { address, state }, next).await })
        }),
        Arc::new(|key, _| key),
    )
}

fn route_at(
    chain: Arc<[Arc<dyn Middleware>]>,
    state: Arc<RwLock<MiddlewareState>>,
    address: DecisionAddress,
    firing: FiringId,
    proposal: Arc<RoutingProposal>,
    baseline: RouteDecision,
) -> LayerFuture<RouteDecision, Intervention> {
    layer_at(
        0,
        chain,
        state,
        baseline,
        Arc::new(move |middleware, state, next| {
            let proposal = proposal.clone();
            Box::pin(async move {
                middleware
                    .route(
                        RouteCall {
                            address,
                            firing,
                            proposal,
                            state,
                        },
                        next,
                    )
                    .await
            })
        }),
        Arc::new(intervention),
    )
}

/// The chain recursion both decision kinds share: resolve the layer at
/// `index`, giving it the rest of the chain as `next` and capturing what the
/// downstream layers resolved so the trace composes.
fn layer_at<D, T>(
    index: usize,
    chain: Arc<[Arc<dyn Middleware>]>,
    state: Arc<RwLock<MiddlewareState>>,
    default: D,
    invoke: Arc<Invoke<D>>,
    entry: Arc<TraceEntry<D, T>>,
) -> LayerFuture<D, T>
where
    D: Clone + PartialEq + Send + Sync + 'static,
    T: Send + 'static,
{
    Box::pin(async move {
        let Some(middleware) = chain.get(index).cloned() else {
            return Ok(Resolved {
                decision: default,
                trace:    Vec::new(),
            });
        };
        let captured = Arc::new(Mutex::new(None));
        let next = Next {
            call: Box::new({
                let chain = chain.clone();
                let state = state.clone();
                let captured = captured.clone();
                let default = default.clone();
                let invoke = invoke.clone();
                let entry = entry.clone();
                move || {
                    let chain = chain.clone();
                    let state = state.clone();
                    let captured = captured.clone();
                    let default = default.clone();
                    let invoke = invoke.clone();
                    let entry = entry.clone();
                    Box::pin(async move {
                        let resolved =
                            layer_at(index + 1, chain, state, default, invoke, entry).await?;
                        let decision = resolved.decision.clone();
                        *captured.lock().unwrap_or_else(PoisonError::into_inner) = Some(resolved);
                        Ok(decision)
                    })
                }
            }),
        };
        let key = middleware.key();
        let middleware_state = read_state(&state, &key)?;
        let decision = invoke(middleware, middleware_state, next).await?;
        let downstream = captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        Ok(resolve_layer(downstream, decision, |decision| {
            entry(key, decision)
        }))
    })
}

fn read_state(
    state: &RwLock<MiddlewareState>,
    key: &MiddlewareKey,
) -> Result<Value, MiddlewareError> {
    state
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(key)
        .map(|(_, state)| state.clone())
        .ok_or_else(|| MiddlewareError::new(format!("middleware state for `{key}` is missing")))
}

fn intervention(key: MiddlewareKey, decision: &RouteDecision) -> Intervention {
    match decision {
        RouteDecision::Emit(edge) => Intervention::Override {
            middleware: key,
            edge:       *edge,
        },
        RouteDecision::Jump(target) => Intervention::Jump {
            middleware: key,
            target:     *target,
        },
        RouteDecision::Block { reason } => Intervention::Block {
            middleware: key,
            reason:     reason.clone(),
        },
        RouteDecision::None => Intervention::Block {
            middleware: key,
            reason:     SmolStr::new("middleware selected no route"),
        },
    }
}

#[derive(Clone)]
pub struct MiddlewareFoldObserver {
    chain:   Arc<[Arc<dyn Middleware>]>,
    state:   Arc<RwLock<MiddlewareState>>,
    failure: Arc<Mutex<Option<MiddlewareError>>>,
}

#[async_trait::async_trait]
impl driver::EventObserver for MiddlewareFoldObserver {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, state: &engine::EngineState) {
        if self.chain.is_empty() {
            return;
        }
        // Finality: `apply` records the final attempt in history in the same
        // transition, so the matching entry sits at (or near) the tail.
        let fold = derive_fold_event(
            &record.event,
            |firing, attempt| {
                state
                    .history()
                    .iter()
                    .rev()
                    .any(|entry| entry.firing == firing && entry.attempt == attempt)
            },
            |firing| state.firing_node(firing),
            |edge| {
                state
                    .graph()
                    .edge(edge)
                    .is_some_and(|edge| edge.transition == ir::EdgeTransition::Restart)
            },
        );
        if let Some(event) = fold {
            self.apply_fold(&event);
        }
    }

    async fn finish(&self) -> Result<(), driver::ObserveError> {
        match self
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            Some(error) => Err(driver::ObserveError::new(
                "middleware fold",
                error.to_string(),
            )),
            None => Ok(()),
        }
    }
}

impl MiddlewareFoldObserver {
    fn apply_fold(&self, event: &FoldEvent<'_>) {
        let mut states = self.state.write().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = fold_states(&self.chain, &mut states, event) {
            *self.failure.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
        }
    }
}

/// Fold one event into every layer's state, in chain order. The one loop both
/// the pipeline's direct fold and the live observer share.
fn fold_states(
    chain: &[Arc<dyn Middleware>],
    states: &mut MiddlewareState,
    event: &FoldEvent<'_>,
) -> Result<(), MiddlewareError> {
    for middleware in chain {
        let key = middleware.key();
        let (_, value) = states.get_mut(&key).ok_or_else(|| {
            MiddlewareError::new(format!("middleware state for `{key}` is missing"))
        })?;
        middleware.fold(value, event)?;
    }
    Ok(())
}
