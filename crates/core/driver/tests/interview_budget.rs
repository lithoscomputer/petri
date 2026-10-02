//! Interview-aware attempt budgets (readiness item 6), on a controlled clock.
//!
//! The runtime starts paused, so every wait here is virtual: `advance` moves
//! the clock and the driver's timers fire exactly when the accounting says
//! they should. A fake executor stands in for the sandbox; the step under
//! test asks questions and waits on its control channel, so no process and
//! no plugin is involved.

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use driver::lifecycle::{BUDGET_PAUSED_KIND, BUDGET_RESUMED_KIND, BudgetNote, Note};
use driver::{DeliverDisposition, Driver, EventObserver, ExecutionReport, RunConfig};
use engine::{EngineState, Event, EventLog, EventRecord};
use executor::MapSecrets;
use ir::{
    Budget, CancelScopeId, Control, FiringId, Graph, GraphBuilder, Outcome, RunStatus, ScopeId,
    StepKindId, StepRef, TimeoutPolicy, Value, validate,
};
use serde::Deserialize;
use serde_json::json;
use steps::{Answer, Question, Registry, Step, StepCtx};
use support::sim::NoExecutor;
use support::*;
use tokio::sync::mpsc;
use tokio::task::yield_now;
use tokio::time::{self, Duration as TokioDuration};

// ── A step that works, asks, waits, and works again ───────────────────────

const ASKING: StepKindId = StepKindId::new_static("asking");

#[derive(Deserialize)]
struct AskingConfig {
    #[serde(default)]
    work_before_ms: u64,
    #[serde(default)]
    questions:      u32,
    #[serde(default)]
    work_after_ms:  u64,
}

struct AskingStep;

#[async_trait::async_trait]
impl Step for AskingStep {
    const NAME: &'static str = "asking";
    type Config = AskingConfig;

    async fn run(&self, config: AskingConfig, mut ctx: StepCtx) -> Outcome {
        let mut pending = BTreeSet::new();
        let work = time::sleep(Duration::from_millis(config.work_before_ms));
        tokio::pin!(work);
        loop {
            tokio::select! {
                () = &mut work => break,
                ctl = ctx.control.recv() => match ctl {
                    Some(Control::Deliver(_)) => {}
                    _ => return Outcome::cancelled(),
                },
            }
        }
        for index in 0..config.questions {
            let id = format!("{}#{}/q{index}", ctx.node, ctx.firing.raw());
            pending.insert(id.clone());
            let question = Question::new(id, format!("Question {index}?"));
            let _ = ctx.logs.send(question.to_event()).await;
        }
        while !pending.is_empty() {
            match ctx.control.recv().await {
                Some(Control::Deliver(value)) => {
                    if let Some(id) = Answer::from_value(&value).and_then(|a| a.question) {
                        pending.remove(&id);
                    }
                }
                _ => return Outcome::cancelled(),
            }
        }
        let work = time::sleep(Duration::from_millis(config.work_after_ms));
        tokio::pin!(work);
        loop {
            tokio::select! {
                () = &mut work => break,
                ctl = ctx.control.recv() => match ctl {
                    Some(Control::Deliver(_)) => {}
                    _ => return Outcome::cancelled(),
                },
            }
        }
        Outcome::new(ir::Status::Success, json!({ "asked": config.questions }))
    }
}

/// Every question the run asked, as (firing, id), in order.
struct Asked(mpsc::UnboundedSender<(FiringId, String)>);

impl EventObserver for Asked {
    fn on_record(&self, record: &EventRecord, _recorded_at: u64, _: &EngineState) {
        if let Event::StepProgressRecorded { firing, ev } = &record.event
            && let Some(question) = Question::from_event(ev)
        {
            let _ = self.0.send((*firing, question.id));
        }
    }
}

fn asking(config: Value) -> StepRef {
    StepRef::new(ASKING, config)
}

fn registry() -> Registry {
    let mut registry = Registry::new();
    registry.register(AskingStep);
    registry
}

