//! The event log. Versioned so replay and resume can rely on the format.

use serde::{Deserialize, Serialize};

use crate::event::Event;

/// Bumped whenever the shape of a record changes.
///
/// v1 → v2: the firing key gained [`ir::Attempt`], `StepStarted` /
/// `StepFinished` carry it, `ScheduleRetry` / `RetryElapsed` joined the
/// vocabulary, finish records carry `context_updates`, and every record records
/// whether it came from outside or from the core.
///
/// v2 → v3: cancelled outcomes route (the semantics change under replay),
/// `KillRequested` joined the vocabulary, and `Node` — serialized inside
/// `NodeExpanded` splices — gained `run_on_cancel`. Per the standing policy
/// there is no migrator: a v2 log is rejected cleanly.
///
/// v3 → v4: `Node` — serialized inside `NodeExpanded` splices — gained `meta`.
/// Standing policy again: no migrator, a v3 log is rejected cleanly.
///
/// v4 → v5: `ControlRequested` and `Control::Deliver` joined the vocabulary, so
/// a pending host-delivered interaction is in the log and replay reproduces it.
/// Standing policy, no migrator.
///
/// v5 → v6: `Outcome` — serialized inside `StepFinished` — gained `splices`,
/// and `Node` — serialized inside `NodeExpanded` splices — gained
/// `splice_policy`, for the outcome-driven splice. Standing policy, no
/// migrator: a v5 log is rejected cleanly.
///
/// v6 → v7: execution starts, admission, and resolved routing became durable
/// command/event round trips; applied routes and execution exits joined the
/// vocabulary. Standing policy, no migrator.
///
/// v7 → v8: nodes can declare independent cancellation groups, and
/// `CancelGroupRequested` records targeted cancellation. No migrator.
///
/// v8 → v9: every persisted record carries `recorded_at`, the wall-clock time
/// (milliseconds since the Unix epoch) at which the driver appended it. The
/// driver reads that clock once per apply, outside the core, and the host's
/// framing keeps it beside the record; [`EventRecord`] itself is unchanged and
/// the core still reads no clock, so replay stays byte-identical. A v8 log has
/// no times to recover and the standing policy holds: no migrator, a v8 log is
/// rejected cleanly.
///
/// v9 → v10: one vocabulary for records and events. A record is an object
/// tagged by `event` with a `<subject>.<verb>` name (`step.finished`) and
/// its fields beside the tag; `Admitted` became `AdmissionDecided`,
/// `StepProgress` became `StepProgressRecorded`, the two cancel records
/// merged into `CancelRequested { target }`, every enum inside a record
/// takes snake-case tags, and the persisted line is
/// `{"seq", "origin", "recorded_at", "body"}` (`source` became `origin`).
/// Standing policy, no migrator: a v9 log is rejected cleanly.
///
/// v10 → v11: `ScopeAcquired` and `ScopeFailed` joined the vocabulary: the
/// driver records where each scope's environment runs (the provider, the
/// sandbox, the workspace, the lease, the acquisition time) or why it could
/// not be acquired. The core applies neither. Standing policy, no migrator:
/// a v10 log is rejected cleanly.
///
/// v11 → v12: a partial success keeps the failure it was converted from
/// whole: `underlying` holds an `UnderlyingFailure` (a failure with its info,
/// or a timeout) instead of a bare `FailureInfo`, so an exhausted timeout
/// accepted as partial no longer loses the timeout (§3.1 rule 3). Standing
/// policy, no migrator: a v11 log is rejected cleanly.
pub const LOG_VERSION: u32 = 12;

/// Where an event came from.
///
/// **This enum is closed.** `External` and `Core` are the complete and
/// permanent vocabulary: an event either entered from outside the core or the
/// core produced it, and there is no third case. Nothing may extend it.
///
/// # The verification contract
///
/// This is load-bearing for every determinism claim the system makes, so it is
/// worth stating plainly:
///
/// - Replay feeds back **only** `External` records.
/// - Every `Core` record is **regenerated** by the core during replay, never
///   replayed from the log.
/// - A replayed log that is byte-identical to the original therefore asserts
///   that the core reached every one of those `Core` events again, in the same
///   order, from the same inputs.
///
/// The regenerated-versus-recorded distinction *is* the assertion. It is not
/// redundancy, and it is not an optimisation. If a future change feeds `Core`
/// records back instead of regenerating them, `verify_replay` keeps passing
/// while asserting nothing at all: it would be comparing the log against a copy
/// of itself. Anything that makes the core consult a clock, an RNG, an
/// environment variable, or a non-deterministic iteration order breaks
/// byte-identity — which is the point.
///
/// A host feeding an event in is `External` even when that event describes
/// something the core asked for, such as `RetryElapsed` answering a
/// `ScheduleRetry`: the decision to send it, and when, came from outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventOrigin {
    /// Fed in by the host: run start, step results, retry timers, cancellation.
    External,
    /// Emitted by the core while draining: routed tokens, splices, cascades.
    Core,
}

