use std::collections::BTreeMap;

use engine::{EngineExit, EngineStart, EventOrigin, MiddlewareKey};
use executor::ScopeOutcome;
use ir::{FailureInfo, RunStatus, ScopeId, Value, WorkspaceId};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use store::RunKey;

use crate::host::ForkOrigin;
use crate::{ExecutionId, GraphDigest, InvocationId, ParentCallKey, SandboxLeaseId};

/// Version 2 records stable dynamic scope identities and their runtime and
/// execution provenance in the resource ledger. Version 3 stamps every record
/// with `recorded_at`, the wall-clock time the store appended it, so replay
/// recovers the original run, invocation and execution times; a version 2 run
/// has none and is refused, never migrated. Version 4 spells every record as
/// `{"seq", "origin", "recorded_at", "body"}`, with `body` tagged by `event`
/// under a `<subject>.<verb>` name (`execution.declared`), `RunNote` renamed
/// `RunNoteRecorded`, and snake-case tags on every enum inside a record; a
/// version 3 run is refused, never migrated. Version 5 stores the run through
/// the store seam: the run declaration carries the run's `key`, the engine
/// log version is pinned by this version and no engine log carries a header
/// line, and sandbox resources are one append-only log; a version 4 run is
/// refused, never migrated. Version 6 pins engine log version 11
/// (`scope.acquired`, `scope.failed`), records `scope.released` when a
/// lease's sandbox is released by retention, and spells the scope identity
/// inside a resource record with snake-case tags; a version 5 run is
/// refused, never migrated. Version 7 lets the run declaration carry
/// `forked_from`, the source and position a forked run was seeded from
/// (`FORK.md`); a version 6 run is refused, never migrated. Version 8 pins
/// engine log version 12 (a partial success keeps its underlying failure
/// whole, a timeout included); a version 7 run is refused, never migrated.
pub const COORDINATOR_FORMAT_VERSION: u32 = 8;

/// Name-only child secret bindings. Plaintext is not representable here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretBindings {
    #[default]
    None,
    Inherit,
    Explicit(BTreeMap<SmolStr, SecretBinding>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretBinding {
    Parent(SmolStr),
    Empty,
}

/// What a caller requests before declaration resolves the binding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Share the caller's sandbox. The caller names its own scope — the step
    /// knows where it runs — so the coordinator resolves the binding without
    /// reading the parent's engine log.
    Inherit { scope: ScopeId },
    #[default]
    Isolated,
}

/// The durable sandbox binding on an invocation declaration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBinding {
    Inherited {
        lease: SandboxLeaseId,
    },
    #[default]
    Isolated,
}

/// Bounded concurrency for a fork's child invocations: the invocations one
/// parent execution declares under the gate `gate` share `max_parallel`
/// slots. A child's engine starts only on a free slot, in declaration order,
/// keeps the slot while its attempts run and between them, and releases it
/// when the engine ends or when a retry backoff begins. So at most
/// `max_parallel` children are live at once, plus any waiting out a backoff.
/// The gate is shared by every invocation the same parent execution declares
/// under the same name, so a fork's branches share one limit while a repeated
/// fork visit or a nested fork gets its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptAdmission {
    pub gate:         SmolStr,
    pub max_parallel: u32,
}

/// Why a cancel was requested, when the requester said so.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CancelReason {
    /// The stall watchdog: no execution activity for the run's budget.
    StallTimeout {
        stall_timeout_ms: u64,
        idle_ms:          u64,
    },
    /// An interrupt from the terminal (Ctrl-C).
    Interrupt,
    /// A run control: the control service, a control file, an embedding
    /// host.
    Control,
}

/// One cancel request on the coordinator's channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRequest {
    pub invocation: InvocationId,
    pub reason:     Option<CancelReason>,
    /// Whether the request reaches the driver of an invocation that is
    /// already cancelled, which escalates that driver to its kill tier. A run
    /// control's repeated cancel escalates. A parent forwarding the polite
    /// cancel it received to a child does not: the coordinator's own cascade
    /// has already cancelled every descendant of a cancelled invocation, and
    /// a second delivery would kill the child.
    pub escalate:   bool,
}

/// The one durable result returned by an invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InvocationResult {
    pub status:          RunStatus,
    pub failure:         Option<FailureInfo>,
    pub final_execution: ExecutionId,
    pub output:          Value,
    /// The final execution's whole run context.
    pub context:         BTreeMap<SmolStr, Value>,
    /// The `context_updates` the result node's final outcome reported: what
    /// that node wrote itself, unchanged values included, as distinct from
    /// the diff a caller can take between `context` and the context it
    /// passed in. Empty when the graph projects no result node. Additive to
    /// coordinator format version 2: a record without it reads as empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub updates:         BTreeMap<SmolStr, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationStatus {
    Declared,
    Running { execution: ExecutionId },
    Finished(InvocationResult),
}