fn driver(graph: Graph, dir: &RunDir) -> (Driver, mpsc::UnboundedReceiver<(FiringId, String)>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let driver = Driver::new(
        graph,
        Arc::new(NoExecutor::default()),
        registry(),
        Arc::new(MapSecrets::empty()),
        RunConfig::new(dir.path()).with_grace(Duration::from_millis(100)),
    )
    .observe(Arc::new(Asked(tx)));
    (driver, rx)
}

/// Let every runnable task make progress without moving the clock.
async fn settle() {
    for _ in 0..64 {
        yield_now().await;
    }
}

async fn advance(duration: Duration) {
    time::advance(TokioDuration::from_millis(
        u64::try_from(duration.as_millis()).expect("fits"),
    ))
    .await;
    settle().await;
}

async fn next_question(rx: &mut mpsc::UnboundedReceiver<(FiringId, String)>) -> (FiringId, String) {
    for _ in 0..200 {
        if let Ok(question) = rx.try_recv() {
            return question;
        }
        yield_now().await;
    }
    panic!("no question was asked");
}

fn one_node(config: Value, timeout: Duration, policy: TimeoutPolicy) -> Graph {
    let mut b = GraphBuilder::new();
    let node = b.add_node("stage", ScopeId::new(0), asking(config));
    b.set_budget(node, Budget::new(1, timeout).with_timeout_policy(policy));
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// The waiting stage's budget stops while its question is pending and
/// resumes with the remaining time: 6 s of work, a long wait, then 5 s more
/// against the 4 s that were left is a timeout.
#[tokio::test(start_paused = true)]
async fn the_waiting_stage_pays_only_for_active_work() {
    let dir = RunDir::new("budget-own-wait");
    let graph = one_node(
        json!({ "work_before_ms": 6_000, "questions": 1, "work_after_ms": 5_000 }),
        Duration::from_secs(10),
        TimeoutPolicy::ExecutorEnforced,
    );
    let (driver, mut asked) = driver(graph.clone(), &dir);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    settle().await;
    advance(Duration::from_secs(6)).await;
    let (firing, id) = next_question(&mut asked).await;
    // An hour at the prompt costs the stage nothing.
    advance(Duration::from_secs(3600)).await;
    assert!(!run.is_finished(), "the paused budget did not expire");
    assert_eq!(
        handle
            .deliver(firing, Answer::choice("ok").for_question(&id).to_control())
            .await,
        DeliverDisposition::Delivered
    );
    settle().await;
    // 4 s remained; the timer fires before the 5 s of work ends.
    advance(Duration::from_secs(4)).await;
    let report = run.await.expect("the run task");
    assert_eq!(status_of(&report, "stage").as_deref(), Some("timed_out"));
    assert_eq!(report.status, RunStatus::Failed);
    assert_replay_identical(&graph, &report);
    // The pause and the resume are durable notes on the firing, with the
    // time the attempt had left.
    let notes: Vec<(String, BudgetNote)> = report
        .state
        .log
        .records()
        .iter()
        .filter_map(|record| match &record.event {
            Event::StepProgressRecorded { ev, .. } => Note::from_step_event(ev),
            _ => None,
        })
        .filter(|note| note.kind == BUDGET_PAUSED_KIND || note.kind == BUDGET_RESUMED_KIND)
        .map(|note| {
            (
                note.kind.to_string(),
                serde_json::from_value(note.payload).expect("a budget note"),
            )
        })
        .collect();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(notes[0].0, BUDGET_PAUSED_KIND);
    assert_eq!(notes[0].1.pending_questions, 1);
    assert!(
        (3_900..=4_000).contains(&notes[0].1.remaining_ms),
        "4 s left at the pause: {notes:?}"
    );
    assert_eq!(notes[1].0, BUDGET_RESUMED_KIND);
    assert_eq!(notes[1].1.pending_questions, 0);
    assert_eq!(notes[1].1.remaining_ms, notes[0].1.remaining_ms);
}

/// The same shape with 3 s of work after the answer succeeds: the remaining
/// budget is what was left, not a fresh one and not an already-spent one.
#[tokio::test(start_paused = true)]
async fn active_work_after_a_wait_resumes_the_remaining_budget() {
    let dir = RunDir::new("budget-resume-remaining");
    let graph = one_node(
        json!({ "work_before_ms": 6_000, "questions": 1, "work_after_ms": 3_000 }),
        Duration::from_secs(10),
        TimeoutPolicy::ExecutorEnforced,
    );
    let (driver, mut asked) = driver(graph.clone(), &dir);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    settle().await;
    advance(Duration::from_secs(6)).await;
    let (firing, id) = next_question(&mut asked).await;
    advance(Duration::from_secs(3600)).await;
    handle
        .deliver(firing, Answer::choice("ok").for_question(&id).to_control())
        .await;
    settle().await;
    advance(Duration::from_secs(3)).await;
    let report = run.await.expect("the run task");
    assert_eq!(status_of(&report, "stage").as_deref(), Some("success"));
    assert_eq!(report.status, RunStatus::Success);
    assert_replay_identical(&graph, &report);
}

/// A sibling's question never extends another stage's budget: `b` works
/// through its 10 s while `a` waits, and `b` times out on schedule.
#[tokio::test(start_paused = true)]
async fn a_siblings_question_does_not_extend_another_stages_budget() {
    let dir = RunDir::new("budget-sibling");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_node("a", scope, asking(json!({ "questions": 1 })));
    b.set_budget(a, Budget::new(1, Duration::from_secs(10)));
    let slow = b.add_node("b", scope, asking(json!({ "work_before_ms": 15_000 })));
    b.set_budget(slow, Budget::new(1, Duration::from_secs(10)));
    let graph = b.build();
    validate(&graph).expect("valid");
    let (driver, mut asked) = driver(graph.clone(), &dir);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    settle().await;
    let (firing, id) = next_question(&mut asked).await;
    advance(Duration::from_secs(10)).await;
    // `b` is timed out and recorded; `a` still waits.
    assert!(!run.is_finished());
    handle
        .deliver(firing, Answer::choice("ok").for_question(&id).to_control())
        .await;
    settle().await;
    let report = run.await.expect("the run task");
    assert_eq!(status_of(&report, "b").as_deref(), Some("timed_out"));
    assert_eq!(status_of(&report, "a").as_deref(), Some("success"));
    assert_replay_identical(&graph, &report);
}

/// Two questions from one stage overlap: the budget resumes only when the
/// last one is answered, and a stale or repeated answer changes nothing.
#[tokio::test(start_paused = true)]
async fn overlapping_questions_hold_the_budget_until_the_last_answer() {
    let dir = RunDir::new("budget-overlap");
    let graph = one_node(
        json!({ "questions": 2, "work_after_ms": 3_000 }),
        Duration::from_secs(10),
        TimeoutPolicy::ExecutorEnforced,
    );
    let (driver, mut asked) = driver(graph.clone(), &dir);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    settle().await;
    let (firing, first) = next_question(&mut asked).await;
    let (_, second) = next_question(&mut asked).await;
    advance(Duration::from_secs(600)).await;
    // A stale answer to a question nobody asked.
    handle
        .deliver(
            firing,
            Answer::choice("ok")
                .for_question("stage#1/nope")
                .to_control(),
        )
        .await;
    handle
        .deliver(
            firing,
            Answer::choice("ok").for_question(&first).to_control(),
        )
        .await;
    // The same answer again: the first already ended that wait.
    handle
        .deliver(
            firing,
            Answer::choice("ok").for_question(&first).to_control(),
        )
        .await;
    settle().await;
    advance(Duration::from_secs(600)).await;
    assert!(!run.is_finished(), "one question still pending: no clock");
    handle
        .deliver(
            firing,
            Answer::choice("ok").for_question(&second).to_control(),
        )
        .await;
    settle().await;
    advance(Duration::from_secs(3)).await;
    let report = run.await.expect("the run task");
    assert_eq!(status_of(&report, "stage").as_deref(), Some("success"));
    assert_replay_identical(&graph, &report);
}

/// A `HandlerManaged` node gets no driver timer: the step outlives the
/// budget's `timeout` and still finishes on its own terms.
#[tokio::test(start_paused = true)]
async fn a_handler_managed_node_gets_no_second_timer() {
    let dir = RunDir::new("budget-handler-managed");
    let graph = one_node(
        json!({ "work_before_ms": 5_000 }),
        Duration::from_secs(1),
        TimeoutPolicy::HandlerManaged,
    );
    let (driver, _asked) = driver(graph.clone(), &dir);
    let run = tokio::spawn(driver.run());
    settle().await;
    advance(Duration::from_secs(5)).await;
    let report = run.await.expect("the run task");
    assert_eq!(status_of(&report, "stage").as_deref(), Some("success"));
    assert_replay_identical(&graph, &report);
}

/// Cancellation stays live during a wait: the pending question is ended by
/// the cancel, not by any timer.
#[tokio::test(start_paused = true)]
async fn cancellation_ends_a_pending_wait() {
    let dir = RunDir::new("budget-cancel-pending");
    let graph = one_node(
        json!({ "questions": 1 }),
        Duration::from_secs(10),
        TimeoutPolicy::ExecutorEnforced,
    );
    let (driver, mut asked) = driver(graph.clone(), &dir);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    settle().await;
    next_question(&mut asked).await;
    advance(Duration::from_secs(60)).await;
    handle.cancel(CancelScopeId::ROOT).await;
    settle().await;
    let report = run.await.expect("the run task");
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(status_of(&report, "stage").as_deref(), Some("cancelled"));
    assert_replay_identical(&graph, &report);
}

/// Local resume: a firing re-dispatched after a crash asks again and gets a
/// fresh per-attempt budget; the wait it re-enters is excluded from active
/// time before any of it is charged.
#[tokio::test(start_paused = true)]
async fn a_redispatched_firing_gets_a_fresh_budget_and_waits_again() {
    let dir = RunDir::new("budget-resume");
    let graph = one_node(
        json!({ "work_before_ms": 2_000, "questions": 1, "work_after_ms": 3_000 }),
        Duration::from_secs(10),
        TimeoutPolicy::ExecutorEnforced,
    );
    // A complete run, to harvest a log to crash.
    let (driver, mut asked) = driver(graph.clone(), &dir);
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    settle().await;
    advance(Duration::from_secs(2)).await;
    let (firing, id) = next_question(&mut asked).await;
    handle
        .deliver(firing, Answer::choice("ok").for_question(&id).to_control())
        .await;
    settle().await;
    advance(Duration::from_secs(3)).await;
    let report = run.await.expect("the run task");
    assert_eq!(report.status, RunStatus::Success);

    // Crash right after the question was asked: the answer never landed.
    let question_seq = report
        .state
        .log
        .records()
        .iter()
        .find(|r| matches!(&r.event, Event::StepProgressRecorded { ev, .. } if Question::from_event(ev).is_some()))
        .map(|r| usize::try_from(r.seq).expect("fits"))
        .expect("the question is in the log");
    let prefix: Vec<EventRecord> = report.state.log.records()[..=question_seq].to_vec();
    let log = EventLog::try_from_records(engine::LOG_VERSION, prefix).expect("a valid prefix");

    let dir = RunDir::new("budget-resume-after");
    let (tx, mut asked) = mpsc::unbounded_channel();
    let (driver, _info) = Driver::resume(
        graph.clone(),
        log,
        Arc::new(NoExecutor::default()),
        registry(),
        Arc::new(MapSecrets::empty()),
        RunConfig::new(dir.path()).with_grace(Duration::from_millis(100)),
    )
    .expect("resumes");
    let driver = driver.observe(Arc::new(Asked(tx)));
    let handle = driver.handle();
    let run = tokio::spawn(driver.run());
    settle().await;
    // The redispatched attempt starts from scratch: 2 s of work, then it asks.
    advance(Duration::from_secs(2)).await;
    let (firing, id) = next_question(&mut asked).await;
    // A long wait before the answer costs nothing on the fresh budget.
    advance(Duration::from_secs(3600)).await;
    assert!(!run.is_finished());
    handle
        .deliver(firing, Answer::choice("ok").for_question(&id).to_control())
        .await;
    settle().await;
    advance(Duration::from_secs(3)).await;
    let resumed: ExecutionReport = run.await.expect("the run task");
    assert_eq!(
        resumed.status,
        RunStatus::Success,
        "{:?}",
        resumed.state.errors()
    );
    assert_eq!(status_of(&resumed, "stage").as_deref(), Some("success"));
    assert_replay_identical(&graph, &resumed);
}
