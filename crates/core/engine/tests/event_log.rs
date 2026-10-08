//! §5/§8: the event log, determinism, and the seams reserved for v2.

mod support;

use engine::{Command, EngineState, Event, LOG_VERSION, apply};
use ir::{
    Control, Digest, GraphBuilder, JoinPolicy, Outcome, RunStatus, StepKind, StepKindId, Value,
    validate,
};
use serde_json::json;
use support::{Harness, NOOP};

fn diamond() -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let join = b.add_step("join", scope, NOOP);
    b.fan_out(start, &[left, right]);
    b.link(left, join);
    b.link(right, join);
    b.set_join(join, JoinPolicy::All);
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// Every event is logged before it is applied, including the tokens routing
/// emits.
#[test]
fn routing_tokens_appear_in_the_log() {
    let mut h = Harness::new(diamond());
    assert_eq!(h.run(), RunStatus::Success);

    let tokens = h
        .state
        .log
        .events()
        .filter(|e| matches!(e, Event::TokenEmitted { .. }))
        .count();
    // One seed, two from the fan-out, two into the join.
    assert_eq!(tokens, 5);
    assert!(
        matches!(
            h.state.log.events().next(),
            Some(Event::ExecutionStarted { .. })
        ),
        "the compatibility spelling normalizes before the append"
    );
    assert_eq!(h.state.log.version(), LOG_VERSION);

    // Sequence numbers are dense and in order.
    let seqs: Vec<u64> = h.state.log.records().iter().map(|r| r.seq).collect();
    assert_eq!(seqs, (0..seqs.len() as u64).collect::<Vec<_>>());
}

/// `apply` is deterministic: the same events produce the same log every time.
#[test]
fn the_same_run_produces_the_same_log() {
    let first = {
        let mut h = Harness::new(diamond());
        h.run();
        h.state.log.clone()
    };
    let second = {
        let mut h = Harness::new(diamond());
        h.run();
        h.state.log.clone()
    };
    assert_eq!(first, second);
}

/// The log round-trips through serde, which is what event sourcing needs.
#[test]
fn the_log_round_trips_through_serde() {
    let mut h = Harness::new(diamond());
    h.run();
    let encoded = serde_json::to_string(&h.state.log).expect("encode");
    let decoded: engine::EventLog = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, h.state.log);
}

/// The whole state round-trips too, so a run can be written down and read back.
/// Stopped mid-run, with a token still waiting on the join, because an empty
/// pending set would not exercise how tokens are stored.
#[test]
fn engine_state_round_trips_through_serde() {
    let mut h = Harness::new(diamond());
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(json!({"ok": true})));

    let branches = h.take_starts();
    h.finish(branches[0].0, Outcome::success(json!("left")));
    assert_eq!(h.state.pending_count(), 1, "one token waits at the join");

    let encoded = serde_json::to_string(&h.state).expect("encode");
    let decoded: EngineState = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, h.state);

    // The waiting token survives, at the same (node, generation).
    let (before, after) = (
        h.state.pending_tokens().next().unwrap(),
        decoded.pending_tokens().next().unwrap(),
    );
    assert_eq!(before.0, after.0);
    assert_eq!(before.1, after.1);
}

/// The run finishes exactly once, at quiescence.
#[test]
fn the_run_finishes_once() {
    let mut h = Harness::new(diamond());
    assert_eq!(h.run(), RunStatus::Success);
    let finishes = h
        .commands
        .iter()
        .filter(|c| matches!(c, Command::FinishExecution { .. }))
        .count();
    assert_eq!(finishes, 1);
    assert!(h.state.is_finished());
    assert!(h.state.is_quiescent());
}

