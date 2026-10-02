//! Questions from a run, answers from a host: the interview boundary.
//!
//! A step that needs a person emits a [`Question`] on its progress channel
//! and waits on its control channel. Which person, and how they are reached,
//! is the host's business: a terminal prompt, a scripted fixture, a product
//! inbox. The host supplies an [`Interviewer`]; the [`InterviewDispatcher`]
//! owns everything around it that is the same for every host.
//!
//! The dispatcher subscribes to the coordinator as an [`ExecutionObserver`],
//! correlates each question with its invocation, execution, firing, node
//! instance and occurrence, hands the interviewer one [`InterviewRequest`] per
//! question on its own task, registers a sensitive answer as a secret before
//! anything else sees it, delivers the answer through the
//! [`CoordinatorHandle`], refuses late and duplicate answers, and on
//! [`InterviewDispatcher::shutdown`] ends every pending wait and writes the
//! [`InterviewReceipt`] a host persists beside the run.
//!
//! # Concurrency
//!
//! Questions from parallel stages reach the interviewer concurrently, one
//! [`Interviewer::reply`] call per question, each on its own task. An
//! interviewer that must serialize (a terminal has one keyboard) does so
//! itself. The observer callback never blocks: it records the question and
//! spawns.
//!
//! # Cancellation and expiry
//!
//! The `cancel` token an interviewer receives fires when the step reports
//! that the question expired ([`QuestionExpired`] on its progress channel),
//! when the question's firing finishes without the answer (the run was
//! cancelled, a sibling failed the branch) and when the dispatcher shuts
//! down. An interviewer returns promptly once it fires; a reply that arrives
//! anyway is recorded as late and not delivered.
//!
//! An expiry is the step's own disposition, not the interviewer's: the record
//! says [`ReplyRecord::TimedOut`] with the default the step took, if it had
//! one, and [`Delivery::Expired`]. The dispatcher never infers a timeout from
//! the firing's end, and an expiry is not an interview error.
//!
//! # Late and duplicate answers
//!
//! A reply for a firing that has already finished is late: recorded as an
//! error, never delivered. A question event that repeats the id of a question
//! whose reply is still pending is a duplicate: ignored. A question event that
//! repeats an id after its answer was delivered is the step asking again (the
//! answer it got named no choice); the interviewer sees it as a new request
//! with the same `occurrence` and a higher `ask`, so a scripted fixture can
//! tell a re-ask from a new question.
//!
//! # Wait accounting
//!
//! The dispatcher decides nothing about time. The driver records the wait
//! itself: a question on a firing's progress channel starts an interaction
//! wait for that firing and attempt, the delivered answer ends it, and an
//! `ExecutorEnforced` attempt budget stops counting in between
//! ([`ir::TimeoutPolicy`]). The watchdog
//! ([`StallWatchdog`](crate::watchdog::StallWatchdog)) parks on the same
//! facts. A `HandlerManaged` step (a human gate) owns its own answer deadline
//! and expires the question itself, reporting the expiry before it acts on
//! it; the driver, the watchdog and this dispatcher all end the wait on that
//! report, with the question and attempt identity it names.
//!
//! # Answer shapes
//!
//! A single choice is `Answer::choice(key)`. A `multi_select` answer is
//! `Answer::choices(keys)`: `choices` carries every selected key, and
//! `choice` repeats the first so a step that routes on one choice still
//! routes. This is the shape the pinned Fabro reference sends over its API
//! as `{"kind": "multi_selected", "option_keys": [...]}`; `choices` is
//! `option_keys`. Free text is `Answer::text`; a refusal is the negative
//! choice; `Answer::cancelled()` is the host's "no answer".
//!
//! # Errors and refusal
//!
//! [`InterviewReply::Failed`] means the interviewer itself could not answer:
//! a closed terminal, a fixture with no matching entry. The dispatcher records
//! the error, delivers [`Answer::cancelled`] so the step fails closed, and
//! the host surfaces the receipt's errors. [`InterviewReply::Cancelled`]
//! delivers the same. Refusing is not an error: it is an ordinary
//! [`InterviewReply::Answered`] naming the negative choice.
//!
//! # Receipt order
//!
//! Reply tasks finish in whatever order the interviewer answers, so the
//! dispatcher orders the receipt's `questions` itself when it produces the
//! receipt: by invocation path, then invocation, execution, firing,
//! occurrence, and ask. The root invocation's questions (`/`) come first
//! and each nested invocation's follow in path order (`/branch:fan@2:0:a`
//! before `/branch:fan@2:1:b`); the path leads because invocation ids are
//! allocated in declaration order, which two parallel branches decide by
//! timing, while the path is the same on every run. The id then orders
//! re-invocations of one slot, and within one invocation the key is the
//! order the run asked. A parallel branch is its own invocation, so its
//! questions never interleave with the root's. Every record has its own
//! key (a re-ask keeps the occurrence and takes the next ask), so the order
//! is total: two runs that ask the same questions write the same receipt
//! order whatever the answer timing. `petri inspect` passes the receipt
//! through as the host wrote it.

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::mem;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use engine::{EngineState, Event, EventRecord};
use executor::SecretProvider;
use ir::{Attempt, Control, FiringId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use smol_str::SmolStr;
use steps::{Answer, Question, QuestionExpired};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    CoordinatorEvent, CoordinatorHandle, CoordinatorRecord, CoordinatorState, ExecutionId,
    ExecutionObserver, InvocationId, ParentCallKey,
};

