//! §6: parallel `for_each` splices clones into the live graph. Each clone gets
//! `item` and `index`; each adds its own edge into the collector.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    Arm, CancelScopeId, ExpandTarget, GraphBuilder, JoinPolicy, Outcome, RunStatus, StepRef, Value,
    collector_exprs, parallel_for_each, validate,
};
use serde_json::json;
use support::{Harness, NOOP};

/// Build: plan -> deploy (expanded per region) -> collect.
fn matrix_graph(max_parallel: Option<u32>, fail_fast: bool) -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let deploy = b.add_step("deploy", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);

    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, deploy);
    b.select(deploy, vec![
        Arm::always(collect).with_map(collector.indexed),
    ]);
    b.select(collect, vec![]);
    b.node_mut(collect).routing = ir::Routing::terminal();
    b.set_join(collect, JoinPolicy::All);

    let item = b.exprs().var("item");
    b.node_mut(deploy).step = StepRef::new(
        NOOP,
        json!({ "region": { EXPR_PLACEHOLDER_KEY: item.raw() } }),
    );
    b.set_meta(deploy, json!({ "label": "Deploy" }));

    parallel_for_each(
        &mut b,
        deploy,
        items,
        ExpandTarget::Node,
        max_parallel,
        fail_fast,
    );
    b.build()
}

/// Every element gets a clone, and the collector's `All` join counts the edges
/// the splice added.
#[test]
fn for_each_clones_the_node_and_the_collector_waits_for_all_of_them() {
    let graph = matrix_graph(None, false);
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["us-east", "us-west", "eu"])),
        "deploy" => Outcome::success(info.config.get("region").cloned().unwrap_or(Value::Null)),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    assert_eq!(h.start_count("deploy"), 3);
    assert_eq!(h.max_concurrent, 3, "clones run concurrently by default");
    assert_eq!(h.start_count("collect"), 1);
    assert_eq!(h.started, vec![
        "plan", "deploy#0", "deploy#1", "deploy#2", "collect"
    ]);

    // A splice clones the node wholesale, so a clone carries the template's `meta`.
    let clone = h
        .state
        .graph()
        .nodes
        .iter()
        .find(|n| n.name == "deploy#1")
        .expect("the clone is in the live graph");
    assert_eq!(clone.meta, json!({ "label": "Deploy" }));
}

/// `index` and `item` are bound inside each clone, so the collector can put the
/// results back in order.
#[test]
fn clones_carry_item_and_index_and_the_collector_orders_results() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let build = b.add_step("build", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let report = b.add_step("report", scope, NOOP);

    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    let item = b.exprs().var("item");
    b.link(plan, build);
    b.select(build, vec![
        Arm::always(collect).with_map(collector.indexed),
    ]);
    b.select(collect, vec![
        Arm::always(report).with_map(collector.ordered),
    ]);
    b.set_join(collect, JoinPolicy::All);
    b.node_mut(build).step = StepRef::new(
        NOOP,
        json!({ "target": { EXPR_PLACEHOLDER_KEY: item.raw() } }),
    );
    parallel_for_each(&mut b, build, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let seen = Rc::new(RefCell::new(Value::Null));
    let sink = seen.clone();
    let mut h = Harness::new(graph).respond_with(move |info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["alpha", "beta", "gamma"])),
        "build" => Outcome::success(json!(format!(
            "built {}",
            info.config
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or("")
        ))),
        "report" => {
            *sink.borrow_mut() = info.input();
            Outcome::success(Value::Null)
        }
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(
        *seen.borrow(),
        json!(["built alpha", "built beta", "built gamma"]),
        "results come back in items order, not completion order"
    );
}

/// `max_parallel` is admission control across the spliced clones.
#[test]
fn max_parallel_limits_how_many_clones_run_at_once() {
    let graph = matrix_graph(Some(2), false);
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b", "c", "d", "e"])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("deploy"), 5, "every element still runs");
    assert_eq!(h.max_concurrent, 2, "never more than two at a time");
}

/// `fail_fast` cancels the sibling clones through the splice's own cancel
/// scope.
#[test]
fn fail_fast_cancels_the_sibling_clones() {
    let graph = matrix_graph(None, true);
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b", "c"])),
        "deploy" if info.index == Some(0) => Outcome::failure("boom"),
        // The siblings honour the cancel. Their outcomes route (§5), but a token
        // from a cancelled firing admits only `run_on_cancel` nodes, so the
        // un-flagged collector still never starts.
        _ => Outcome::cancelled(),
    });
    assert_eq!(h.run(), RunStatus::Failed);

    let cancels = h
        .commands
        .iter()
        .filter(|c| matches!(c, engine::Command::DeliverControl { .. }))
        .count();
    assert_eq!(cancels, 2, "the two live siblings are cancelled");
    assert_eq!(h.start_count("collect"), 0, "the collector never fires");
    assert_eq!(
        h.status_of("collect").as_deref(),
        Some("cancelled"),
        "it completes without running instead"
    );

    // The splice made its own cancel scope, nested under the root.
    let splice = h.state.splices().first().expect("one splice");
    assert_ne!(splice.cancel_scope, CancelScopeId::ROOT);
    assert_eq!(
        h.state.cancel_scope(splice.cancel_scope).unwrap().parent,
        Some(CancelScopeId::ROOT)
    );
    assert!(h.state.cancel_scope(splice.cancel_scope).unwrap().cancelled);
}