/// Where a cancellation's escalation is recorded, and why it is not a failure
/// class.
///
/// A consumer reading the log for "how did this step get stopped" looks here,
/// not at [`ir::Status`]. `Status::Cancelled` and `Status::TimedOut` carry no
/// `FailureInfo`, and giving them one would widen an enum the core declares
/// closed and permanent. So the escalation is a field on the finish record's
/// `outcome.output`:
///
/// | Value | Meaning |
/// |---|---|
/// | `sigterm` | the step exited within the grace period after `SIGTERM` |
/// | `sigkill` | the grace period ran out and the group was killed |
/// | `cancel_forced` | the step kind never returned; the driver stopped waiting |
/// | `cancelled_before_resume` | the polite tier had marked the firing cancelling when the driver died; resume finished it without re-spawning |
/// | `killed_before_resume` | the kill tier had; same direct finish, recorded without routing |
///
/// The key is absent on any outcome that was not cancelled or timed out.
pub const CANCEL_ESCALATION_KEY: &str = "cancel_escalation";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Position in the log, starting at 0.
    pub seq:    u64,
    pub origin: EventOrigin,
    pub event:  Event,
}

/// A log whose version is not the one this build speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("event log is version {found}; this build reads version {expected}")]
pub struct UnsupportedLogVersion {
    pub found:    u32,
    pub expected: u32,
}

/// A record list that cannot become a log.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRecords {
    #[error(transparent)]
    Version(#[from] UnsupportedLogVersion),
    #[error("record at index {index} has seq {found}; a log's records are contiguous from 0")]
    SeqMismatch { index: usize, found: u64 },
}

/// An append-only list of every event the run has applied, in order.
///
/// The core appends here before applying, including for the events it emits
/// itself while routing. Replaying the external records through `apply` from a
/// fresh state reproduces the run exactly, because `apply` has no other inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(into = "EventLogRepr", try_from = "EventLogRepr")]
pub struct EventLog {
    version: u32,
    records: Vec<EventRecord>,
}

impl Default for EventLog {
    fn default() -> Self {
        Self {
            version: LOG_VERSION,
            records: Vec::new(),
        }
    }
}

impl EventLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// The format version. Always [`LOG_VERSION`] for a log this build
    /// produced.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Rebuild a log from records a host persisted in its own store.
    ///
    /// This constructor, plus the serde round-trip of [`EventLog`] itself, is
    /// the whole of the core's persistence surface: a host frames and
    /// stores records however it likes — a jsonl file, a database, an
    /// object store — and hands them back here. The version is checked
    /// exactly as deserialization checks it (standing no-migrator policy),
    /// and the records must be contiguous from seq 0.
    pub fn try_from_records(
        version: u32,
        records: Vec<EventRecord>,
    ) -> Result<Self, InvalidRecords> {
        check_version(version)?;
        for (index, record) in records.iter().enumerate() {
            if record.seq != index as u64 {
                return Err(InvalidRecords::SeqMismatch {
                    index,
                    found: record.seq,
                });
            }
        }
        Ok(Self { version, records })
    }

    pub(crate) fn append(&mut self, origin: EventOrigin, event: Event) -> u64 {
        let seq = self.records.len() as u64;
        self.records.push(EventRecord { seq, origin, event });
        seq
    }

    pub fn records(&self) -> &[EventRecord] {
        &self.records
    }

    pub fn events(&self) -> impl Iterator<Item = &Event> {
        self.records.iter().map(|r| &r.event)
    }

    /// The events a host fed in, in order. This is what replay consumes.
    pub fn external_events(&self) -> impl Iterator<Item = &Event> {
        self.records
            .iter()
            .filter(|r| r.origin == EventOrigin::External)
            .map(|r| &r.event)
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The first `len` records, as a log of their own.
    ///
    /// The rewind/fork helper: resuming a truncated log is rewind, doing it in
    /// a fresh run dir is fork — replay regenerates everything past the
    /// prefix. One method, no policy.
    #[must_use]
    pub fn prefix(&self, len: usize) -> Self {
        Self {
            version: self.version,
            records: self.records[..len.min(self.records.len())].to_vec(),
        }
    }
}

/// The wire shape. Reading one goes through the version check, so a v1 log is
/// rejected cleanly rather than half-understood.
#[derive(Serialize, Deserialize)]
struct EventLogRepr {
    version: u32,
    records: Vec<EventRecord>,
}

impl From<EventLog> for EventLogRepr {
    fn from(log: EventLog) -> Self {
        Self {
            version: log.version,
            records: log.records,
        }
    }
}

impl TryFrom<EventLogRepr> for EventLog {
    type Error = UnsupportedLogVersion;

    fn try_from(repr: EventLogRepr) -> Result<Self, Self::Error> {
        check_version(repr.version)?;
        Ok(Self {
            version: repr.version,
            records: repr.records,
        })
    }
}

/// The standing no-migrator policy: only this build's version reads.
fn check_version(found: u32) -> Result<(), UnsupportedLogVersion> {
    if found != LOG_VERSION {
        return Err(UnsupportedLogVersion {
            found,
            expected: LOG_VERSION,
        });
    }
    Ok(())
}