/// Relationships and lifecycle facts that span engine logs.
///
/// On the wire an event is an object tagged by `event`, named
/// `<subject>.<verb>` after the variant, with the variant's fields beside the
/// tag, as the engine's records are. The public event stream carries the
/// same object, unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum CoordinatorEvent {
    #[serde(rename = "run.started")]
    RunStarted {
        format_version:   u32,
        /// The run's identity in its store and on its sandbox providers.
        key:              RunKey,
        root:             InvocationId,
        middleware_chain: Vec<MiddlewareKey>,
        /// Where a forked run was seeded from: the source run and the
        /// position its records were kept up to. Absent on a run that
        /// started fresh. See `FORK.md`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forked_from:      Option<ForkOrigin>,
    },
    #[serde(rename = "graph.registered")]
    GraphRegistered { digest: GraphDigest },
    #[serde(rename = "invocation.declared")]
    InvocationDeclared {
        invocation:      InvocationId,
        call:            Option<ParentCallKey>,
        graph:           GraphDigest,
        context:         BTreeMap<SmolStr, Value>,
        secret_bindings: SecretBindings,
        sandbox:         SandboxBinding,
        /// Bounded attempt concurrency, when the caller asked for it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        admission:       Option<AttemptAdmission>,
    },
    #[serde(rename = "execution.declared")]
    ExecutionDeclared {
        execution:        ExecutionId,
        invocation:       InvocationId,
        predecessor:      Option<ExecutionId>,
        start:            EngineStart,
        middleware_state: BTreeMap<MiddlewareKey, (u32, Value)>,
    },
    #[serde(rename = "execution.finished")]
    ExecutionFinished {
        execution: ExecutionId,
        exit:      EngineExit,
    },
    #[serde(rename = "invocation.finished")]
    InvocationFinished {
        invocation: InvocationId,
        result:     InvocationResult,
    },
    #[serde(rename = "invocation.cancel.requested")]
    InvocationCancelRequested {
        invocation: InvocationId,
        /// Why, when the requester said. Absent for a plain cancel.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason:     Option<CancelReason>,
    },
    /// A run control held every attempt not yet admitted. Additive since
    /// format version 2: a log without it replays as before, and a resume
    /// starts paused when this is the last control recorded.
    #[serde(rename = "run.paused")]
    RunPaused,
    /// A run control released held and future attempts. Additive since
    /// format version 2.
    #[serde(rename = "run.unpaused")]
    RunUnpaused,
    /// A note from a run-level hook point (`run_finished`, `scope_released`):
    /// no firing owns it, so it lives beside the run, appended from the
    /// execution's report before `RunFinished`. Additive since format
    /// version 2: a log without it replays as before, and `payload` reads
    /// as `null` when absent.
    #[serde(rename = "run.note.recorded")]
    RunNoteRecorded {
        /// The execution whose driver ran the point, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution: Option<ExecutionId>,
        kind:      SmolStr,
        #[serde(default)]
        payload:   Value,
    },
    /// A lease's sandbox was released once the invocation that owns it
    /// finished: stopped and kept, or deleted, as the run's retention decides
    /// for `outcome`. `retained` is whether the sandbox and its workspace
    /// still exist on the provider afterwards; `problems` is what the release
    /// could not do, in which case the sandbox is still there and the next
    /// release (`finish`, or `petri sandbox prune`) tries again. Usually
    /// before `run.finished`; a lease a crash left live is released by the
    /// resumed run's end, after it.
    #[serde(rename = "scope.released")]
    ScopeReleased {
        invocation: InvocationId,
        lease:      SandboxLeaseId,
        /// The stable identity of the scope the lease was reserved for.
        scope:      engine::ScopeIdentity,
        workspace:  WorkspaceId,
        provider:   SmolStr,
        /// The provider's id for the sandbox, when one was created.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance:   Option<SmolStr>,
        outcome:    ScopeOutcome,
        retained:   bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        problems:   Vec<String>,
    },
    #[serde(rename = "run.finished")]
    RunFinished { status: RunStatus },
}

/// One `coordinator.jsonl` line, `{"seq", "origin", "recorded_at", "body"}`:
/// the same shape as an engine log's line. A coordinator record is always
/// external: the host appends every one. The public event stream carries
/// this same line, unchanged, as a record's `record`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorRecord {
    pub seq:         u64,
    pub origin:      EventOrigin,
    /// Milliseconds since the Unix epoch when the store appended the record:
    /// the recording time, read at the append and persisted with it, so a
    /// replay recovers the time the event happened, not the time it was read.
    pub recorded_at: u64,
    pub body:        CoordinatorEvent,
}

impl CoordinatorRecord {
    /// A record the host appended now.
    pub fn external(seq: u64, recorded_at: u64, body: CoordinatorEvent) -> Self {
        Self {
            seq,
            origin: EventOrigin::External,
            recorded_at,
            body,
        }
    }
}