/// A clone whose node tolerates failure never pulls its siblings down: the
/// failed leg records `failure`, the others run to completion, the collector
/// fires, and the run folds to `Success` — GHA's job-level `continue-on-error`
/// under a matrix.
#[test]
fn a_tolerated_clone_failure_does_not_trigger_fail_fast() {
    let mut graph = matrix_graph(None, true);
    graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == "deploy")
        .unwrap()
        .tolerates_failure = true;
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b", "c"])),
        "deploy" if info.index == Some(0) => Outcome::failure("boom"),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    let cancels = h
        .commands
        .iter()
        .filter(|c| matches!(c, engine::Command::DeliverControl { .. }))
        .count();
    assert_eq!(cancels, 0, "no sibling is cancelled");
    assert_eq!(h.start_count("deploy"), 3);
    assert_eq!(h.start_count("collect"), 1, "the collector fires normally");
    assert_eq!(h.status_of("deploy#0").as_deref(), Some("failure"));
}

/// The other half of the `fail_fast` rule: a collector marked `run_on_cancel`
/// fires and gathers the partial results — the failed leg's, and the cancelled
/// siblings' — instead of never firing.
#[test]
fn fail_fast_still_fires_a_marked_collector() {
    let mut graph = matrix_graph(None, true);
    graph
        .body
        .nodes
        .iter_mut()
        .find(|n| n.name == "collect")
        .unwrap()
        .run_on_cancel = true;
    validate(&graph).expect("valid");

    let seen = Rc::new(RefCell::new(0usize));
    let sink = seen.clone();
    let mut h = Harness::new(graph).respond_with(move |info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b", "c"])),
        "deploy" if info.index == Some(0) => Outcome::failure("boom"),
        "collect" => {
            *sink.borrow_mut() = info.inputs.len();
            Outcome::success(Value::Null)
        }
        _ => Outcome::cancelled(),
    });
    assert_eq!(h.run(), RunStatus::Failed);
    assert_eq!(h.start_count("collect"), 1, "the marked collector fires");
    assert_eq!(*seen.borrow(), 3, "and sees every leg's token");
}

/// §5: a root cancel does not start `max_parallel`-deferred clones. They are
/// un-marked, so once the running clone settles they complete `Cancelled`
/// without a `StartStep` — deferral is not a back door around the admission
/// rule.
#[test]
fn a_cancel_does_not_start_deferred_unmarked_clones() {
    let graph = matrix_graph(Some(1), false);
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(engine::Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(json!(["a", "b", "c"])));
    let running = h.take_starts();
    assert_eq!(running.len(), 1, "max_parallel admits one clone");

    h.cancel(CancelScopeId::ROOT);
    h.finish(running[0].0, Outcome::cancelled());

    assert_eq!(h.take_starts(), vec![], "the deferred clones never start");
    assert_eq!(h.start_count("deploy"), 1);
    for leg in ["deploy#1", "deploy#2"] {
        assert_eq!(h.status_of(leg).as_deref(), Some("cancelled"), "{leg}");
    }
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

/// Splice semantics: a splice supersedes the whole template region, so a
/// template edge no token can cross stops counting toward a downstream join.
///
/// This is the regression test for a deadlock generator. `All` is defined over
/// the incoming edges a node has *at firing time*, so if the template's `test
/// -> collect` edge kept counting after the clones were spliced in, the
/// collector would wait forever on an edge nothing can ever cross.
#[test]
fn superseded_template_edges_do_not_deadlock_the_collector() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let setup = b.add_step("setup", scope, NOOP);
    let test = b.add_step("test", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);

    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, setup);
    b.link(setup, test);
    let template_edges = b.select(test, vec![Arm::always(collect).with_map(collector.indexed)]);
    let template_edge = template_edges[0];
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(
        &mut b,
        setup,
        items,
        ExpandTarget::Subgraph {
            entry: setup,
            exit:  test,
        },
        None,
        false,
    );
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!([1, 2])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);

    // The whole region is cloned, not just the node carrying the expansion.
    assert_eq!(h.start_count("setup"), 2);
    assert_eq!(h.start_count("test"), 2);
    assert_eq!(
        h.start_count("collect"),
        1,
        "the collector is not deadlocked"
    );

    // Every template node is superseded, entry and interior alike.
    assert!(h.state.is_superseded(setup), "the region entry");
    assert!(h.state.is_superseded(test), "the region exit");
    assert!(!h.state.is_superseded(collect), "the collector is outside");

    // The template edge is still in the graph — superseding is not deletion, so the
    // log and the graph still describe what was there.
    assert!(
        h.state.graph().edge(template_edge).is_some(),
        "the template edge is kept, only discounted"
    );
    assert!(h.state.is_quiescent());
    assert_eq!(h.state.pending_count(), 0, "no token is left stranded");
}