/// The receipt format this module writes. Bump when a field changes meaning.
/// Version 2 continues a receipt across host processes: each record says
/// which process asked it (`lifetime`).
pub const RECEIPT_VERSION: u32 = 2;

/// The receipt's file name under a standalone run dir.
pub const RECEIPT_FILE: &str = "interviews.json";

/// How long `shutdown` waits for reply tasks that ignore their cancel token.
const SHUTDOWN_PATIENCE: Duration = Duration::from_secs(5);

/// One question, with everything a host needs to tell it apart from every
/// other question this run asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterviewRequest {
    pub invocation:      InvocationId,
    /// The logical path of the invocation: `/` for the root, then one `/`
    /// segment per nested call slot (`/manager/1`).
    pub invocation_path: String,
    pub execution:       ExecutionId,
    pub firing:          FiringId,
    pub attempt:         Attempt,
    /// The node instance name.
    pub node:            SmolStr,
    /// Which distinct question this node instance is asking within its
    /// invocation, 1-based. A node that fires again in a loop asks a new
    /// question and the occurrence advances.
    pub occurrence:      u32,
    /// How many times this exact question id has been asked, 1-based. Greater
    /// than one only when the step re-asked after rejecting an answer.
    pub ask:             u32,
    pub question:        Question,
}

/// What an interviewer decided.
#[derive(Debug)]
pub enum InterviewReply {
    /// An answer to bind to the question and deliver. A refusal is an
    /// `Answered` naming the negative choice.
    Answered(Answer),
    /// The interviewer stopped waiting: the token fired, or a script said to
    /// cancel this interview.
    Cancelled,
    /// The interviewer itself failed. The gate fails closed and the error
    /// reaches the receipt.
    Failed(InterviewError),
}

/// A failure of the interviewer, not a negative answer.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct InterviewError {
    message: String,
    #[source]
    source:  Option<Box<dyn StdError + Send + Sync + 'static>>,
    /// What the interviewer wants in the receipt's `script` section even
    /// though it failed: a scripted interviewer's per-entry counts when a
    /// required entry went unused.
    report:  Option<Value>,
}

impl InterviewError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source:  None,
            report:  None,
        }
    }

    pub fn with_source(
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source:  Some(Box::new(source)),
            report:  None,
        }
    }

    /// Attach the receipt section a failed `finish` still has to report.
    #[must_use]
    pub fn with_report(mut self, report: Value) -> Self {
        self.report = Some(report);
        self
    }

    pub fn report(&self) -> Option<&Value> {
        self.report.as_ref()
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Where a run's questions go.
///
/// Implementations choose answers and nothing else: correlation, secret
/// registration, delivery and the receipt belong to the
/// [`InterviewDispatcher`]. See the module documentation for the concurrency,
/// cancellation and error contract an implementation relies on.
#[async_trait::async_trait]
pub trait Interviewer: Send + Sync {
    /// Answer one question. Return promptly once `cancel` fires; the reply is
    /// then recorded as cancelled or late and never delivered.
    async fn reply(&self, request: InterviewRequest, cancel: CancellationToken) -> InterviewReply;

    /// Called once after the run finished and every reply task ended. An
    /// implementation with expectations of its own (a script with required
    /// entries) reports what was left unmet as the error, and may return a
    /// summary the host writes into the receipt under `script`. The default
    /// reports nothing.
    async fn finish(&self) -> Result<Option<Value>, InterviewError> {
        Ok(None)
    }
}

/// How an answer left the dispatcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// The control reached the firing's channel.
    Delivered,
    /// The firing was no longer live when the control was sent.
    NotLive,
    /// The firing finished before the interviewer replied; nothing was sent.
    Late,
    /// The dispatcher shut down while the reply was pending; nothing was sent.
    Shutdown,
    /// A sensitive answer could not be registered as a secret; its plaintext
    /// was withheld and the gate cancelled instead.
    Withheld,
    /// The step's answer deadline passed before the interviewer replied;
    /// nothing was sent.
    Expired,
}

/// How the question was resolved, as the receipt records it: what the
/// interviewer replied, or the step's own timeout. A sensitive text answer
/// appears only as its `$secret` reference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplyRecord {
    Answered {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        choice:  Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        choices: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text:    Option<Value>,
    },
    /// The interviewer stopped without an answer: it was told to (the run
    /// was cancelled, the dispatcher shut down) or a script said to cancel.
    Cancelled,
    Failed {
        error: String,
    },
    /// The step's answer deadline passed with no answer, as the step itself
    /// reported. `default` is the option the step took on its own, by key,
    /// when it had one; without one the step failed with its own outcome.
    TimedOut {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
    },
}

