//! Handoff §2: run-scoped state readable by expressions. Written only inside
//! `apply`, in event order, and read only through `nodes.*` and `kv.*`.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use ir::{Arm, BinOp, Budget, GraphBuilder, JoinPolicy, Outcome, RunStatus, Value, validate};
use serde_json::json;
use support::{Harness, NOOP};

/// Handoff §7 test 3. An Attractor-style goal gate: the exit is blocked, the
/// run routes back to a repair target, the gate is satisfied on the second
/// pass, and the exit is taken. Entirely via guards on `nodes.*` — no engine
/// feature.
#[test]
fn a_goal_gate_routes_on_the_run_context() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let gate = b.add_step("gate", scope, NOOP);
    let repair = b.add_step("repair", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);

    // The gate reads the repair node's status straight out of the run context.
    let repaired = {
        let e = b.exprs();
        let status = e.path("nodes", &["repair", "status"]);
        let success = e.lit("success");
        e.binary(BinOp::Eq, status, success)
    };

    b.link(start, gate);
    b.set_join(gate, JoinPolicy::Any);
    b.select(gate, vec![Arm::when(exit, repaired), Arm::always(repair)]);
    b.select(repair, vec![Arm::always(gate).with_back()]);
    b.set_budget(gate, Budget::looped(5));
    b.set_budget(repair, Budget::looped(5));
    b.set_budget(exit, Budget::looped(5));
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);

    assert_eq!(
        h.started,
        vec!["start", "gate", "repair", "gate", "exit"],
        "blocked, repaired, then through"
    );
    assert_eq!(h.start_count("exit"), 1);

    // The second pass is a new generation, reached over the back edge.
    let generations: Vec<u32> = h
        .state
        .history()
        .iter()
        .filter(|r| r.name == "gate")
        .map(|r| r.generation.raw())
        .collect();
    assert_eq!(generations, vec![0, 1]);
    h.verify_replay();
}

/// Handoff §7 test 4. Two parallel nodes writing the same key: the last event
/// wins, and replay is identical.
#[test]
fn context_updates_merge_in_event_order() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let first = b.add_step("first", scope, NOOP);
    let second = b.add_step("second", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let report = b.add_step("report", scope, NOOP);

    // The collector reads the merged value back out of the run context.
    let shared = b.exprs().path("kv", &["shared"]);
    b.fan_out(start, &[first, second]);
    b.link(first, collect);
    b.link(second, collect);
    b.set_join(collect, JoinPolicy::All);
    b.select(collect, vec![Arm::always(report).with_map(shared)]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let seen = Rc::new(RefCell::new(Value::Null));
    let sink = seen.clone();
    let mut h = Harness::new(graph).respond_with(move |info| match info.base.as_str() {
        "first" => Outcome::success(Value::Null)
            .with_context_update("shared", "written by first")
            .with_context_update("only_first", 1),
        "second" => {
            Outcome::success(Value::Null).with_context_update("shared", "written by second")
        }
        "report" => {
            *sink.borrow_mut() = info.input();
            Outcome::success(Value::Null)
        }
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    // `first` finished first, so `second` wins the contested key.
    assert_eq!(
        h.state.run_context().get("shared"),
        Some(&json!("written by second"))
    );
    assert_eq!(h.state.run_context().get("only_first"), Some(&json!(1)));
    assert_eq!(*seen.borrow(), json!("written by second"));
    h.verify_replay();
}

/// Node records land under the instance name, so a matrix clone records
/// separately.
#[test]
fn clones_record_under_their_instance_name() {
    use ir::{ExpandTarget, collector_exprs, parallel_for_each};

    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let build = b.add_step("build", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, build);
    b.select(build, vec![
        Arm::always(collect).with_map(collector.indexed),
    ]);
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, build, items, ExpandTarget::Node, None, false);
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["x", "y"])),
        "build" => Outcome::success(json!(info.index)),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    let run = h.state.run_context();
    assert!(run.node("build#0").is_some());
    assert!(run.node("build#1").is_some());
    assert!(
        run.node("build").is_none(),
        "the superseded template never runs, so it records nothing"
    );
    assert_eq!(run.node("build#1").unwrap().output, json!(1));
}

/// A precondition's `success()` reads the run context, not the token payload.
#[test]
fn preconditions_read_upstream_status_from_the_run_context() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let build = b.add_step("build", scope, NOOP);
    let publish = b.add_step("publish", scope, NOOP);
    let cleanup = b.add_step("cleanup", scope, NOOP);
    b.link(build, publish);
    b.link(publish, cleanup);
    let (succeeded, whatever) = {
        let e = b.exprs();
        (e.call("success", vec![]), e.call("always", vec![]))
    };
    b.set_precondition(publish, succeeded);
    b.set_precondition(cleanup, whatever);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| {
        if info.base == "build" {
            Outcome::failure("compile error")
        } else {
            Outcome::success(Value::Null)
        }
    });
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.status_of("publish").as_deref(), Some("skipped"));
    assert_eq!(h.start_count("cleanup"), 1, "always() still runs");
}

/// The run context is derived state: replaying the log rebuilds it exactly.
#[test]
fn the_run_context_is_rebuilt_by_replay() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    let graph = b.build();

    let mut h = Harness::new(graph.clone()).respond_with(|info| {
        Outcome::success(json!(format!("output of {}", info.base)))
            .with_context_update("last", info.base.clone())
    });
    assert_eq!(h.run(), RunStatus::Success);

    let replayed = engine::replay(graph, &h.state.log);
    assert_eq!(
        replayed.run_context(),
        h.state.run_context(),
        "the run context comes back from the log alone"
    );
    assert_eq!(replayed.run_context().get("last"), Some(&json!("c")));
}

/// `success()` on an entry node: there are no real input edges, only the seed,
/// so the upstream fold has nothing to look up and evaluates true.
#[test]
fn success_on_an_entry_node_evaluates_true_off_the_seed() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let entry = b.add_step("entry", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(entry, after);
    let succeeded = b.exprs().call("success", vec![]);
    b.set_precondition(entry, succeeded);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(
        h.started,
        vec!["entry", "after"],
        "an entry node with a success() precondition still runs"
    );
}

/// `success()` on a matrix clone: a clone is seeded like an entry node, so its
/// own precondition evaluates true, and downstream nodes look the clone up by
/// its instance name.
#[test]
fn success_works_on_a_matrix_clone_and_downstream_of_one() {
    use ir::{BinOp, ExpandTarget, collector_exprs, parallel_for_each};

    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let build = b.add_step("build", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let report = b.add_step("report", scope, NOOP);

    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    let succeeded = b.exprs().call("success", vec![]);

    // The collector gates on one clone by instance name.
    let clone_ok = {
        let e = b.exprs();
        let status = e.path("nodes", &["build#1", "status"]);
        let want = e.lit("success");
        e.binary(BinOp::Eq, status, want)
    };

    b.link(plan, build);
    b.select(build, vec![
        Arm::always(collect).with_map(collector.indexed),
    ]);
    b.set_join(collect, JoinPolicy::All);
    b.select(collect, vec![Arm::when(report, clone_ok)]);
    // Every clone carries this precondition; each is seeded, so each evaluates
    // true.
    b.set_precondition(build, succeeded);
    parallel_for_each(&mut b, build, items, ExpandTarget::Node, None, false);
    let graph = b.build();

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b"])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    assert_eq!(
        h.start_count("build"),
        2,
        "success() on a seeded clone evaluates true"
    );
    assert_eq!(
        h.start_count("report"),
        1,
        "the instance-name lookup found build#1"
    );
    assert!(h.state.run_context().node("build#1").is_some());
}
