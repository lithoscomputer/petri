//! Host-side admission and routing decisions.

use std::sync::{Mutex, PoisonError};

use engine::{
    Admission, DecisionId, GroupDecision, MiddlewareKey, RouteDecision, RoutingProposal,
    WeightedDraw,
};
use ir::PickPolicy;
use smol_str::SmolStr;

/// One admission command with its durable identity.
#[derive(Clone, Debug)]
pub struct AdmitRequest {
    pub decision_id: DecisionId,
}

/// The result persisted in `Event::AdmissionDecided`.
#[derive(Clone, Debug)]
pub struct AdmissionResolution {
    pub decision: Admission,
    pub trace:    Vec<MiddlewareKey>,
}

/// One routing command with the core's proposals.
#[derive(Clone, Debug)]
pub struct RoutingRequest {
    pub decision_id:     DecisionId,
    pub restart_allowed: bool,
    pub groups:          Vec<RoutingProposal>,
}

/// The result persisted in `Event::RoutingResolved`.
#[derive(Clone, Debug)]
pub struct RoutingResolution {
    pub groups: Vec<GroupDecision>,
}

/// A decision pipeline failure. The driver converts it into a durable block.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct DecisionError {
    message: SmolStr,
}

impl DecisionError {
    pub fn new(message: impl Into<SmolStr>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// The host extension point for admission and routing middleware.
#[async_trait::async_trait]
pub trait DecisionResolver: Send + Sync {
    async fn admit(&self, request: AdmitRequest) -> Result<AdmissionResolution, DecisionError>;

    async fn route(&self, request: RoutingRequest) -> Result<RoutingResolution, DecisionError>;

    /// Resolve an admission synchronously when nothing needs to await, so the
    /// driver skips a task spawn and a loop round trip. `None` falls back to
    /// [`DecisionResolver::admit`].
    fn admit_now(&self, _request: &AdmitRequest) -> Option<AdmissionResolution> {
        None
    }

    /// The synchronous counterpart of [`DecisionResolver::route`].
    fn route_now(&self, _request: &RoutingRequest) -> Option<RoutingResolution> {
        None
    }
}

/// The ordinary Petri behavior with no user middleware.
#[derive(Default)]
pub struct DefaultDecisionResolver;

#[async_trait::async_trait]
impl DecisionResolver for DefaultDecisionResolver {
    async fn admit(&self, _request: AdmitRequest) -> Result<AdmissionResolution, DecisionError> {
        Ok(default_admission())
    }

    async fn route(&self, request: RoutingRequest) -> Result<RoutingResolution, DecisionError> {
        default_routing(&request)
    }

    fn admit_now(&self, _request: &AdmitRequest) -> Option<AdmissionResolution> {
        Some(default_admission())
    }

    fn route_now(&self, request: &RoutingRequest) -> Option<RoutingResolution> {
        default_routing(request).ok()
    }
}

pub(crate) fn default_admission() -> AdmissionResolution {
    AdmissionResolution {
        decision: Admission::Admit,
        trace:    Vec::new(),
    }
}

pub(crate) fn default_routing(
    request: &RoutingRequest,
) -> Result<RoutingResolution, DecisionError> {
    routing_with(request, &mut os_roll)
}

fn routing_with(
    request: &RoutingRequest,
    roll: &mut dyn FnMut() -> Result<u64, DecisionError>,
) -> Result<RoutingResolution, DecisionError> {
    let groups = request
        .groups
        .iter()
        .map(|proposal| group_decision_with(proposal, request.restart_allowed, roll))
        .collect::<Result<_, _>>()?;
    Ok(RoutingResolution { groups })
}

/// The default decision for one routing group: the engine's deterministic pick
/// (with a fresh draw for weighted tiers), downgraded by the restart limit.
///
/// This is the baseline a middleware pipeline composes over, group by group.
pub fn default_group_decision(
    proposal: &RoutingProposal,
    restart_allowed: bool,
) -> Result<GroupDecision, DecisionError> {
    group_decision_with(proposal, restart_allowed, &mut os_roll)
}

fn group_decision_with(
    proposal: &RoutingProposal,
    restart_allowed: bool,
    roll: &mut dyn FnMut() -> Result<u64, DecisionError>,
) -> Result<GroupDecision, DecisionError> {
    let draw = weighted_draw(proposal, roll)?;
    let picked = engine::deterministic_pick(proposal, draw.as_ref())
        .map_err(|reason| DecisionError::new(reason.as_str()))?;
    let decision = picked.map_or(RouteDecision::None, RouteDecision::Emit);
    let decision = engine::enforce_restart_limit(restart_allowed, proposal, decision);
    Ok(GroupDecision {
        group: proposal.group,
        draw,
        trace: Vec::new(),
        decision,
    })
}

/// A draw from the operating system.
fn os_roll() -> Result<u64, DecisionError> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|error| DecisionError::new(format!("random draw failed: {error}")))?;
    Ok(u64::from_le_bytes(bytes))
}

/// Roll the recorded draw a weighted tier needs; every other pick is
/// deterministic and needs none.
fn weighted_draw(
    proposal: &RoutingProposal,
    roll: &mut dyn FnMut() -> Result<u64, DecisionError>,
) -> Result<Option<WeightedDraw>, DecisionError> {
    if proposal.pick != Some(PickPolicy::WeightedRandom) || proposal.candidates.is_empty() {
        return Ok(None);
    }
    let total: u64 = proposal
        .candidates
        .iter()
        .map(|candidate| u64::from(candidate.weight))
        .sum();
    if total == 0 {
        return Err(DecisionError::new(
            "weighted routing has no positive candidate weight",
        ));
    }
    let roll = roll()?;
    Ok(Some(WeightedDraw {
        tier: proposal.tier.unwrap_or(0),
        candidates: proposal
            .candidates
            .iter()
            .map(|candidate| candidate.edge)
            .collect(),
        roll: roll % total,
        total,
    }))
}

/// [`DefaultDecisionResolver`] with its weighted draws rolled from a seed
/// instead of the operating system, so a simulated run routes the same way
/// every time. The draws are recorded either way, so replay never needs the
/// seed.
pub struct SeededDecisionResolver {
    state: Mutex<u64>,
}

impl SeededDecisionResolver {
    pub fn new(seed: u64) -> Self {
        Self {
            state: Mutex::new(seed),
        }
    }

    /// The next draw: `SplitMix64` over the seed.
    fn roll(&self) -> u64 {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn routing(&self, request: &RoutingRequest) -> Result<RoutingResolution, DecisionError> {
        routing_with(request, &mut || Ok(self.roll()))
    }
}

#[async_trait::async_trait]
impl DecisionResolver for SeededDecisionResolver {
    async fn admit(&self, _request: AdmitRequest) -> Result<AdmissionResolution, DecisionError> {
        Ok(default_admission())
    }

    async fn route(&self, request: RoutingRequest) -> Result<RoutingResolution, DecisionError> {
        self.routing(&request)
    }

    fn admit_now(&self, _request: &AdmitRequest) -> Option<AdmissionResolution> {
        Some(default_admission())
    }

    fn route_now(&self, request: &RoutingRequest) -> Option<RoutingResolution> {
        self.routing(request).ok()
    }
}