/// One question the run asked, and what became of it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterviewRecord {
    pub invocation:      InvocationId,
    pub invocation_path: String,
    pub execution:       ExecutionId,
    pub firing:          FiringId,
    pub attempt:         Attempt,
    pub node:            SmolStr,
    pub occurrence:      u32,
    pub ask:             u32,
    pub question:        String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind:            Option<String>,
    pub text:            String,
    pub options:         Vec<String>,
    pub sensitive:       bool,
    /// What the question asked the person to review, when it named
    /// something (a `review_target` gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference:       Option<steps::QuestionReference>,
    /// The step's answer deadline, when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms:      Option<u64>,
    pub reply:           ReplyRecord,
    pub delivery:        Delivery,
    /// The host process that asked it, counting from 0 among the processes
    /// that wrote the receipt ([`InterviewReceipt::lifetime`]).
    #[serde(default)]
    pub lifetime:        u32,
}

/// The machine-readable record of a run's interviews. The standalone host
/// writes it as JSON to `<run_dir>/interviews.json` ([`RECEIPT_FILE`]) each
/// time a question's outcome is recorded, and a resumed host continues the
/// receipt it finds ([`InterviewDispatcher::continuing`]), so a crash loses
/// only the questions still waiting, which the resumed run asks again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InterviewReceipt {
    pub version:   u32,
    /// The last host process to write it, counting from 0.
    #[serde(default)]
    pub lifetime:  u32,
    /// Every question the run asked, in the receipt order: by invocation
    /// path, then invocation, execution, firing, occurrence, and ask
    /// ([`Self::sort_questions`]).
    pub questions: Vec<InterviewRecord>,
    /// Interviewer failures, late replies, withheld plaintext, pending tasks
    /// at shutdown, and whatever `finish` reported. Non-empty means the run's
    /// interviews did not go as intended, whatever the engine status says.
    pub errors:    Vec<String>,
    /// What [`Interviewer::finish`] returned: a scripted interviewer's
    /// per-entry consumption, for instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script:    Option<Value>,
}

impl InterviewReceipt {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }

    /// Put `questions` in the receipt order: by invocation path, then
    /// invocation, execution, firing, occurrence, and ask (the module docs
    /// define it). The dispatcher applies it when it produces the receipt;
    /// a host that assembles a receipt from its own records applies it
    /// before writing.
    pub fn sort_questions(&mut self) {
        self.questions
            .sort_by(|left, right| receipt_key(left).cmp(&receipt_key(right)));
    }
}

/// The receipt order's key for one record (see the module docs); a question
/// asked again in a later process follows the earlier one.
fn receipt_key(
    record: &InterviewRecord,
) -> (&str, InvocationId, ExecutionId, FiringId, u32, u32, u32) {
    (
        record.invocation_path.as_str(),
        record.invocation,
        record.execution,
        record.firing,
        record.occurrence,
        record.ask,
        record.lifetime,
    )
}

/// Receives the receipt as it grows ([`InterviewDispatcher::publish_to`]).
pub type ReceiptSink = Arc<dyn Fn(&InterviewReceipt) + Send + Sync>;

struct Wiring {
    handle:  CoordinatorHandle,
    secrets: Arc<dyn SecretProvider>,
}

struct LiveQuestion {
    firing:  FiringId,
    cancel:  CancellationToken,
    /// Set when the step reported the question expired, before `cancel`
    /// fired for it.
    expired: Option<QuestionExpired>,
}

#[derive(Default)]
struct State {
    /// Which invocation each execution belongs to, from the lifecycle log.
    executions:  BTreeMap<ExecutionId, InvocationId>,
    /// Each invocation's logical path.
    paths:       BTreeMap<InvocationId, String>,
    /// Distinct questions per (invocation, node instance).
    occurrences: BTreeMap<(InvocationId, SmolStr), u32>,
    /// Asks per (execution, question id).
    asks:        BTreeMap<(ExecutionId, String), u32>,
    /// Questions whose reply is pending.
    live:        BTreeMap<(ExecutionId, String), LiveQuestion>,
    records:     Vec<InterviewRecord>,
    errors:      Vec<String>,
    tasks:       Vec<JoinHandle<()>>,
    /// Set once `shutdown` starts; later questions are refused.
    closed:      bool,
}

struct Inner {
    interviewer: Arc<dyn Interviewer>,
    wiring:      OnceLock<Wiring>,
    shutdown:    CancellationToken,
    state:       Mutex<State>,
    /// This host process, counting from 0 among those that wrote the
    /// receipt.
    lifetime:    u32,
    sink:        OnceLock<ReceiptSink>,
}

