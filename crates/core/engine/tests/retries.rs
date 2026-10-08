//! Handoff §1: retries add an attempt dimension. A retry is not a loop
//! iteration — it advances `Attempt` and never touches `Generation`.

mod support;

use std::time::Duration;

use engine::{Command, Event};
use ir::{
    Arm, Attempt, Budget, GraphBuilder, JoinPolicy, Outcome, RetryOn, RetryPolicy, RunStatus,
    StatusKind, Value, sequential_for_each, validate,
};
use serde_json::json;
use support::{Harness, NOOP};

/// Handoff §7 test 1. A node fails twice then succeeds under `max_attempts: 3`:
/// routing fires once, on the final outcome; the log shows three attempts;
/// replay is byte-identical.
#[test]
fn a_node_that_fails_twice_then_succeeds_routes_once() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let flaky = b.add_step("flaky", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(flaky, after);
    b.node_mut(flaky).retry = RetryPolicy::attempts(3);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.base == "flaky" && info.attempt.raw() < 3 {
            Outcome::failure(format!("attempt {} failed", info.attempt))
        } else {
            Outcome::success(json!({ "attempt": info.attempt.raw() }))
        }
    });
    assert_eq!(h.run(), RunStatus::Success);

    assert_eq!(h.start_count("flaky"), 3, "three attempts ran");
    assert_eq!(h.start_count("after"), 1, "routing fired exactly once");

    // Only the final attempt is recorded. Intermediate failures never reach the
    // history, so they never fold the run to failed.
    let records: Vec<_> = h
        .state
        .history()
        .iter()
        .filter(|r| r.name == "flaky")
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].attempt, Attempt::new(3));
    assert!(records[0].outcome.status.is_success_like());

    // The attempts are still visible in the run context and in the log.
    let record = h.state.run_context().node("flaky").expect("recorded");
    assert_eq!(record.attempts, 3);
    let finished = h
        .state
        .log
        .events()
        .filter(|e| matches!(e, Event::StepFinished { .. }))
        .count();
    assert_eq!(finished, 4, "three flaky attempts plus `after`");

    h.verify_replay();
}

/// A retry advances the attempt and leaves the generation alone.
#[test]
fn a_retry_is_not_a_loop_iteration() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let flaky = b.add_step("flaky", scope, NOOP);
    b.node_mut(flaky).retry = RetryPolicy::attempts(3);
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.attempt.raw() < 3 {
            Outcome::failure("not yet")
        } else {
            Outcome::success(Value::Null)
        }
    });
    assert_eq!(h.run(), RunStatus::Success);

    let attempts: Vec<u32> = h
        .state
        .log
        .events()
        .filter_map(|e| match e {
            Event::StepStarted { attempt, .. } => Some(attempt.raw()),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, vec![1, 2, 3]);
    assert_eq!(
        h.state.firing_count(flaky),
        1,
        "three attempts, still one firing"
    );
}

/// Handoff §7 test 2. A retried node on a back edge: the attempt counter resets
/// each generation, and the budget counts one firing per generation.
#[test]
fn attempts_reset_on_each_generation() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let work = b.add_step("work", scope, NOOP);
    let done = b.add_step("done", scope, NOOP);
    sequential_for_each(&mut b, plan, work, work, done, 10);
    b.node_mut(work).retry = RetryPolicy::attempts(2);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b", "c"])),
        // The first attempt of every iteration fails, the second succeeds.
        "work" if info.attempt == Attempt::FIRST => Outcome::failure("flaky"),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    // One recorded outcome per generation, each on its second attempt.
    let work_records: Vec<(u32, u32)> = h
        .state
        .history()
        .iter()
        .filter(|r| r.name == "work")
        .map(|r| (r.generation.raw(), r.attempt.raw()))
        .collect();
    assert_eq!(
        work_records,
        vec![(0, 2), (1, 2), (2, 2)],
        "the attempt counter starts over in every generation"
    );

    assert_eq!(
        h.state.firing_count(work),
        3,
        "the budget counts firings, not attempts"
    );
    assert_eq!(h.start_count("work"), 6, "two attempts per generation");
    h.verify_replay();
}

