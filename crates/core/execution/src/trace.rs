//! W3C trace context for a run's steps: one trace per run, one span per step
//! attempt, so a proxy that reads `traceparent` (a model gateway, an MCP
//! gateway) can place every request a step makes under that step.
//!
//! Both identities are derived, never minted: the trace ID is the first 16
//! bytes of SHA-256 over `petri.trace.v1`, a NUL and the run key, and the
//! span ID is the first 8 bytes of SHA-256 over `petri.span.v1`, a NUL, the
//! trace ID, and the invocation, execution, firing and attempt as big-endian
//! integers (`u64`, `u64`, `u64`, `u32`). So a resumed run keeps its trace
//! and a resumed attempt its span without a record of either, and replay
//! derives the same values the live run sent. Neither is the run key
//! itself: a host's run key is any string (Fabro's is a ULID, Petri's own is
//! not), and a trace ID that spelled it would collide with any other
//! producer that made the same choice. The trace is not secret: the run key
//! travels beside it in `baggage`.

use std::fmt::Write as _;

use ir::{Attempt, FiringId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use store::RunKey;

use crate::{ExecutionId, ExecutionIdentity, InvocationId};

/// One step attempt's place in its run's trace, as lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepTrace {
    /// The run's trace: 32 hex digits.
    pub trace_id: String,
    /// The attempt's span: 16 hex digits.
    pub span_id:  String,
}

impl StepTrace {
    /// The span of attempt `attempt` of firing `firing`, in the execution
    /// and invocation `identity` names, in the trace of its run.
    pub fn of(identity: &ExecutionIdentity, firing: FiringId, attempt: Attempt) -> Self {
        Self::derive(
            &identity.run,
            identity.invocation,
            identity.execution,
            firing,
            attempt,
        )
    }

    pub(crate) fn derive(
        run: &RunKey,
        invocation: InvocationId,
        execution: ExecutionId,
        firing: FiringId,
        attempt: Attempt,
    ) -> Self {
        let trace: [u8; 16] = nonzero(
            Sha256::new()
                .chain_update(b"petri.trace.v1\0")
                .chain_update(run.as_str().as_bytes())
                .finalize(),
        );
        let span: [u8; 8] = nonzero(
            Sha256::new()
                .chain_update(b"petri.span.v1\0")
                .chain_update(trace)
                .chain_update(invocation.raw().to_be_bytes())
                .chain_update(execution.raw().to_be_bytes())
                .chain_update(firing.raw().to_be_bytes())
                .chain_update(attempt.raw().to_be_bytes())
                .finalize(),
        );
        Self {
            trace_id: hex(&trace),
            span_id:  hex(&span),
        }
    }

    /// The `traceparent` header naming this span as the parent: version
    /// `00`, the sampled flag set.
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-01", self.trace_id, self.span_id)
    }

    /// A fresh span beneath this one, for work the step has done outside a
    /// Pebble session of its own (a hook that calls a model). Its ID is
    /// random, not derived: one attempt may run the same hook many times
    /// (a tool hook per tool call), so the span is recorded where the work
    /// is reported rather than recomputed.
    pub fn child(&self) -> ChildSpan {
        let mut span = [0; 8];
        // A failed OS random source leaves zeros, which `nonzero` turns into
        // a valid ID; the record still names the span it sent.
        let _ = getrandom::fill(&mut span);
        ChildSpan {
            trace_id:       self.trace_id.clone(),
            span_id:        hex(&nonzero::<8>(span)),
            parent_span_id: self.span_id.clone(),
        }
    }
}

/// A span beneath a step attempt's, as lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSpan {
    /// The run's trace.
    pub trace_id:       String,
    /// This span.
    pub span_id:        String,
    /// The step attempt's span ([`StepTrace::span_id`]).
    pub parent_span_id: String,
}

impl ChildSpan {
    /// The `traceparent` header naming this span as the parent.
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-01", self.trace_id, self.span_id)
    }
}

/// The leading `N` bytes of a digest; W3C forbids an all-zero ID, so that
/// one (a 2^-64 chance at worst) ends in `01` instead.
fn nonzero<const N: usize>(digest: impl AsRef<[u8]>) -> [u8; N] {
    let mut id = [0; N];
    id.copy_from_slice(&digest.as_ref()[..N]);
    if id.iter().all(|byte| *byte == 0) {
        id[N - 1] = 1;
    }
    id
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(run: &str, firing: u64, attempt: u32) -> StepTrace {
        StepTrace::derive(
            &RunKey::new(run),
            InvocationId::new(0),
            ExecutionId::new(1),
            FiringId::new(firing),
            Attempt::new(attempt),
        )
    }

    /// The derivation is a stored run's contract: a run resumed by a later
    /// build must keep its trace, so the values are pinned. Expected values
    /// computed with Python's `hashlib` from the scheme in the module docs.
    #[test]
    fn the_derivation_is_pinned() {
        let trace = step("01JDQ7ZK3V8Y2M4N6P8R0T2W4X", 3, 2);
        assert_eq!(
            trace.traceparent(),
            "00-6a83a73da1129431875b9c1d1105e0a3-1e5230205362b186-01"
        );
    }

    #[test]
    fn a_child_span_is_a_fresh_span_beneath_its_step() {
        let step = step("run", 3, 1);
        let (first, second) = (step.child(), step.child());
        assert_eq!(first.trace_id, step.trace_id);
        assert_eq!(first.parent_span_id, step.span_id);
        assert_ne!(first.span_id, step.span_id);
        assert_ne!(first.span_id, second.span_id);
        assert_eq!(
            first.traceparent(),
            format!("00-{}-{}-01", step.trace_id, first.span_id)
        );
    }

    #[test]
    fn a_run_has_one_trace_and_each_attempt_its_own_span() {
        let first = step("run", 3, 1);
        let retry = step("run", 3, 2);
        let other = step("other", 3, 1);
        assert_eq!(first.trace_id, retry.trace_id);
        assert_ne!(first.span_id, retry.span_id);
        assert_ne!(first.trace_id, other.trace_id);
    }
}