/// The host side of the interview boundary. Construct before the run,
/// [`observe`](crate::host::HostRun::observe) a clone, [`wire`](Self::wire) it
/// once the coordinator exists, and [`shutdown`](Self::shutdown) it after the
/// run for the receipt. Clones share one dispatcher.
#[derive(Clone)]
pub struct InterviewDispatcher {
    inner: Arc<Inner>,
}

impl InterviewDispatcher {
    pub fn new(interviewer: Arc<dyn Interviewer>) -> Self {
        Self::continuing(interviewer, None)
    }

    /// A dispatcher for a resumed host: the receipt an earlier process wrote
    /// keeps its questions and errors, and this process's records follow
    /// them as the next lifetime.
    pub fn continuing(
        interviewer: Arc<dyn Interviewer>,
        earlier: Option<InterviewReceipt>,
    ) -> Self {
        let mut state = State::default();
        state.paths.insert(InvocationId::ROOT, "/".to_owned());
        let lifetime = earlier.as_ref().map_or(0, |earlier| earlier.lifetime + 1);
        if let Some(earlier) = earlier {
            state.records = earlier.questions;
            state.errors = earlier.errors;
        }
        Self {
            inner: Arc::new(Inner {
                interviewer,
                wiring: OnceLock::new(),
                shutdown: CancellationToken::new(),
                state: Mutex::new(state),
                lifetime,
                sink: OnceLock::new(),
            }),
        }
    }

    /// Hand `sink` the receipt, whole, each time a question's outcome or an
    /// interview error is recorded: a host that writes it to disk loses no
    /// more than the questions still waiting when it crashes.
    pub fn publish_to(&self, sink: ReceiptSink) {
        let _ = self.inner.sink.set(sink);
    }

    /// Connect to the running coordinator. `run_configured` hands both over
    /// before the first record; a question that arrives unwired is refused
    /// and recorded as an error.
    pub fn wire(&self, handle: CoordinatorHandle, secrets: Arc<dyn SecretProvider>) {
        let _ = self.inner.wiring.set(Wiring { handle, secrets });
    }

    /// End every pending wait, join the reply tasks, ask the interviewer to
    /// finish, and produce the receipt. Call once, after the run.
    pub async fn shutdown(&self) -> InterviewReceipt {
        let inner = &self.inner;
        inner.shutdown.cancel();
        let tasks = {
            let mut state = inner.state();
            state.closed = true;
            mem::take(&mut state.tasks)
        };
        for task in tasks {
            match timeout(SHUTDOWN_PATIENCE, task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => inner
                    .state()
                    .errors
                    .push(format!("a reply task failed: {error}")),
                Err(_) => inner
                    .state()
                    .errors
                    .push("a reply task was still pending after shutdown".to_owned()),
            }
        }
        let script = match inner.interviewer.finish().await {
            Ok(script) => script,
            Err(error) => {
                inner.state().errors.push(error.to_string());
                // A failed verification still reports what it counted.
                error.report().cloned()
            }
        };
        let mut receipt = {
            let mut state = inner.state();
            InterviewReceipt {
                version: RECEIPT_VERSION,
                lifetime: inner.lifetime,
                questions: mem::take(&mut state.records),
                errors: mem::take(&mut state.errors),
                script,
            }
        };
        // The records arrived in reply order; the receipt is in ask order.
        receipt.sort_questions();
        receipt
    }
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record a question's outcome, and publish the receipt.
    fn keep(&self, mut record: InterviewRecord) {
        record.lifetime = self.lifetime;
        self.state().records.push(record);
        self.publish();
    }

    /// Record an interview error, and publish the receipt.
    fn fail(&self, error: String) {
        self.state().errors.push(error);
        self.publish();
    }

    /// Hand the receipt so far to the sink, when there is one.
    fn publish(&self) {
        let Some(sink) = self.sink.get() else {
            return;
        };
        let mut receipt = {
            let state = self.state();
            InterviewReceipt {
                version:   RECEIPT_VERSION,
                lifetime:  self.lifetime,
                questions: state.records.clone(),
                errors:    state.errors.clone(),
                script:    None,
            }
        };
        receipt.sort_questions();
        sink(&receipt);
    }