/// The core computes the backoff; the driver waits and adds jitter.
#[test]
fn the_core_schedules_a_deterministic_backoff() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let flaky = b.add_step("flaky", scope, NOOP);
    b.node_mut(flaky).retry = RetryPolicy::attempts(4).with_backoff(ir::Backoff {
        initial: Duration::from_millis(100),
        factor:  3.0,
        max:     Duration::from_secs(1),
        jitter:  true,
    });
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|_| Outcome::failure("always"));
    assert_eq!(h.run(), RunStatus::Failed);

    let delays: Vec<Duration> = h.scheduled_retries.iter().map(|(_, _, d)| *d).collect();
    assert_eq!(
        delays,
        vec![
            Duration::from_millis(100),
            Duration::from_millis(300),
            Duration::from_millis(900),
        ],
        "initial * factor^(n-1), and the 4th attempt is the last"
    );
    assert_eq!(h.start_count("flaky"), 4);
    h.verify_replay();
}

/// The backoff is capped, and the cap is reported exactly.
#[test]
fn the_backoff_is_capped() {
    let policy = RetryPolicy::attempts(10).with_backoff(ir::Backoff {
        initial: Duration::from_secs(1),
        factor:  10.0,
        max:     Duration::from_secs(30),
        jitter:  false,
    });
    assert_eq!(policy.base_delay(Attempt::new(1)), Duration::from_secs(1));
    assert_eq!(policy.base_delay(Attempt::new(2)), Duration::from_secs(10));
    assert_eq!(policy.base_delay(Attempt::new(3)), Duration::from_secs(30));
    assert_eq!(policy.base_delay(Attempt::new(9)), Duration::from_secs(30));
}

/// `retry_on` matches failure classes, which is how BuildKite's `exit_status`
/// and Attractor's `RETRY` outcome both lower.
#[test]
fn retry_on_matches_failure_classes() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let node = b.add_step("step", scope, NOOP);
    b.node_mut(node).retry = RetryPolicy::attempts(3)
        .with_retry_on(RetryOn::classes(&["exit_status:2", "retry_requested"]));
    let graph = b.build();

    // A class the policy does not list is not retried.
    let mut h = Harness::new(graph.clone()).respond_with(|_| {
        Outcome::new(
            ir::Status::Failure(ir::FailureInfo::exit_status(1)),
            Value::Null,
        )
    });
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.start_count("step"), 1, "exit_status:1 is not retryable");

    // A listed class is.
    let mut h = Harness::new(graph).respond_with(|info| {
        if info.attempt.raw() < 3 {
            Outcome::new(
                ir::Status::Failure(ir::FailureInfo::exit_status(2)),
                Value::Null,
            )
        } else {
            Outcome::success(Value::Null)
        }
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("step"), 3);
}

/// Attractor's first-class `RETRY` outcome lowers onto a failure class, not a
/// new `Status` variant. The status vocabulary stays closed.
#[test]
fn a_retry_request_is_a_failure_class_not_a_status() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let handler = b.add_step("handler", scope, NOOP);
    b.node_mut(handler).retry =
        RetryPolicy::attempts(2).with_retry_on(RetryOn::classes(&["retry_requested"]));
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.attempt == Attempt::FIRST {
            Outcome::new(
                ir::Status::Failure(
                    ir::FailureInfo::new("handler asked to run again")
                        .with_class("retry_requested"),
                ),
                Value::Null,
            )
        } else {
            Outcome::success(json!("second pass"))
        }
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("handler"), 2);
    assert_eq!(h.output("handler"), json!("second pass"));
}

