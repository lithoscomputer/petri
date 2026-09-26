//! §6a: sequential `for_each` is a cycle over back edges and generations.
//! The engine has no loop primitive.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    Arm, Budget, GraphBuilder, JoinPolicy, Outcome, RunStatus, StepRef, Value, sequential_for_each,
    validate,
};
use serde_json::json;
use support::{Harness, NOOP};

/// Ordered region deploys: one node, three generations, results accumulated in
/// order. No new IR, only back edges and generations.
#[test]
fn sequential_for_each_desugars_to_a_cycle() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let deploy = b.add_step("deploy", scope, NOOP);
    let report = b.add_step("report", scope, NOOP);

    let wiring = sequential_for_each(&mut b, plan, deploy, deploy, report, 10);
    // The step reads the current element straight out of the loop state.
    b.node_mut(deploy).step = StepRef::new(
        NOOP,
        json!({ "region": { EXPR_PLACEHOLDER_KEY: wiring.exprs.item.raw() } }),
    );
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["us-east", "us-west", "eu"])),
        "deploy" => {
            let region = info.config.get("region").cloned().unwrap_or(Value::Null);
            Outcome::success(json!(format!("deployed {}", region.as_str().unwrap_or(""))))
        }
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    assert_eq!(h.start_count("deploy"), 3, "one firing per region, in turn");
    assert_eq!(h.max_concurrent, 1, "iterations never overlap");

    let generations: Vec<u32> = h
        .state
        .history()
        .iter()
        .filter(|r| r.name == "deploy")
        .map(|r| r.generation.raw())
        .collect();
    assert_eq!(
        generations,
        vec![0, 1, 2],
        "each iteration is its own generation"
    );

    assert_eq!(h.start_count("report"), 1, "the loop exits once");
    assert_eq!(
        h.state
            .history()
            .iter()
            .find(|r| r.name == "report")
            .map(|r| r.outcome.status.tag()),
        Some("success")
    );
}

/// The exit edge's payload is the accumulated result array.
#[test]
fn the_loop_exit_carries_every_result_in_order() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let work = b.add_step("work", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let wiring = sequential_for_each(&mut b, plan, work, work, collect, 10);
    b.node_mut(work).step = StepRef::new(
        NOOP,
        json!({ "item": { EXPR_PLACEHOLDER_KEY: wiring.exprs.item.raw() } }),
    );
    let graph = b.build();
    validate(&graph).expect("valid");

    let seen = Rc::new(RefCell::new(Value::Null));
    let sink = seen.clone();
    let mut h = Harness::new(graph).respond_with(move |info| match info.base.as_str() {
        "plan" => Outcome::success(json!([2, 3, 4])),
        "work" => {
            let item = info.config.get("item").and_then(Value::as_i64).unwrap_or(0);
            Outcome::success(item * 10)
        }
        "collect" => {
            *sink.borrow_mut() = info.input();
            Outcome::success(Value::Null)
        }
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(*seen.borrow(), json!([20, 30, 40]));
}

/// `Budget.max_firings` is what caps a loop. A guard that never exits hits it
/// and fails the run instead of spinning.
#[test]
fn a_runaway_loop_stops_at_its_budget() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let spin = b.add_step("spin", scope, NOOP);
    let never = b.add_step("never", scope, NOOP);
    b.link(start, spin);
    b.set_join(spin, JoinPolicy::Any);
    b.set_budget(spin, Budget::looped(4));
    let always_loop = b.exprs().lit(true);
    b.select(spin, vec![
        Arm::when(spin, always_loop).with_back(),
        Arm::always(never),
    ]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.start_count("spin"), 4, "stopped at the cap");
    assert_eq!(h.start_count("never"), 0);
    assert!(matches!(
        h.state.errors().first(),
        Some(engine::RunError::BudgetExceeded { max_firings: 4, .. })
    ));
}

/// A hierarchical `SubgraphStep` is deliberately rejected: iterations must stay
/// visible in the event log. Here they are, one `StepFinished` per iteration.
#[test]
fn every_iteration_appears_in_the_event_log() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let work = b.add_step("work", scope, NOOP);
    let done = b.add_step("done", scope, NOOP);
    sequential_for_each(&mut b, plan, work, work, done, 10);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b"])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    let finished = h
        .state
        .log
        .events()
        .filter(|e| matches!(e, engine::Event::StepFinished { .. }))
        .count();
    assert_eq!(finished, 4, "plan, work, work, done");
    assert_eq!(h.state.log.version(), engine::LOG_VERSION);
}

/// A token leaving a loop keeps the loop's generation, and a join matches
/// tokens of one generation. So an `All` join that meets a loop's exit and a
/// branch that skips the loop fires only when the loop exits in generation
/// 0: with two items the exit arrives in generation 1, the join waits
/// forever, and the run still reports success. Validation warns about the
/// shape (`lint.join_across_generations`).
#[test]
fn an_all_join_after_a_loop_matches_only_a_loop_that_never_iterated() {
    let run = |items: Value| {
        let mut b = GraphBuilder::new();
        let scope = ir::ScopeId::new(0);
        let start = b.add_step("start", scope, NOOP);
        let plan = b.add_step("plan", scope, NOOP);
        let work = b.add_step("work", scope, NOOP);
        let side = b.add_step("side", scope, NOOP);
        let report = b.add_step("report", scope, NOOP);
        b.fan_out(start, &[plan, side]);
        sequential_for_each(&mut b, plan, work, work, report, 10);
        b.link(side, report);
        b.set_join(report, JoinPolicy::All);
        let graph = b.build();
        validate(&graph).expect("valid");
        let warnings: Vec<&str> = ir::check(&graph)
            .warnings
            .iter()
            .map(ir::ValidationWarning::code)
            .collect();
        assert_eq!(warnings, ["lint.join_across_generations"]);

        let mut h = Harness::new(graph).respond_with(move |info| match info.base.as_str() {
            "plan" => Outcome::success(items.clone()),
            _ => Outcome::success(Value::Null),
        });
        let status = h.run();
        let waiting: Vec<u32> = h
            .state
            .pending_tokens()
            .filter(|((node, _), _)| *node == report)
            .map(|((_, generation), _)| generation.raw())
            .collect();
        (status, h, waiting)
    };

    let (status, h, waiting) = run(json!(["a"]));
    assert_eq!(status, RunStatus::Success);
    assert_eq!(
        h.start_count("report"),
        1,
        "one item: the loop exits in generation 0"
    );
    assert!(waiting.is_empty());

    let (status, h, waiting) = run(json!(["a", "b"]));
    assert_eq!(
        status,
        RunStatus::Success,
        "nothing failed, so the run reports success"
    );
    assert_eq!(h.start_count("work"), 2);
    assert_eq!(h.start_count("report"), 0, "report never runs");
    assert_eq!(
        waiting,
        vec![0, 1],
        "the side branch waits in generation 0, the loop's exit in generation 1"
    );
}
