//! The engine IR: a directed graph (cycles permitted) executed with token-flow
//! semantics.
//!
//! A node fires when its [`JoinPolicy`] is satisfied by incoming tokens. On
//! completion its [`Routing`] emits tokens on outgoing edges. Routing is an
//! **AND of XORs**: each [`SelectGroup`] emits at most one token, and groups
//! emit concurrently. The default is selection — one completing node routes to
//! exactly one successor — so fan-out is never implicit. It takes writing more
//! than one group, which keeps single-path workflows analyzable and makes
//! parallelism visible.
//!
//! This crate is data and pure functions only. Coordination lives in the
//! `engine` crate; side effects live behind traits in the host.

mod builder;
pub mod container;
mod desugar;
mod digest;
pub mod expr;
mod flow;
mod graph;
mod ids;
pub mod placeholder;
mod splice;
mod step;
pub mod validate;

pub use builder::{Arm, GraphBuilder};
pub use container::{
    ContainerOptions, HealthCheck, OptionError, ServiceOptions, parse_container_options,
    parse_service_options,
};
pub use desugar::{
    CollectorExprs, LoopExprs, SequentialForEach, collector_exprs, parallel_for_each,
    sequential_for_each, sequential_for_each_over,
};
pub use digest::{digest_hex, encode_graph, graph_digest, graph_digest_bytes};
pub use expr::{
    BinOp, EvalEnv, EvalError, Expr, ExprTable, StaticCtx, UnOp, eval, eval_bool, is_truthy,
};
pub use flow::{
    Control, FailureClass, FailureInfo, LogStream, Metrics, NodeRecord, Outcome, RunContext,
    RunStatus, SandboxInstance, Status, StatusKind, StepEvent, Token, UnderlyingFailure,
};
pub use graph::{
    Backoff, Budget, Candidate, Completion, Edge, EdgeTransition, Exhaustion, ExpandTarget,
    Expansion, ExprOrValue, Fallthrough, Graph, GraphBody, Guard, JoinPolicy, Node, PickPolicy,
    RegistryCredentials, ResultProjection, RetryOn, RetryPolicy, Routing, RoutingGroup, RunPolicy,
    RuntimeSpec, RuntimeTarget, Scope, SelectGroup, SelectionPolicy, ServiceSpec, StepRef, Tier,
    TimeoutPolicy, WorkspacePolicy,
};
pub use ids::{
    Attempt, CancelScopeId, EdgeId, EnvironmentId, ExecutionId, ExprId, FiringId, Generation,
    InvocationId, Live, Local, NodeId, SandboxLeaseId, ScopeId, StepKindId, WorkspaceId,
};
pub use splice::{
    Attachment, ExistingNodeRef, FragmentErrorKind, FragmentValidationError, GraphFragment,
    ReplaceScope, SpliceContext, SpliceMode, SplicePolicy, SpliceRequest, validate_fragment,
    validate_request,
};
pub use step::{Digest, StepFailure, StepKind, StepKinds};
pub use validate::{
    ValidationError, ValidationLocation, ValidationReport, ValidationWarning, check, validate,
    validate_step_kinds, validate_with,
};

/// The value type carried by tokens, outcomes and step configuration.
pub type Value = serde_json::Value;