/// `on_exhaustion: AcceptPartial` turns an exhausted failure into
/// `PartialSuccess`, carrying the real failure so the log stays truthful.
#[test]
fn exhausted_retries_can_accept_a_partial_success() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let node = b.add_step("step", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(node, after);
    b.node_mut(node).retry = RetryPolicy::attempts(2).accepting_partial();
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.base == "step" {
            Outcome::new(
                ir::Status::Failure(ir::FailureInfo::exit_status(7)),
                Value::Null,
            )
        } else {
            Outcome::success(Value::Null)
        }
    });
    assert_eq!(
        h.run(),
        RunStatus::Success,
        "a partial success is not a run failure"
    );
    assert_eq!(h.start_count("step"), 2, "both attempts ran");
    assert_eq!(h.start_count("after"), 1, "routing continued");

    let record = h.state.history().iter().find(|r| r.name == "step").unwrap();
    assert_eq!(record.outcome.status.tag(), "partial_success");
    let underlying = record
        .outcome
        .status
        .failure_info()
        .expect("the real failure is kept");
    assert_eq!(underlying.class, "exit_status:7");
}

/// §3.1 rule 3 holds for a timeout too: an exhausted timeout accepted as a
/// partial success keeps the timeout, though it has no failure info. Before
/// log v12 the record said `partial_success` with nothing underneath.
#[test]
fn an_exhausted_timeout_accepted_as_partial_keeps_the_timeout() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let node = b.add_step("step", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(node, after);
    b.node_mut(node).retry = RetryPolicy::attempts(2).accepting_partial();
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.base == "step" {
            Outcome::new(ir::Status::TimedOut, Value::Null)
        } else {
            Outcome::success(Value::Null)
        }
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("step"), 2, "the timeout was retried once");

    let record = h.state.history().iter().find(|r| r.name == "step").unwrap();
    assert_eq!(record.outcome.status, ir::Status::PartialSuccess {
        underlying: Some(ir::UnderlyingFailure::TimedOut),
    });
    h.verify_replay();
}

/// A cancelled firing is never retried: cancelling means stop, not start again.
#[test]
fn a_cancelled_firing_is_not_retried() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let node = b.add_step("step", scope, NOOP);
    b.node_mut(node).retry = RetryPolicy::attempts(5);
    let graph = b.build();

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(ir::CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::failure("interrupted"));

    assert!(
        !h.commands
            .iter()
            .any(|c| matches!(c, Command::ScheduleRetry { .. })),
        "no retry is scheduled"
    );
    assert_eq!(h.status, Some(RunStatus::Cancelled));
}

/// A firing waiting out its backoff keeps the run alive and its scope held.
#[test]
fn a_backoff_holds_the_run_open() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let node = b.add_step("step", scope, NOOP);
    b.node_mut(node).retry = RetryPolicy::attempts(2);
    b.set_budget(node, Budget::once());
    let graph = b.build();

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.feed(Event::StepStarted {
        firing:  starts[0].0,
        attempt: Attempt::FIRST,
    });
    h.feed(Event::StepFinished {
        firing:  starts[0].0,
        attempt: Attempt::FIRST,
        outcome: Outcome::failure("first try"),
    });

    assert!(h.status.is_none(), "the run has not finished");
    assert!(!h.state.is_quiescent(), "a backoff is not quiescence");
    assert_eq!(h.state.awaiting_retry().count(), 1);
    assert_eq!(h.state.held_scopes().count(), 1, "the scope stays held");

    h.drain_retries();
    let retry_starts = h.take_starts();
    assert_eq!(retry_starts.len(), 1);
    h.finish(retry_starts[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Success));
}

/// Success-like statuses are never retried; the test goes through
/// `Status::is_success_like`, the single classification point.
#[test]
fn success_like_statuses_are_never_retried() {
    let policy = RetryPolicy::attempts(5).with_retry_on(RetryOn::statuses(vec![
        StatusKind::Failure,
        StatusKind::PartialSuccess,
    ]));
    assert!(
        !policy.should_retry(&ir::Status::partial_clean()),
        "even an explicit PartialSuccess entry cannot make a success-like status retryable"
    );
    assert!(!policy.should_retry(&ir::Status::Success));
    assert!(policy.should_retry(&ir::Status::failure("nope")));
}

