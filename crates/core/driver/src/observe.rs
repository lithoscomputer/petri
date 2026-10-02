//! The host-facing record stream.
//!
//! An [`EventObserver`] is the seam every host consumer hangs off — a run
//! store's ingest, a progress UI, a checkpoint writer, a watchdog. The driver
//! calls it after every `apply` with each newly appended record and the
//! post-apply state, so a consumer resolves a firing to its node, name and
//! `meta` in place ([`EngineState::firing_node`]) instead of keeping a
//! projection of its own.
//!
//! This is deliberately a callback and not a broadcast channel: broadcast drops
//! on lag, and a store ingest must never lose a record. Losslessness downstream
//! is the observer's job — hand slow work to a channel and return fast.

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use engine::{EngineState, EventRecord};

/// The recording clock: milliseconds since the Unix epoch, read where a record
/// is appended to a durable log — the driver's append for an engine record, the
/// coordinator store's for a coordinator record — and never inside the state
/// machine. A clock before the epoch reads as zero.
pub fn recorded_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// Where the driver reads the `recorded_at` it hands its observers: the wall
/// clock ([`recorded_now`]) unless a host passes another. A simulation passes
/// a virtual one, so the stamps replay with the run.
#[derive(Clone)]
pub struct RecordingClock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl RecordingClock {
    /// A clock that reads milliseconds since the Unix epoch from `now`.
    pub fn new(now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    pub(crate) fn now(&self) -> u64 {
        (self.0)()
    }
}

impl Default for RecordingClock {
    fn default() -> Self {
        Self::new(recorded_now)
    }
}

impl fmt::Debug for RecordingClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecordingClock")
    }
}

/// A sink for a run's record stream, registered on the driver before `run()`.
#[async_trait::async_trait]
pub trait EventObserver: Send + Sync {
    /// Every appended record, External and Core, in seq order, exactly once per
    /// driver lifetime — at-least-once across a resume, deduped by
    /// `(log identity, seq)`: `seq` is unique only within one log, and the
    /// log's identity is host-named (a run dir, a store key), stable across
    /// resume, fresh per fork.
    ///
    /// `recorded_at` is when the driver appended the record
    /// ([`recorded_now`], read once per apply, so every record one apply
    /// appended carries the same time; a resume stamps the regenerated suffix
    /// with the resume's time, since the originals never reached a log). A
    /// store persists it beside the record so replay recovers the original
    /// time; the core never sees it.
    ///
    /// `state` is the post-apply engine state: resolve a firing to its node,
    /// name and `meta` here; borrow, don't keep. Called on the driver task —
    /// return fast, hand slow work to a channel. Infallible by design: the
    /// driver loop cannot meaningfully handle a sink error mid-apply, so an
    /// observer records its own failure and reports it from
    /// [`EventObserver::finish`].
    fn on_record(&self, record: &EventRecord, recorded_at: u64, state: &EngineState);

    /// Resolve once every record this observer has been handed through `seq`
    /// is in its durable storage — past the point where a process crash can
    /// lose it. This answers a step's acknowledged progress send
    /// (`steps::ProgressSender::send_acked`): the driver awaits it off its own
    /// task after the append, and the error reaches the step as
    /// `ProgressError::NotDurable`. An observer that stores nothing keeps the
    /// default and answers at once; a store answers only after its write, and
    /// reports a failed write here as well as from [`EventObserver::finish`].
    async fn durable(&self, seq: u64) -> Result<(), ObserveError> {
        let _ = seq;
        Ok(())
    }

    /// Awaited by `Driver::run` after the last record, before the report: drain
    /// queues, flush files, report what failed. Failures land in
    /// `ExecutionReport::observer_errors` and never change the run status — a
    /// host with fatal-sink semantics watches its own observer and cancels
    /// via `RunHandle`.
    async fn finish(&self) -> Result<(), ObserveError> {
        Ok(())
    }
}

/// What an observer failed to do, reported from [`EventObserver::finish`].
#[derive(Debug, thiserror::Error)]
#[error("{observer}: {message}")]
pub struct ObserveError {
    /// Which observer failed, in the observer's own words (`events.jsonl`,
    /// say).
    pub observer: String,
    pub message:  String,
    /// The underlying failure, when the observer has a typed one to keep.
    #[source]
    pub source:   Option<Box<dyn Error + Send + Sync>>,
}

impl ObserveError {
    pub fn new(observer: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            observer: observer.into(),
            message:  message.into(),
            source:   None,
        }
    }

    #[must_use]
    pub fn with_source(mut self, source: impl Into<Box<dyn Error + Send + Sync>>) -> Self {
        self.source = Some(source.into());
        self
    }
}
