//! The IO loop between the pure core and real processes.
//!
//! The driver owns all IO scheduling and makes no policy decisions: it
//! translates [`Command`](engine::Command)s into effects and effects back into
//! [`Event`](engine::Event)s. Every event it produces is
//! [`EventOrigin::External`](engine::EventOrigin::External), and every one of
//! them passes through a single channel, so **arrival order is the total
//! order** and the log makes it canonical. That is what keeps replay
//! byte-identical over real processes whose completion order is a wall-clock
//! accident.

mod decision;
mod jitter;
pub mod lifecycle;
mod observe;
mod run;
mod sink;
mod view;

pub use decision::{
    AdmissionResolution, AdmitRequest, DecisionError, DecisionResolver, DecisionRolls,
    DefaultDecisionResolver, RoutingRequest, RoutingResolution, SeededDecisionResolver,
    default_group_decision, default_group_decision_rolled, default_routing_with,
};
pub use lifecycle::{ExecutionHooks, HookContext, ParentLink};
pub use observe::{EventObserver, ObserveError, RecordingClock, recorded_now};
pub use run::{
    CANCEL_FORCED, CANCELLED_BEFORE_RESUME, CONTROL_CHANNEL_CAPACITY, DEFAULT_CLEANUP_GRACE,
    DeliverDisposition, Driver, ExecutionReport, ExecutionSlot, KILLED_BEFORE_RESUME, ResumeError,
    ResumeInfo, RunConfig, RunGuard, RunHandle, SandboxAssignment, ScopeLease, ScopeLeaseAllocator,
    ScopeLeases,
};
pub use sink::{StepLogDir, StepLogStore};
pub use view::{BRANCH_ROLE_META, BranchMap, BranchRef, BranchRole, FiringView};