    /// Correlate a question and start its reply task. Never blocks.
    fn ask(
        self: &Arc<Self>,
        execution: ExecutionId,
        firing: FiringId,
        question: Question,
        engine: &EngineState,
    ) {
        let Some(wiring) = self.wiring.get() else {
            self.fail(format!(
                "question `{}` arrived before the dispatcher was wired",
                question.id
            ));
            return;
        };
        let attempt = engine
            .firing(firing)
            .map_or(Attempt::new(1), |live| live.attempt);
        let node = engine
            .firing_node(firing)
            .and_then(|id| engine.graph().node(id))
            .map_or_else(|| SmolStr::new("?"), |node| node.name.clone());
        let (request, cancel) = {
            let mut state = self.state();
            if state.closed {
                state
                    .errors
                    .push(format!("question `{}` arrived after shutdown", question.id));
                return;
            }
            let key = (execution, question.id.clone());
            if state.live.contains_key(&key) {
                tracing::debug!(question = %question.id, "duplicate question while a reply is pending");
                return;
            }
            let invocation = state
                .executions
                .get(&execution)
                .copied()
                .unwrap_or(InvocationId::ROOT);
            let invocation_path = state
                .paths
                .get(&invocation)
                .cloned()
                .unwrap_or_else(|| "/".to_owned());
            let ask = state.asks.entry(key.clone()).or_insert(0);
            *ask += 1;
            let ask = *ask;
            let occurrence = if ask == 1 {
                let count = state
                    .occurrences
                    .entry((invocation, node.clone()))
                    .or_insert(0);
                *count += 1;
                *count
            } else {
                state
                    .occurrences
                    .get(&(invocation, node.clone()))
                    .copied()
                    .unwrap_or(1)
            };
            let cancel = self.shutdown.child_token();
            state.live.insert(key, LiveQuestion {
                firing,
                cancel: cancel.clone(),
                expired: None,
            });
            (
                InterviewRequest {
                    invocation,
                    invocation_path,
                    execution,
                    firing,
                    attempt,
                    node,
                    occurrence,
                    ask,
                    question,
                },
                cancel,
            )
        };
        let handle = wiring.handle.clone();
        let secrets = wiring.secrets.clone();
        let this = self.clone();
        let task = tokio::spawn(async move {
            this.serve(request, cancel, handle, secrets).await;
        });
        self.state().tasks.push(task);
    }

    /// The step reported that a question expired: keep the report for the
    /// record and end the interviewer's wait. A question that is not pending
    /// (already answered and delivered, or never asked here) has nothing to
    /// expire.
    fn expire(&self, execution: ExecutionId, expired: QuestionExpired) {
        let mut state = self.state();
        if let Some(live) = state.live.get_mut(&(execution, expired.question.clone())) {
            let cancel = live.cancel.clone();
            live.expired = Some(expired);
            drop(state);
            cancel.cancel();
        }
    }

    async fn serve(
        &self,
        request: InterviewRequest,
        cancel: CancellationToken,
        handle: CoordinatorHandle,
        secrets: Arc<dyn SecretProvider>,
    ) {
        let key = (request.execution, request.question.id.clone());
        let firing = request.firing;
        let mut record = InterviewRecord {
            invocation:      request.invocation,
            invocation_path: request.invocation_path.clone(),
            execution:       request.execution,
            firing:          request.firing,
            attempt:         request.attempt,
            node:            request.node.clone(),
            occurrence:      request.occurrence,
            ask:             request.ask,
            question:        request.question.id.clone(),
            kind:            request.question.kind.clone(),
            text:            request.question.text.clone(),
            options:         request
                .question
                .options
                .iter()
                .map(|option| option.key.clone())
                .collect(),
            sensitive:       request.question.sensitive,
            reference:       request.question.reference.clone(),
            timeout_ms:      request.question.timeout_ms,
            reply:           ReplyRecord::Cancelled,
            delivery:        Delivery::Shutdown,
            lifetime:        self.lifetime,
        };
        let question = request.question.clone();
        // A reply ready in the same poll as the close wins, and is recorded
        // below as late: the same way on every run.
        let reply = tokio::select! {
            biased;
            reply = self.interviewer.reply(request, cancel.clone()) => Some(reply),
            () = cancel.cancelled() => None,
        };
        // Whatever the reply, this question is no longer pending. Whether it
        // is still deliverable depends on whether the token fired: the step
        // expired the question, the firing finished, or the dispatcher shut
        // down.
        let live = self.state().live.remove(&key);
        let was_live = live.is_some();
        if let Some(expired) = live.and_then(|live| live.expired) {
            // The step's own disposition: it reported the expiry and acted
            // on it. An answer or a failure that raced in beside it was not
            // delivered and is worth a line in the receipt's errors; a
            // cancelled reply is the interviewer stopping when told to.
            record.reply = ReplyRecord::TimedOut {
                default: expired.default,
            };
            record.delivery = Delivery::Expired;
            if matches!(
                reply,
                Some(InterviewReply::Answered(_) | InterviewReply::Failed(_))
            ) {
                self.fail(format!(
                    "a reply to `{}` arrived after the question expired",
                    key.1
                ));
            }
            self.keep(record);
            return;
        }
        let closed = self.shutdown.is_cancelled();
        if cancel.is_cancelled() || !was_live || closed {
            record.delivery = if closed {
                Delivery::Shutdown
            } else {
                Delivery::Late
            };
            match reply {
                // The interviewer stopped when told to: the ordinary end of
                // a question nobody could answer any more.
                None | Some(InterviewReply::Cancelled) => {
                    record.reply = ReplyRecord::Cancelled;
                }
                // An answer, or a failure, after the firing finished: not
                // delivered, and worth a line in the receipt's errors.
                Some(reply) => {
                    record.reply = describe(&reply);
                    self.fail(format!(
                        "a reply to `{}` arrived after its firing finished",
                        key.1
                    ));
                }
            }
            self.keep(record);
            return;
        }
        let Some(reply) = reply else {
            record.delivery = Delivery::Late;
            self.keep(record);
            return;
        };
        let control = match reply {
            InterviewReply::Answered(answer) => {
                let mut answer = answer.for_question(&question.id);
                if question.sensitive
                    && answer.choice.is_none()
                    && answer.choices.is_empty()
                    && let Some(Value::String(text)) = answer.text.clone()
                {
                    let name = question.secret_name();
                    // Registered first, then referenced: the log sees the
                    // name only. A registration failure withholds the value.
                    if let Err(error) = secrets.register(&name, &text) {
                        self.fail(format!(
                            "could not register the answer to `{}` as a secret: {error}",
                            question.id
                        ));
                        record.reply = ReplyRecord::Answered {
                            choice:  None,
                            choices: Vec::new(),
                            text:    Some(json!({ "$secret": name })),
                        };
                        record.delivery = Delivery::Withheld;
                        let _ = handle.deliver(key.0, firing, cancelled(&question.id)).await;
                        self.keep(record);
                        return;
                    }
                    answer.text = Some(json!({ "$secret": name }));
                }
                record.reply = ReplyRecord::Answered {
                    choice:  answer.choice.clone(),
                    choices: answer.choices.clone(),
                    text:    answer.text.clone(),
                };
                answer.to_control()
            }
            InterviewReply::Cancelled => {
                record.reply = ReplyRecord::Cancelled;
                cancelled(&question.id)
            }
            InterviewReply::Failed(error) => {
                let rendered = chain(&error);
                self.fail(format!(
                    "the interviewer failed on `{}`: {rendered}",
                    question.id
                ));
                record.reply = ReplyRecord::Failed { error: rendered };
                cancelled(&question.id)
            }
        };
        let disposition = handle.deliver(key.0, firing, control).await;
        record.delivery = match disposition {
            driver::DeliverDisposition::Delivered => Delivery::Delivered,
            driver::DeliverDisposition::NotLive => Delivery::NotLive,
        };
        self.keep(record);
    }
}