/// Expanding over an empty array leaves the collector with nothing to wait for
/// and no clone to run.
#[test]
fn an_empty_items_array_expands_to_nothing() {
    let graph = matrix_graph(None, false);
    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!([])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("deploy"), 0);
    assert_eq!(h.start_count("collect"), 0);
    assert!(h.state.is_quiescent());
}

/// `items` that is not an array is a run error, not a panic.
#[test]
fn non_array_items_fails_the_run() {
    let graph = matrix_graph(None, false);
    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!("not an array")),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Failed);
    assert!(matches!(
        h.state.errors().first(),
        Some(engine::RunError::ItemsNotArray { .. })
    ));
}

/// Two independent matrices whose entry nodes expand in the same cascade: each
/// splice's clones land at distinct ids, and every live node occupies the slot
/// its id names. (Regression: both expansions once allocated clone ids against
/// the same pre-splice graph length, and the second splice corrupted the live
/// graph.)
#[test]
fn two_expansions_in_one_cascade_get_distinct_ids() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let a_done = b.add_step("a_done", scope, NOOP);
    let z = b.add_step("z", scope, NOOP);
    let z_done = b.add_step("z_done", scope, NOOP);

    let collector = collector_exprs(b.exprs());
    let a_items = b.exprs().lit(json!(["x", "y"]));
    let z_items = b.exprs().lit(json!(["1", "2", "3"]));
    b.select(a, vec![Arm::always(a_done).with_map(collector.indexed)]);
    b.select(z, vec![Arm::always(z_done).with_map(collector.indexed)]);
    b.set_join(a_done, JoinPolicy::All);
    b.set_join(z_done, JoinPolicy::All);
    parallel_for_each(&mut b, a, a_items, ExpandTarget::Node, None, false);
    parallel_for_each(&mut b, z, z_items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("a"), 2);
    assert_eq!(h.start_count("z"), 3);
    for (index, node) in h.state.graph().nodes.iter().enumerate() {
        assert_eq!(
            node.id.index(),
            index,
            "node `{}` sits at its id",
            node.name
        );
    }
}

/// A template node routed by a `Tiered` policy: the clone's tiers must name
/// the clone's own arms, not the template's, or no clone ever emits and the
/// collector never fires.
#[test]
fn a_tiered_template_node_routes_its_clones() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let deploy = b.add_step("deploy", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, deploy);
    let ids = b.select(deploy, vec![
        Arm::always(collect).with_map(collector.indexed),
    ]);
    b.node_mut(deploy).routing.groups[0].policy = ir::SelectionPolicy::Tiered(vec![ir::Tier {
        candidates: vec![ir::Candidate {
            edge: ids[0],
            when: ir::Guard::Always,
            rank: None,
        }],
        pick:       ir::PickPolicy::First,
    }]);
    b.node_mut(collect).routing = ir::Routing::terminal();
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, deploy, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph).respond_with(|info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b"])),
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Success);
    assert_eq!(h.start_count("deploy"), 2);
    assert_eq!(
        h.start_count("collect"),
        1,
        "every clone's tier found its own arm"
    );
}

/// A `for_each` node's own join decides when the expansion happens; the
/// clones then start on their seeds without applying it again. With a quorum
/// of two over three branches, the clones start after the second branch, and
/// the third branch's token is dropped.
#[test]
fn a_quorum_on_the_for_each_node_decides_when_the_clones_start() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let branches: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|name| b.add_step(name, scope, NOOP))
        .collect();
    let deploy = b.add_step("deploy", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    b.fan_out(start, &branches);
    for branch in &branches {
        b.link(*branch, deploy);
    }
    b.link(deploy, collect);
    b.set_join(deploy, JoinPolicy::Quorum { n: 2 });
    let items = b.exprs().lit(json!(["x", "y"]));
    parallel_for_each(&mut b, deploy, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(engine::Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let started = h.take_starts();
    h.finish(started[0].0, Outcome::success(Value::Null));
    let branch_starts = h.take_starts();
    assert_eq!(branch_starts.len(), 3);

    h.finish(branch_starts[0].0, Outcome::success(Value::Null));
    assert!(h.take_starts().is_empty(), "one branch is not a quorum");
    h.finish(branch_starts[1].0, Outcome::success(Value::Null));
    let clones = h.take_starts();
    let names: Vec<&str> = clones.iter().map(|(_, name)| name.as_str()).collect();
    assert_eq!(
        names,
        ["deploy#0", "deploy#1"],
        "the second branch starts both clones"
    );

    h.finish(branch_starts[2].0, Outcome::success(Value::Null));
    assert!(
        h.take_starts().is_empty(),
        "the third branch's token is dropped"
    );
    for (firing, _) in clones {
        h.finish(firing, Outcome::success(Value::Null));
    }
    let collector = h.take_starts();
    assert_eq!(collector.len(), 1);
    h.finish(collector[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Success));
    h.verify_replay();
}
