//! Invocation trees and reset-free execution successions.

pub mod breaker;
mod client;
pub mod controls;
mod coordinator;
mod event;
pub mod events;
mod fork;
pub mod hooks;
pub mod host;
mod id;
pub mod inspect;
pub mod interview;
mod middleware;
mod observe;
pub mod prune;
mod resource;
mod secret;
mod state;
mod store;
pub mod watchdog;

/// The run store seam and the run-directory layout, re-exported for hosts.
pub use ::store::{
    Access, COORDINATOR_FILE, EVENTS_FILE, EXECUTIONS_DIR, GRAPHS_DIR, LogId, MemoryRunStore,
    OwnerId, RESOURCES_FILE, RUN_FILE, Record, RunDirStore, RunKey, RunLogs, RunStore,
    execution_relative_dir,
};
pub use client::{
    ChildStart, CoordinatorInvocationClient, InvocationClient, InvocationHandle, InvocationRequest,
    InvokeError,
};
pub use coordinator::{
    Coordinator, CoordinatorError, CoordinatorHandle, CoordinatorOptions, DEFAULT_MAX_INVOCATIONS,
    InvocationLimitError, MAX_INVOCATIONS,
};
pub use event::{
    AttemptAdmission, COORDINATOR_FORMAT_VERSION, CancelReason, CancelRequest, CoordinatorEvent,
    CoordinatorRecord, InvocationResult, InvocationStatus, SandboxBinding, SandboxMode,
    SecretBinding, SecretBindings,
};
pub use id::{
    CallSite, ExecutionId, ExecutionIdentity, GraphDigest, InvocationId, ParentCallKey,
    SandboxAllocationKey, SandboxLeaseId,
};
pub use interview::{
    Delivery, InterviewDispatcher, InterviewError, InterviewReceipt, InterviewRecord,
    InterviewReply, InterviewRequest, Interviewer, RECEIPT_FILE, RECEIPT_VERSION, ReplyRecord,
};
pub use middleware::{
    AdmitCall, AdmitNext, DecisionAddress, FoldEvent, Middleware, MiddlewareError,
    MiddlewareFoldObserver, MiddlewarePipeline, MiddlewareState, RouteCall, RouteNext,
    initial_middleware_state, validate_middleware_state,
};
pub use observe::{
    AddressedObserver, DecodedEngineFile, DecodedEngineLog, EngineLogDecodeError, EngineLogError,
    ExecutionLogWriter, ExecutionObserver, StoreWriter, StoredEngineRecord, decode_engine_log,
    decode_engine_records, encode_engine_log, encode_engine_record, read_engine_log,
    read_execution_log,
};
pub use resource::{
    HOST_PROVIDER, LeaseState, PendingIntent, ResourceError, ResourceLedger, ResourceLogRecord,
    ResourceStore, SandboxResourceRecord, ScopeAdmission,
};
pub use secret::InvocationSecrets;
pub use state::{
    CoordinatorState, ExecutionDeclaration, ExecutionState, InvocationDeclaration, InvocationState,
    RunNote, StateError,
};
pub use store::{
    CoordinatorStore, StoreError, decode_coordinator_records, encode_record, open_run_dir,
    read_coordinator_log,
};