/// The control that ends a question without an answer. The engine forwards
/// only `Control::Deliver` from a host, so the "no answer" is itself an
/// answer, marked cancelled; the step fails closed on it.
fn cancelled(question: &str) -> Control {
    Answer::cancelled().for_question(question).to_control()
}

fn describe(reply: &InterviewReply) -> ReplyRecord {
    match reply {
        InterviewReply::Answered(answer) => ReplyRecord::Answered {
            choice:  answer.choice.clone(),
            choices: answer.choices.clone(),
            text:    answer.text.clone(),
        },
        InterviewReply::Cancelled => ReplyRecord::Cancelled,
        InterviewReply::Failed(error) => ReplyRecord::Failed {
            error: chain(error),
        },
    }
}

/// An error and its source chain on one line.
fn chain(error: &dyn StdError) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}

impl ExecutionObserver for InterviewDispatcher {
    fn on_engine_record(
        &self,
        execution: ExecutionId,
        record: &EventRecord,
        _recorded_at: u64,
        state: &EngineState,
    ) {
        match &record.event {
            Event::StepProgressRecorded { firing, ev } => {
                if let Some(question) = Question::from_event(ev) {
                    self.inner.ask(execution, *firing, question, state);
                } else if let Some(expired) = QuestionExpired::from_event(ev) {
                    self.inner.expire(execution, expired);
                }
            }
            Event::StepFinished { firing, .. } => {
                // The firing is gone: every pending question of its is
                // unanswerable now. Ending the wait tells the interviewer to
                // stop, and marks whatever arrives afterwards as late. Firing
                // ids are per execution, so the execution is part of the key:
                // a parallel branch's child execution numbers its firings
                // from one like every other.
                let state = self.inner.state();
                for live in state
                    .live
                    .iter()
                    .filter(|((owner, _), live)| *owner == execution && live.firing == *firing)
                    .map(|(_, live)| live)
                {
                    live.cancel.cancel();
                }
            }
            _ => {}
        }
    }

    fn on_lifecycle(&self, record: &CoordinatorRecord) {
        let mut state = self.inner.state();
        match &record.body {
            CoordinatorEvent::ExecutionDeclared {
                execution,
                invocation,
                ..
            } => {
                state.executions.insert(*execution, *invocation);
            }
            CoordinatorEvent::InvocationDeclared {
                invocation,
                call: Some(call),
                ..
            } => {
                let path = state.path_of(call);
                state.paths.insert(*invocation, path);
            }
            _ => {}
        }
    }

    /// Start from the run as the log left it: every declared execution's
    /// invocation and every invocation's path, so a question a nested
    /// invocation asks again after a resume is filed where it belongs.
    fn on_resumed(&self, coordinator: &CoordinatorState) {
        let mut state = self.inner.state();
        for (execution, declared) in &coordinator.executions {
            state
                .executions
                .insert(*execution, declared.declaration.invocation);
        }
        // A caller is declared before what it calls: in id order, every
        // parent's path is known before its children's.
        for (invocation, declared) in &coordinator.invocations {
            if let Some(call) = &declared.declaration.call {
                let path = state.path_of(call);
                state.paths.insert(*invocation, path);
            }
        }
    }
}