/// A guarded loop that also retries still terminates on its firing budget.
#[test]
fn budget_still_bounds_a_retrying_loop() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let spin = b.add_step("spin", scope, NOOP);
    let never = b.add_step("never", scope, NOOP);
    b.link(start, spin);
    b.set_join(spin, JoinPolicy::Any);
    b.set_budget(spin, Budget::looped(3));
    b.node_mut(spin).retry = RetryPolicy::attempts(2);
    let always_loop = b.exprs().lit(true);
    b.select(spin, vec![
        Arm::when(spin, always_loop).with_back(),
        Arm::always(never),
    ]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.state.firing_count(spin), 3, "three firings, then the cap");
    h.verify_replay();
}

/// `AcceptPartial` with `max_attempts: 1`: exhaustion means "no attempts
/// remain", which with one attempt is immediate. Reading it otherwise would
/// make `allow_partial` conditional on unrelated retry configuration.
///
/// The conversion carries the real failure, and the exhausted attempt's
/// `context_updates` are merged — that outcome is the final one.
#[test]
fn accept_partial_applies_with_no_retries_configured() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let step = b.add_step("step", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(step, after);
    b.node_mut(step).retry = RetryPolicy::attempts(1).accepting_partial();
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.base == "step" {
            Outcome::new(
                ir::Status::Failure(ir::FailureInfo::exit_status(4)),
                json!("partial output"),
            )
            .with_context_update("wrote", "on the exhausted attempt")
        } else {
            Outcome::success(Value::Null)
        }
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("step"), 1, "one attempt, no retry");
    assert_eq!(h.start_count("after"), 1, "routing continued");

    let record = h.state.history().iter().find(|r| r.name == "step").unwrap();
    assert_eq!(record.outcome.status.tag(), "partial_success");
    let underlying = record
        .outcome
        .status
        .failure_info()
        .expect("log truth: the real failure is kept");
    assert_eq!(underlying.class, "exit_status:4");
    assert_eq!(underlying.message, "step exited with status 4");
    assert_eq!(record.outcome.output, json!("partial output"));

    // The final outcome's updates reach the routing-visible store.
    assert_eq!(
        h.state.run_context().get("wrote"),
        Some(&json!("on the exhausted attempt"))
    );
    h.verify_replay();
}

/// A retried attempt's `context_updates` never reach `kv`: retries are
/// invisible everywhere except the event log. The discarded attempt's writes
/// are still in its finish record, for tooling to read.
#[test]
fn a_discarded_attempts_updates_never_reach_the_run_context() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let step = b.add_step("step", scope, NOOP);
    b.node_mut(step).retry = RetryPolicy::attempts(2);
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.attempt == Attempt::FIRST {
            Outcome::failure("first try")
                .with_context_update("marker", "from the discarded attempt")
        } else {
            Outcome::success(Value::Null).with_context_update("marker", "from the final attempt")
        }
    });
    assert_eq!(h.run(), RunStatus::Success);

    assert_eq!(
        h.state.run_context().get("marker"),
        Some(&json!("from the final attempt")),
        "only the final attempt writes to the routing-visible store"
    );

    // The discarded attempt's updates are still on the record, in the log.
    let discarded = h
        .state
        .log
        .events()
        .find_map(|e| match e {
            Event::StepFinished {
                attempt, outcome, ..
            } if *attempt == Attempt::FIRST => Some(outcome.clone()),
            _ => None,
        })
        .expect("the first attempt is in the log");
    assert_eq!(
        discarded.context_updates.get("marker").map(|v| v.as_str()),
        Some(Some("from the discarded attempt"))
    );
}