/// Events that arrive out of order are recorded as run errors, not panics.
#[test]
fn out_of_order_events_are_errors_not_panics() {
    let state = EngineState::new(diamond());
    let (state, commands) = apply(state, Event::StepFinished {
        firing:  ir::FiringId::new(99),
        attempt: ir::Attempt::FIRST,
        outcome: Outcome::success(Value::Null),
    });
    assert!(commands.is_empty());
    assert_eq!(state.errors(), &[engine::RunError::NotStarted]);

    let (state, _) = apply(state, Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let (state, _) = apply(state, Event::StepFinished {
        firing:  ir::FiringId::new(99),
        attempt: ir::Attempt::FIRST,
        outcome: Outcome::success(Value::Null),
    });
    assert!(
        state
            .errors()
            .contains(&engine::RunError::UnknownFiring(ir::FiringId::new(99)))
    );
}

/// §8: `Control` is non-exhaustive from day one, so `Pause` / `Steer` /
/// `Approve` can land without a breaking change.
#[test]
fn control_is_non_exhaustive() {
    let ctl = Control::Cancel;
    let described = match ctl {
        Control::Cancel => "cancel",
        _ => "something new",
    };
    assert_eq!(described, "cancel");
}

/// §8: `StepKind::fingerprint` defaults to `None` — nothing is cached in v1.
#[test]
fn fingerprint_defaults_to_none() {
    struct Plain;
    impl StepKind for Plain {
        fn id(&self) -> StepKindId {
            StepKindId::new("plain")
        }
        #[expect(
            clippy::unnecessary_literal_bound,
            reason = "the trait fixes this signature; an impl cannot widen the returned lifetime"
        )]
        fn name(&self) -> &str {
            "plain"
        }
    }
    struct Cacheable;
    impl StepKind for Cacheable {
        fn id(&self) -> StepKindId {
            StepKindId::new("cacheable")
        }
        #[expect(
            clippy::unnecessary_literal_bound,
            reason = "the trait fixes this signature; an impl cannot widen the returned lifetime"
        )]
        fn name(&self) -> &str {
            "cacheable"
        }
        fn fingerprint(&self, config: &Value) -> Option<Digest> {
            Some(Digest::new(&config.to_string()))
        }
    }

    assert_eq!(Plain.fingerprint(&json!({"a": 1})), None);
    assert_eq!(
        Cacheable.fingerprint(&json!({"a": 1})),
        Some(Digest::new(r#"{"a":1}"#))
    );
}

/// Step progress is observation only: it changes no coordination state.
#[test]
fn step_progress_changes_nothing() {
    let mut h = Harness::new(diamond());
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    let before = h.state.pending_count();
    h.commands.clear();
    h.feed(Event::StepProgressRecorded {
        firing: starts[0].0,
        ev:     ir::StepEvent::Log {
            stream: ir::LogStream::Stdout,
            line:   "building".into(),
        },
    });
    assert_eq!(h.state.pending_count(), before);
    assert!(h.commands.is_empty(), "progress produces no commands");
    assert!(
        h.state
            .log
            .events()
            .any(|e| matches!(e, Event::StepProgressRecorded { .. })),
        "it is still recorded"
    );
}

/// v2 records carry provenance. Replay feeds back only the external ones.
#[test]
fn records_say_whether_they_came_from_outside_or_from_the_core() {
    let mut h = Harness::new(diamond());
    assert_eq!(h.run(), RunStatus::Success);

    let external: Vec<&Event> = h.state.log.external_events().collect();
    assert!(
        external
            .iter()
            .all(|e| !matches!(e, Event::TokenEmitted { .. } | Event::NodeExpanded { .. })),
        "routed tokens and splices are the core's own"
    );
    assert!(matches!(
        external.first(),
        Some(Event::ExecutionStarted { .. })
    ));

    let core_records = h
        .state
        .log
        .records()
        .iter()
        .filter(|r| r.origin == engine::EventOrigin::Core)
        .count();
    assert!(core_records > 0, "routing produced events of its own");
    assert_eq!(
        core_records + external.len(),
        h.state.log.len(),
        "every record is one or the other"
    );
}

/// Replaying the external records reproduces the log byte for byte.
#[test]
fn replay_is_byte_identical() {
    let graph = diamond();
    let mut h = Harness::new(graph.clone());
    assert_eq!(h.run(), RunStatus::Success);

    let replayed = engine::verify_replay(graph, &h.state.log).expect("byte-identical");
    assert_eq!(replayed.folded_status(), RunStatus::Success);
    assert_eq!(replayed.history(), h.state.history());
}

/// Replay rebuilds a spliced graph from the events, rather than reading back a
/// mutated one.
#[test]
fn replay_reproduces_an_expansion() {
    use ir::{Arm, ExpandTarget, JoinPolicy, collector_exprs, parallel_for_each};

    let mut b = ir::GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let work = b.add_step("work", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, work);
    b.select(work, vec![Arm::always(collect).with_map(collector.indexed)]);
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, work, items, ExpandTarget::Node, None, false);
    let graph = b.build();

    let mut h = Harness::new(graph.clone()).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b", "c"])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    let replayed = engine::verify_replay(graph, &h.state.log).expect("byte-identical");
    assert_eq!(
        replayed.graph().nodes.len(),
        h.state.graph().nodes.len(),
        "the clones are re-derived, not copied"
    );
}

/// A v1 log is rejected cleanly rather than half-understood.
#[test]
fn an_old_log_version_is_rejected() {
    let mut h = Harness::new(diamond());
    h.run();
    let encoded = serde_json::to_string(&h.state.log).expect("encode");
    let current = format!("\"version\":{LOG_VERSION}");
    assert!(encoded.contains(&current));

    // Each bump changed the record shape or its replay semantics (v3: cancelled
    // outcomes route; v4: `Node.meta` inside splices; v5: `ControlRequested`;
    // v6: `Outcome.splices` and `Node.splice_policy`; v7: durable admission and
    // routing decisions; v8: cancellation groups; v9: a recording time beside
    // every persisted record; v10: `<subject>.<verb>` record names; v11: the
    // scope records; v12: a partial success's whole underlying failure), so an
    // old log is rejected cleanly rather than replayed
    // under rules it was not written for.
    for old in 1..LOG_VERSION {
        let downgraded = encoded.replacen(&current, &format!("\"version\":{old}"), 1);
        let error = serde_json::from_str::<engine::EventLog>(&downgraded)
            .expect_err("an old log must not deserialize");
        assert!(
            error.to_string().contains(&format!("version {old}")),
            "the error names the version it found: {error}"
        );
    }
}

/// The whole state still round-trips, now including the run context.
#[test]
fn state_round_trip_carries_the_run_context() {
    let mut h = Harness::new(diamond());
    h.run();
    let encoded = serde_json::to_string(&h.state).expect("encode");
    let decoded: EngineState = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded.run_context(), h.state.run_context());
    assert_eq!(decoded.log, h.state.log);
}
