//! fabro-51ad reproduction: a `nodes.<id>.generation` guard on a looping
//! node must see the loop counter advance, so a deadlock exit listed ahead
//! of the back edge can fire. Observed on the fabro line: three visits of
//! the failing node, guard `nodes.flaky.generation >= 2` never satisfied,
//! routing resolving empty, run ending `workflow_error`.

mod support;

use ir::{
    Arm, BinOp, Budget, GraphBuilder, Outcome, RunStatus, Value, validate,
};
use support::{Harness, NOOP};
use std::time::Duration;

fn flaky_loop_graph() -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let flaky = b.add_step("flaky", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    b.link(start, flaky);
    // A loop head joins Any: the next iteration must not wait for the
    // initial entry once the back edge is live.
    b.set_join(flaky, ir::JoinPolicy::Any);

    // max_visits=10, retries 0 — the probe-06 shape.
    b.node_mut(flaky).budget = Budget::new(10, Duration::from_secs(3600));

    let deadlock_guard = {
        let e = b.exprs();
        let generation = e.path("nodes", &["flaky", "generation"]);
        let two = e.lit(1); // > 1 === >= 2
        e.binary(BinOp::Gt, generation, two)
    };
    b.select(flaky, vec![
        Arm::when(exit, deadlock_guard),
        Arm::always(flaky).with_back(),
    ]);
    let mut graph = b.build();
    // The fabro rule: a routed failure is control flow; only the exit
    // node's record decides the run.
    graph.completion = ir::Completion::TerminalNode(exit);
    validate(&graph).expect("valid");
    graph
}

#[test]
fn a_generation_guard_on_a_looping_node_sees_the_counter_advance() {
    let graph = flaky_loop_graph();
    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "flaky" => Outcome::failure("always fails"),
        _ => Outcome::success(Value::Null),
    });
    let status = h.run();

    let generations: Vec<u32> = h
        .state
        .history()
        .iter()
        .filter(|r| r.name == "flaky")
        .map(|r| r.generation.raw())
        .collect();
    assert_eq!(generations, vec![0, 1, 2], "three visits, one per generation");

    assert_eq!(h.start_count("flaky"), 3, "the deadlock exit fires on visit 3");
    assert_eq!(status, RunStatus::Success, "the exit node decides the run");
    assert!(
        h.state.errors().is_empty(),
        "no engine errors: {:?}",
        h.state.errors()
    );
}