impl State {
    /// The logical path of the invocation called from `call`: its caller's
    /// path, then the call's slot.
    fn path_of(&self, call: &ParentCallKey) -> String {
        let parent = self
            .executions
            .get(&call.parent)
            .and_then(|parent| self.paths.get(parent))
            .cloned()
            .unwrap_or_else(|| "/".to_owned());
        if parent == "/" {
            format!("/{}", call.slot)
        } else {
            format!("{parent}/{}", call.slot)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        invocation: u64,
        execution: u64,
        firing: u64,
        occurrence: u32,
        ask: u32,
    ) -> InterviewRecord {
        record_at("/", invocation, execution, firing, occurrence, ask)
    }

    fn record_at(
        path: &str,
        invocation: u64,
        execution: u64,
        firing: u64,
        occurrence: u32,
        ask: u32,
    ) -> InterviewRecord {
        InterviewRecord {
            invocation: InvocationId::new(invocation),
            invocation_path: path.to_owned(),
            execution: ExecutionId::new(execution),
            firing: FiringId::new(firing),
            attempt: Attempt::FIRST,
            node: SmolStr::new("gate"),
            occurrence,
            ask,
            question: format!("gate#{firing}"),
            kind: None,
            text: String::new(),
            options: Vec::new(),
            sensitive: false,
            reference: None,
            timeout_ms: None,
            reply: ReplyRecord::Cancelled,
            delivery: Delivery::Delivered,
            lifetime: 0,
        }
    }

    fn keys(receipt: &InterviewReceipt) -> Vec<(u64, u64, u64, u32, u32)> {
        receipt
            .questions
            .iter()
            .map(|record| {
                (
                    record.invocation.raw(),
                    record.execution.raw(),
                    record.firing.raw(),
                    record.occurrence,
                    record.ask,
                )
            })
            .collect()
    }

    /// Records land in reply order; the receipt is in ask order, whatever
    /// the interviewer's timing was.
    #[test]
    fn the_receipt_orders_questions_as_the_run_asked_them() {
        let mut receipt = InterviewReceipt {
            lifetime:  0,
            version:   RECEIPT_VERSION,
            questions: vec![
                // The re-ask of a gate, answered before the original ask.
                record(0, 0, 3, 1, 2),
                // A nested invocation's gate, answered first of all.
                record_at("/m", 1, 1, 1, 1, 1),
                // The same node's next firing in a loop.
                record(0, 0, 5, 2, 1),
                // The original ask.
                record(0, 0, 3, 1, 1),
                // A second question in the same firing.
                record(0, 0, 3, 3, 1),
                // The root's second execution after a restart.
                record(0, 2, 1, 1, 1),
            ],
            errors:    Vec::new(),
            script:    None,
        };
        receipt.sort_questions();
        assert_eq!(keys(&receipt), vec![
            (0, 0, 3, 1, 1),
            (0, 0, 3, 1, 2),
            (0, 0, 3, 3, 1),
            (0, 0, 5, 2, 1),
            (0, 2, 1, 1, 1),
            (1, 1, 1, 1, 1),
        ]);
    }

    /// Two parallel branches take their invocation ids in declaration
    /// order, which timing decides; the path orders them the same way on
    /// every run. The root's later question still precedes both.
    #[test]
    fn parallel_branches_follow_their_paths_not_their_ids() {
        let mut receipt = InterviewReceipt {
            lifetime:  0,
            version:   RECEIPT_VERSION,
            questions: vec![
                record_at("/branch:fan@2:1:b", 1, 1, 1, 1, 1),
                record_at("/branch:fan@2:0:a", 2, 2, 1, 1, 1),
                record(0, 0, 7, 2, 1),
            ],
            errors:    Vec::new(),
            script:    None,
        };
        receipt.sort_questions();
        let paths: Vec<&str> = receipt
            .questions
            .iter()
            .map(|record| record.invocation_path.as_str())
            .collect();
        assert_eq!(paths, vec!["/", "/branch:fan@2:0:a", "/branch:fan@2:1:b"]);
    }

    /// Sorting an ordered receipt changes nothing.
    #[test]
    fn an_ordered_receipt_stays_as_it_is() {
        let questions = vec![record(0, 0, 2, 1, 1), record(0, 0, 2, 1, 2)];
        let mut receipt = InterviewReceipt {
            lifetime:  0,
            version:   RECEIPT_VERSION,
            questions: questions.clone(),
            errors:    Vec::new(),
            script:    None,
        };
        receipt.sort_questions();
        assert_eq!(receipt.questions, questions);
    }

    /// An interviewer that never answers: these tests read the dispatcher's
    /// bookkeeping only.
    struct Silent;

    #[async_trait::async_trait]
    impl Interviewer for Silent {
        async fn reply(
            &self,
            _request: InterviewRequest,
            cancel: CancellationToken,
        ) -> InterviewReply {
            cancel.cancelled().await;
            InterviewReply::Cancelled
        }
    }

    /// The lifecycle records of a root that calls a child from execution 0,
    /// which calls a grandchild from execution 1.
    fn nested_run() -> Vec<CoordinatorRecord> {
        let digest = "0".repeat(64);
        let start = serde_json::json!({
            "entry": "graph_entries", "context": {}, "prior_firings": {},
            "execution_index": 0, "max_executions": 32
        });
        let call = |parent: u64, slot: &str| serde_json::json!({"parent": parent, "firing": 1, "attempt": 1, "slot": slot});
        let bodies = [
            serde_json::json!({"event": "run.started", "format_version": crate::COORDINATOR_FORMAT_VERSION,
                "key": "run", "root": 0, "middleware_chain": []}),
            serde_json::json!({"event": "graph.registered", "digest": digest}),
            serde_json::json!({"event": "invocation.declared", "invocation": 0, "call": null,
                "graph": digest, "context": {}, "secret_bindings": "none", "sandbox": "isolated"}),
            serde_json::json!({"event": "execution.declared", "execution": 0, "invocation": 0,
                "predecessor": null, "start": start, "middleware_state": {}}),
            serde_json::json!({"event": "invocation.declared", "invocation": 1, "call": call(0, "c0"),
                "graph": digest, "context": {}, "secret_bindings": "none", "sandbox": "isolated"}),
            serde_json::json!({"event": "execution.declared", "execution": 1, "invocation": 1,
                "predecessor": null, "start": start, "middleware_state": {}}),
            serde_json::json!({"event": "invocation.declared", "invocation": 2, "call": call(1, "c1"),
                "graph": digest, "context": {}, "secret_bindings": "none", "sandbox": "isolated"}),
            serde_json::json!({"event": "execution.declared", "execution": 2, "invocation": 2,
                "predecessor": null, "start": start, "middleware_state": {}}),
        ];
        bodies
            .into_iter()
            .enumerate()
            .map(|(seq, body)| CoordinatorRecord {
                seq:         seq as u64,
                origin:      engine::EventOrigin::External,
                recorded_at: 0,
                body:        serde_json::from_value(body).expect("a lifecycle record"),
            })
            .collect()
    }

    fn mapping(
        dispatcher: &InterviewDispatcher,
    ) -> (
        BTreeMap<ExecutionId, InvocationId>,
        BTreeMap<InvocationId, String>,
    ) {
        let state = dispatcher.inner.state();
        (state.executions.clone(), state.paths.clone())
    }

    /// A resumed dispatcher knows every declared execution and every
    /// invocation's path, as one that saw the records live does, so a
    /// question a nested invocation asks after a resume is filed under it.
    #[test]
    fn a_resumed_dispatcher_maps_nested_invocations_as_a_live_one_does() {
        let records = nested_run();
        let live = InterviewDispatcher::new(Arc::new(Silent));
        for record in &records {
            live.on_lifecycle(record);
        }
        let resumed = InterviewDispatcher::new(Arc::new(Silent));
        resumed.on_resumed(&CoordinatorState::replay(&records).expect("the records replay"));
        assert_eq!(mapping(&resumed), mapping(&live));
        assert_eq!(mapping(&resumed).1[&InvocationId::new(2)], "/c0/c1");
    }

    /// A resumed host's dispatcher continues the receipt it found: the
    /// earlier questions and errors stay, its own records carry the next
    /// lifetime, and the sink sees the whole receipt at each outcome.
    #[tokio::test]
    async fn a_continued_receipt_keeps_the_earlier_questions_and_counts_lifetimes() {
        let earlier = InterviewReceipt {
            version:   RECEIPT_VERSION,
            lifetime:  0,
            questions: vec![record(0, 0, 1, 1, 1)],
            errors:    vec!["an earlier error".to_owned()],
            script:    None,
        };
        let dispatcher = InterviewDispatcher::continuing(Arc::new(Silent), Some(earlier));
        let published = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&published);
        dispatcher.publish_to(Arc::new(move |receipt: &InterviewReceipt| {
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(receipt.clone());
        }));
        dispatcher.inner.keep(record(0, 0, 1, 1, 2));
        let published = published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        assert_eq!(published.len(), 1);
        let receipt = &published[0];
        assert_eq!(receipt.lifetime, 1);
        assert_eq!(receipt.errors, ["an earlier error"]);
        let lifetimes: Vec<(u32, u32)> = receipt
            .questions
            .iter()
            .map(|question| (question.ask, question.lifetime))
            .collect();
        assert_eq!(lifetimes, [(1, 0), (2, 1)]);
        let finished = dispatcher.shutdown().await;
        assert_eq!(finished.lifetime, 1);
        assert_eq!(finished.questions.len(), 2);
    }
}
