//! Failures from materializing or using an environment.

use std::{fmt, io};

use smol_str::SmolStr;

#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    /// A filesystem operation on or beside the workspace failed. `action` is
    /// the verb ("create", "read", "write", ...), `path` where.
    #[error("could not {action} `{path}`")]
    Workspace {
        action: &'static str,
        path:   String,
        #[source]
        source: io::Error,
    },
    #[error("could not spawn `{program}`")]
    Spawn {
        program: SmolStr,
        #[source]
        source:  io::Error,
    },
    #[error("could not signal process group {pgid} with {signal}")]
    Signal {
        pgid:   i32,
        signal: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("waiting on the process failed")]
    Wait(#[source] io::Error),
    /// The executor's backing system refused or failed: a container engine, a
    /// cloud API, a remote agent. `backend` names it; the interface does
    /// not know the list.
    #[error("{backend} {operation} failed: {message}")]
    Backend {
        backend:   SmolStr,
        operation: SmolStr,
        message:   String,
    },
    /// `acquire`'s fence found prior work still alive that it could not safely
    /// end — the executor's kill mechanism is gone or never engaged, and
    /// nothing is ever signalled on a bare recorded id (it can be recycled
    /// to an innocent; the no-innocent-signal invariant is absolute). The
    /// scope's firings fail routably through the ordinary acquire-failure
    /// path; cleanup belongs to the operator or host policy.
    #[error("prior work survived the fence: {detail}")]
    FenceLeaked { detail: String },
    #[error("the environment is gone")]
    Gone,
    #[error("this sandbox cannot reach services on Petri's machine")]
    HostUnreachable,
    /// The run's own store failed a write the environment needed, such as a
    /// sandbox lease record. It is not the environment's failure: the driver
    /// stops without recording one, and the host resumes the run from what
    /// the store holds.
    #[error("the run's store failed: {message}")]
    Store { message: String },
}

impl EnvError {
    /// The failure class recorded when acquiring an environment fails, so a bad
    /// image or a down daemon routes like any other failure.
    pub const ACQUIRE_CLASS: ir::FailureClass = ir::FailureClass::new_static("env_acquire");
    pub const HOST_UNREACHABLE_CLASS: ir::FailureClass =
        ir::FailureClass::new_static("host_unreachable");

    /// A fixed discriminant, for diagnostics that may not carry the error
    /// itself. A `Backend` message is the backing system's own stderr — a
    /// container engine's, a service container's log tail — which no masker has
    /// ever seen, so a caller that cannot vouch for the variant reports this
    /// instead, beside the structural `backend` and `operation`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Workspace { .. } => "workspace",
            Self::Spawn { .. } => "spawn",
            Self::Signal { .. } => "signal",
            Self::Wait(_) => "wait",
            Self::Backend { .. } => "backend",
            Self::FenceLeaked { .. } => "fence_leaked",
            Self::Gone => "gone",
            Self::HostUnreachable => "host_unreachable",
            Self::Store { .. } => "store",
        }
    }

    /// A failed filesystem operation on the workspace side, in one call:
    /// `action` is the verb, `path` where, `source` the failure itself.
    pub fn workspace(action: &'static str, path: impl fmt::Display, source: io::Error) -> Self {
        Self::Workspace {
            action,
            path: path.to_string(),
            source,
        }
    }

    /// A refusal or failure of the backing system, in one call: `backend`
    /// names it, `operation` what was asked of it, `message` its own account.
    pub fn backend(backend: &str, operation: &str, message: impl Into<String>) -> Self {
        Self::Backend {
            backend:   SmolStr::new(backend),
            operation: SmolStr::new(operation),
            message:   message.into(),
        }
    }
}

/// What tearing an environment down actually managed to do.
///
/// Release is best effort and never fails a run: a leaked resource is a problem
/// to report, not a reason to lose the run's result. The resources are
/// described, not enumerated — a workspace, a container, an instance — so the
/// report is the same shape for every executor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReleaseReport {
    /// Resources that were torn down, described (`workspace /run/x`, `container
    /// c1`).
    pub released: Vec<String>,
    /// Resources deliberately left in place, described, with why implied by
    /// policy.
    pub kept:     Vec<String>,
    pub problems: Vec<String>,
}

impl ReleaseReport {
    /// Include the teardown results of another resource owner.
    pub fn merge(&mut self, other: Self) {
        self.released.extend(other.released);
        self.kept.extend(other.kept);
        self.problems.extend(other.problems);
    }

    #[must_use]
    pub fn released(mut self, what: impl Into<String>) -> Self {
        self.released.push(what.into());
        self
    }

    #[must_use]
    pub fn kept(mut self, what: impl Into<String>) -> Self {
        self.kept.push(what.into());
        self
    }

    #[must_use]
    pub fn problem(mut self, message: impl Into<String>) -> Self {
        self.problems.push(message.into());
        self
    }

    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    /// Whether anything matching `needle` was released — `"container"`, say.
    pub fn released_any(&self, needle: &str) -> bool {
        self.released.iter().any(|r| r.contains(needle))
    }

    pub fn kept_any(&self, needle: &str) -> bool {
        self.kept.iter().any(|r| r.contains(needle))
    }
}
