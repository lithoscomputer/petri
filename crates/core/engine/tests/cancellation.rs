//! §5a: cancel scopes. A scope is a dynamic set of firings cancellable as a
//! unit. Cancellation is a state transition in the pure core; only signal
//! delivery is the host's job.
//!
//! Two tiers. Cancel is polite: outcomes route, and a node marked
//! `run_on_cancel` may still fire, so `always()`- and `cancelled()`-style
//! cleanup can run. Kill is forced: tokens drop, nothing routes, nothing is
//! admitted.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use engine::{Command, Event};
use ir::{
    Attempt, CancelScopeId, Control, ExpandTarget, FiringId, GraphBuilder, JoinPolicy, Outcome,
    RetryPolicy, RunStatus, Status, Value, collector_exprs, parallel_for_each, validate,
};
use serde_json::json;
use support::{Harness, NOOP};

#[test]
fn cancelling_one_expanded_group_preserves_the_other_clone() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let work = b.add_step("work", scope, NOOP);
    b.node_mut(work).cancel_group = Some(work);
    let items = b.exprs().lit(json!([1, 2]));
    parallel_for_each(&mut b, work, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid expansion");
    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    let first = h
        .state
        .graph()
        .nodes
        .iter()
        .find(|node| node.name == "work#0")
        .unwrap()
        .id;
    h.feed(Event::cancel_group(first));
    let controls: Vec<_> = h
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::DeliverControl {
                firing,
                ctl: Control::Cancel,
            } => Some(*firing),
            _ => None,
        })
        .collect();
    assert_eq!(controls, vec![
        starts.iter().find(|(_, name)| name == "work#0").unwrap().0
    ]);
    for (firing, name) in starts {
        h.finish(
            firing,
            if name == "work#0" {
                Outcome::cancelled()
            } else {
                Outcome::success(Value::Null)
            },
        );
    }
    assert_eq!(h.status_of("work#1").as_deref(), Some("success"));
    h.verify_replay();
}

#[test]
fn a_cancellation_group_cannot_cross_an_expansion_boundary() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let work = b.add_step("work", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(work, after);
    b.node_mut(work).cancel_group = Some(work);
    b.node_mut(after).cancel_group = Some(work);
    let items = b.exprs().lit(json!([1, 2]));
    parallel_for_each(&mut b, work, items, ExpandTarget::Node, None, false);
    assert!(
        validate(&b.build())
            .unwrap_err()
            .iter()
            .any(|error| error.code() == "validate.cancel_group_expansion")
    );
}

#[test]
fn targeted_cancellation_stops_a_group_and_preserves_other_branches() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let worker = b.add_step("worker", scope, NOOP);
    let pending = b.add_step("pending", scope, NOOP);
    let sibling = b.add_step("sibling", scope, NOOP);
    b.node_mut(worker).cancel_group = Some(worker);
    b.node_mut(pending).cancel_group = Some(worker);
    b.fan_out(start, &[worker, sibling]);
    b.link(worker, pending);
    let graph = b.build();
    validate(&graph).expect("valid groups");
    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let start = h.take_starts()[0].0;
    h.finish(start, Outcome::success(Value::Null));
    let running = h.take_starts();
    h.feed(Event::cancel_group(worker));
    let controls: Vec<_> = h
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::DeliverControl {
                firing,
                ctl: Control::Cancel,
            } => Some(*firing),
            _ => None,
        })
        .collect();
    let worker_firing = running.iter().find(|(_, name)| name == "worker").unwrap().0;
    assert_eq!(controls, vec![worker_firing]);
    assert!(!h.state.is_cancelled());
    for (firing, name) in running {
        h.finish(
            firing,
            if name == "worker" {
                Outcome::cancelled()
            } else {
                Outcome::success(Value::Null)
            },
        );
    }
    assert_eq!(h.start_count("pending"), 0);
    assert_eq!(h.status_of("pending").as_deref(), Some("cancelled"));
    assert_eq!(h.status_of("sibling").as_deref(), Some("success"));
    h.verify_replay();
}

#[test]
fn targeted_cancellation_settles_retry_backoff_without_another_attempt() {
    let mut b = GraphBuilder::new();
    let worker = b.add_step("worker", ir::ScopeId::new(0), NOOP);
    let after = b.add_step("after", ir::ScopeId::new(0), NOOP);
    b.node_mut(after).run_on_cancel = true;
    b.link(worker, after);
    b.node_mut(worker).cancel_group = Some(worker);
    b.node_mut(worker).retry = RetryPolicy::attempts(2);
    let mut h = Harness::new(b.build());
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let firing = h.take_starts()[0].0;
    h.finish(firing, Outcome::failure("retry"));
    h.feed(Event::cancel_group(worker));
    h.feed(Event::RetryElapsed {
        firing,
        next_attempt: Attempt::new(2),
    });
    assert_eq!(h.start_count("worker"), 1);
    assert_eq!(h.status_of("worker").as_deref(), Some("cancelled"));
    assert!(h.state.errors().is_empty(), "{:?}", h.state.errors());
    let after = h.take_starts()[0].0;
    h.finish(after, Outcome::success(Value::Null));
    h.verify_replay();
}

/// Cancelling the root scope cancels every live firing and drops every token.
#[test]
fn cancelling_the_root_scope_stops_the_run() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.fan_out(start, &[left, right]);
    b.link(left, after);
    b.link(right, after);
    b.set_join(after, JoinPolicy::All);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(Value::Null));

    let branches = h.take_starts();
    assert_eq!(branches.len(), 2);

    h.cancel(CancelScopeId::ROOT);
    let delivered: Vec<_> = h
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::DeliverControl { firing, ctl } => Some((*firing, ctl.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(delivered.len(), 2, "both live firings get the signal");
    assert!(delivered.iter().all(|(_, ctl)| *ctl == Control::Cancel));
    assert!(h.state.is_cancelled());

    // The host reports back; the run ends cancelled and `after` never runs.
    for (firing, _) in branches {
        h.finish(firing, Outcome::cancelled());
    }
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    assert_eq!(h.start_count("after"), 0);
    assert_eq!(h.state.pending_count(), 0);
}

/// A cancelled firing's outcome routes like any other (§5), but the token
/// cannot restart un-marked work: the downstream node completes `Cancelled`
/// without executing. Its record — which only routing could have produced — is
/// the proof the token flowed.
#[test]
fn a_cancelled_outcome_routes_but_does_not_restart_unmarked_work() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::cancelled());

    assert_eq!(h.start_count("c"), 0);
    assert_eq!(h.status_of("c").as_deref(), Some("cancelled"));
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

/// Splice scopes nest under the root, so cancelling the root reaches inside
/// them.
#[test]
fn cancelling_the_root_reaches_into_splice_scopes() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let work = b.add_step("work", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, work);
    b.select(work, vec![
        ir::Arm::always(collect).with_map(collector.indexed),
    ]);
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, work, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(json!(["a", "b", "c"])));

    let clones = h.take_starts();
    assert_eq!(clones.len(), 3);

    let splice_scope = h.state.splices().first().unwrap().cancel_scope;
    h.cancel(CancelScopeId::ROOT);
    assert!(
        h.state.cancel_scope(splice_scope).unwrap().cancelled,
        "the nested splice scope is cancelled too"
    );
    let delivered = h
        .commands
        .iter()
        .filter(|c| matches!(c, Command::DeliverControl { .. }))
        .count();
    assert_eq!(delivered, 3);
}

/// Cancelling one splice scope leaves work outside it alone.
#[test]
fn cancelling_a_splice_scope_spares_the_rest_of_the_run() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let matrix = b.add_step("matrix", scope, NOOP);
    let sibling = b.add_step("sibling", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.fan_out(plan, &[matrix, sibling]);
    b.select(matrix, vec![
        ir::Arm::always(collect).with_map(collector.indexed),
    ]);
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, matrix, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(json!(["a", "b"])));

    let running = h.take_starts();
    // Two clones plus the sibling branch.
    assert_eq!(running.len(), 3);

    let splice_scope = h.state.splices().first().unwrap().cancel_scope;
    h.cancel(splice_scope);
    let delivered = h
        .commands
        .iter()
        .filter(|c| matches!(c, Command::DeliverControl { .. }))
        .count();
    assert_eq!(delivered, 2, "only the clones are cancelled");
    assert!(!h.state.is_cancelled(), "the run itself is not cancelled");

    for (firing, name) in running {
        let outcome = if name.starts_with("matrix") {
            Outcome::cancelled()
        } else {
            Outcome::success(Value::Null)
        };
        h.finish(firing, outcome);
    }
    assert_eq!(h.status, Some(RunStatus::Success));
    assert_eq!(h.start_count("collect"), 0);
}

/// §5 test 1: after a cancel, a `run_on_cancel` node with an absent (or true)
/// precondition fires for real, while an un-flagged node between completes
/// `Cancelled` without anything being evaluated — even a precondition that
/// would error is never touched.
#[test]
fn run_on_cancel_admits_cleanup_and_unmarked_work_is_never_evaluated() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let between = b.add_step("between", scope, NOOP);
    let cleanup = b.add_step("cleanup", scope, NOOP);
    b.link(a, between);
    b.link(between, cleanup);
    // `between` is not marked; its precondition would error if evaluated, and the
    // rule says it must not be.
    let broken = b.exprs().var("no_such_binding");
    b.set_precondition(between, broken);
    b.node_mut(cleanup).run_on_cancel = true;
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::cancelled());

    assert_eq!(h.start_count("between"), 0);
    assert_eq!(h.status_of("between").as_deref(), Some("cancelled"));
    assert!(
        h.state.errors().is_empty(),
        "the broken precondition was never evaluated: {:?}",
        h.state.errors()
    );

    let cleanup_starts = h.take_starts();
    assert_eq!(
        cleanup_starts
            .iter()
            .map(|(_, n)| n.as_str())
            .collect::<Vec<_>>(),
        vec!["cleanup"],
        "the marked node fires for real"
    );
    h.finish(cleanup_starts[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

/// §5 test 2: a `run_on_cancel` node whose precondition errors keeps today's
/// behavior exactly — `RunError::Eval` plus a routed `Failure` — because
/// cancellation must not convert a broken expression into a clean cancellation.
#[test]
fn a_marked_nodes_broken_precondition_is_still_an_error() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let cleanup = b.add_step("cleanup", scope, NOOP);
    b.link(a, cleanup);
    let broken = b.exprs().var("no_such_binding");
    b.set_precondition(cleanup, broken);
    b.node_mut(cleanup).run_on_cancel = true;
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::cancelled());

    assert_eq!(h.start_count("cleanup"), 0);
    assert_eq!(h.status_of("cleanup").as_deref(), Some("failure"));
    assert!(
        h.state
            .errors()
            .iter()
            .any(|e| matches!(e, engine::RunError::Eval { .. })),
        "the evaluation error is on the record: {:?}",
        h.state.errors()
    );
    h.verify_replay();
}

/// §5: a `run_on_cancel` node whose gate is false records `Cancelled`, not
/// `Skipped` — matching GitHub's UI for unreached steps, and saying why it did
/// not run. Pins the upstream fold too: a cancelled upstream folds to
/// "cancelled", so the core `success()` guard is false and `cancelled()` true
/// over it.
#[test]
fn upstream_cancelled_folds_to_cancelled_and_a_false_gate_records_cancelled() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let on_success = b.add_step("on_success", scope, NOOP);
    let on_cancel = b.add_step("on_cancel", scope, NOOP);
    b.fan_out(a, &[on_success, on_cancel]);
    let success = b.exprs().call("success", vec![]);
    let cancelled = b.exprs().call("cancelled", vec![]);
    b.set_precondition(on_success, success);
    b.set_precondition(on_cancel, cancelled);
    b.node_mut(on_success).run_on_cancel = true;
    b.node_mut(on_cancel).run_on_cancel = true;
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::cancelled());

    // success() folded over the cancelled upstream is false: gate-false on a
    // marked node records Cancelled, not Skipped.
    assert_eq!(h.start_count("on_success"), 0);
    assert_eq!(h.status_of("on_success").as_deref(), Some("cancelled"));

    // cancelled() folded over the same upstream is true: the node runs.
    let cleanup = h.take_starts();
    assert_eq!(
        cleanup.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(),
        vec!["on_cancel"]
    );
    h.finish(cleanup[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

/// §5 test 3: an expansion node in a cancelled scope never expands. It
/// completes `Cancelled`, and nothing is spliced.
#[test]
fn a_cancelled_expansion_never_splices() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let work = b.add_step("work", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, work);
    b.select(work, vec![
        ir::Arm::always(collect).with_map(collector.indexed),
    ]);
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, work, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::success(json!(["a", "b", "c"])));

    assert!(h.state.splices().is_empty(), "no splice happened");
    assert!(
        !h.state
            .log
            .events()
            .any(|e| matches!(e, Event::NodeExpanded { .. })),
        "no NodeExpanded in the log"
    );
    assert_eq!(h.status_of("work").as_deref(), Some("cancelled"));
    assert_eq!(h.start_count("work"), 0);
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

/// §5 test 4: an `Always`-guarded back edge through a cancelled region
/// terminates by budget, as the `Skipped` cascade does — same envelope, no new
/// machinery. The test finishing is the termination proof. The refusal is
/// quiet: nothing would have run, so it is no engine error.
#[test]
fn a_back_edge_through_a_cancelled_region_stops_at_its_budget() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let spin = b.add_step("spin", scope, NOOP);
    let never = b.add_step("never", scope, NOOP);
    b.link(start, spin);
    b.set_join(spin, JoinPolicy::Any);
    b.set_budget(spin, ir::Budget::looped(4));
    let always_loop = b.exprs().lit(true);
    b.select(spin, vec![
        ir::Arm::when(spin, always_loop).with_back(),
        ir::Arm::always(never),
    ]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::cancelled());

    assert_eq!(h.start_count("spin"), 0, "nothing executed");
    let spins = h
        .state
        .history()
        .iter()
        .filter(|r| r.name == "spin")
        .count();
    assert_eq!(
        spins, 4,
        "one synthesized Cancelled per generation, then the cap"
    );
    assert_eq!(h.state.errors(), [], "a cancelled loop stops quietly");
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

/// The same loop in a cancel group: a group cancel stops it quietly at its
/// budget, and the rest of the run keeps its own status. A `BudgetExceeded`
/// here would fail the run although nothing failed.
#[test]
fn a_group_cancelled_loop_stops_quietly_at_its_budget() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let spin = b.add_step("spin", scope, NOOP);
    let sibling = b.add_step("sibling", scope, NOOP);
    let never = b.add_step("never", scope, NOOP);
    b.fan_out(start, &[spin, sibling]);
    b.node_mut(spin).cancel_group = Some(spin);
    b.set_join(spin, JoinPolicy::Any);
    b.set_budget(spin, ir::Budget::looped(4));
    let always_loop = b.exprs().lit(true);
    b.select(spin, vec![
        ir::Arm::when(spin, always_loop).with_back(),
        ir::Arm::always(never),
    ]);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let start = h.take_starts()[0].0;
    h.finish(start, Outcome::success(Value::Null));
    let running = h.take_starts();
    h.feed(Event::cancel_group(spin));
    for (firing, name) in running {
        h.finish(
            firing,
            if name == "spin" {
                Outcome::cancelled()
            } else {
                Outcome::success(Value::Null)
            },
        );
    }

    let spins: Vec<(String, String)> = h
        .statuses()
        .into_iter()
        .filter(|(name, _)| name == "spin")
        .collect();
    assert_eq!(spins.len(), 4, "the cancelled run, then three completions");
    assert!(spins.iter().all(|(_, status)| status == "cancelled"));
    assert_eq!(h.status_of("sibling").as_deref(), Some("success"));
    assert_eq!(h.state.errors(), [], "a cancelled loop stops quietly");
    assert_eq!(h.status, Some(RunStatus::Success));
    h.verify_replay();
}

/// §5 test 7: a cancelled route that leaves an `All` join unsatisfiable parks
/// its token forever — and the environment must not be parked with it. When the
/// run finishes, every held scope is released and the finished state claims
/// nothing.
#[test]
fn a_parked_token_does_not_hold_its_scope_past_the_finish() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let flaky = b.add_step("flaky", scope, NOOP);
    let steady = b.add_step("steady", scope, NOOP);
    let join = b.add_step("join", scope, NOOP);
    b.fan_out(start, &[flaky, steady]);
    // flaky routes onward only on success; a cancelled outcome falls through and
    // emits nothing, so the join can never satisfy.
    let success = b.exprs().call("success", vec![]);
    b.select(join, vec![]);
    b.node_mut(join).routing = ir::Routing::terminal();
    b.select(flaky, vec![ir::Arm::when(join, success)]);
    b.link(steady, join);
    b.set_join(join, JoinPolicy::All);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(Value::Null));
    let branches = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    for (firing, name) in branches {
        let outcome = if name == "flaky" {
            Outcome::cancelled()
        } else {
            // The step finished before the signal landed: a success is still a
            // legal report, and it still routes.
            Outcome::success(Value::Null)
        };
        h.finish(firing, outcome);
    }

    assert_eq!(h.status, Some(RunStatus::Cancelled));
    assert_eq!(
        h.state.pending_count(),
        1,
        "steady's token is parked at the join"
    );
    assert_eq!(
        h.state.held_scopes().count(),
        0,
        "the finished state claims no resources"
    );
    let commands: Vec<&Command> = h
        .commands
        .iter()
        .filter(|c| {
            matches!(
                c,
                Command::ReleaseScope { .. } | Command::FinishExecution { .. }
            )
        })
        .collect();
    assert!(
        matches!(
            commands.as_slice(),
            [Command::ReleaseScope { scope }, Command::FinishExecution { .. }] if scope.raw() == 0
        ),
        "terminal release, then the finish: {commands:?}"
    );
    h.verify_replay();
}

// ── Retry backoff meets cancellation (§5 test 9) ──────────────────────────

fn retry_then_cleanup_graph() -> ir::Graph {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let cleanup = b.add_step("cleanup", scope, NOOP);
    b.link(a, cleanup);
    b.node_mut(a).retry = RetryPolicy::attempts(3);
    b.node_mut(cleanup).run_on_cancel = true;
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// A cancel settles a firing that is waiting out a retry backoff at once: there
/// is no task to deliver a control to, so the core records `Cancelled` and
/// routes it. The driver's sleeper cannot be recalled; its late `RetryElapsed`
/// consumes a tombstone silently, while every other invalid `RetryElapsed`
/// still errors.
#[test]
fn cancel_settles_an_awaiting_retry_firing_at_once() {
    let mut h = Harness::new(retry_then_cleanup_graph());
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    let (a, _) = starts[0];
    h.finish(a, Outcome::failure("flaky"));
    assert!(
        h.state.firing(a).is_some_and(|f| f.awaiting_retry),
        "the firing waits out its backoff"
    );

    h.cancel(CancelScopeId::ROOT);
    assert!(h.state.firing(a).is_none(), "settled, not left waiting");
    assert_eq!(h.status_of("a").as_deref(), Some("cancelled"));

    // The settled outcome routed: the marked cleanup starts.
    let cleanup = h.take_starts();
    assert_eq!(cleanup.len(), 1);

    // The sleeper fires late; the tombstone absorbs exactly that one event.
    let errors_before = h.state.errors().len();
    h.feed(Event::RetryElapsed {
        firing:       a,
        next_attempt: Attempt::FIRST.next(),
    });
    assert_eq!(
        h.state.errors().len(),
        errors_before,
        "no error for the tombstone"
    );

    // A duplicate — the tombstone is consumed — and a never-existing firing both
    // still error: the no-op is cancellation-specific.
    h.feed(Event::RetryElapsed {
        firing:       a,
        next_attempt: Attempt::FIRST.next(),
    });
    h.feed(Event::RetryElapsed {
        firing:       FiringId::new(999),
        next_attempt: Attempt::FIRST.next(),
    });
    assert_eq!(
        h.state.errors().len(),
        errors_before + 2,
        "duplicates and unknown firings stay loud: {:?}",
        h.state.errors()
    );

    h.finish(cleanup[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

/// The kill half: settling records the outcome without routing, so not even a
/// marked cleanup node is admitted.
#[test]
fn kill_settles_an_awaiting_retry_firing_without_routing() {
    let mut h = Harness::new(retry_then_cleanup_graph());
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    let (a, _) = starts[0];
    h.finish(a, Outcome::failure("flaky"));

    h.feed(Event::KillRequested {
        scope: CancelScopeId::ROOT,
    });
    assert!(h.state.firing(a).is_none());
    assert_eq!(h.status_of("a").as_deref(), Some("cancelled"));
    assert_eq!(h.take_starts(), vec![], "nothing is admitted");
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

// ── Kill (§5 tests 10–13) ─────────────────────────────────────────────────

/// Kill is the pre-v3 cancel, kept under its own event: tokens drop, outcomes
/// are recorded without routing, `run_on_cancel` admits nothing, and every held
/// scope is still released when the run finishes.
#[test]
fn kill_stops_everything_and_still_releases_the_scopes() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    let cleanup = b.add_step("cleanup", scope, NOOP);
    b.fan_out(start, &[left, right]);
    b.link(left, after);
    b.link(right, after);
    b.set_join(after, JoinPolicy::All);
    b.link(after, cleanup);
    b.node_mut(cleanup).run_on_cancel = true;
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(Value::Null));
    let branches = h.take_starts();
    assert_eq!(branches.len(), 2);

    h.feed(Event::KillRequested {
        scope: CancelScopeId::ROOT,
    });
    let kills: Vec<FiringId> = h
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::DeliverControl {
                firing,
                ctl: Control::Kill,
            } => Some(*firing),
            _ => None,
        })
        .collect();
    assert_eq!(kills.len(), 2, "every live firing gets Control::Kill");

    for (firing, _) in branches {
        h.finish(firing, Outcome::cancelled());
    }
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    assert!(
        h.state.history().iter().all(|r| r.name != "after"),
        "recorded outcomes did not route"
    );
    assert_eq!(h.start_count("cleanup"), 0, "run_on_cancel admits nothing");
    assert_eq!(h.state.pending_count(), 0, "tokens dropped");
    assert_eq!(h.state.held_scopes().count(), 0);
    assert!(
        h.commands
            .iter()
            .any(|c| matches!(c, Command::ReleaseScope { .. })),
        "the held scope is released"
    );
    h.verify_replay();
}

/// §5 test 11: a firing the polite tier already signalled still receives
/// `Control::Kill` — the `cancelling` skip that makes repeated cancels
/// idempotent must not swallow the escalation.
#[test]
fn kill_reaches_a_firing_already_politely_cancelling() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    b.add_step("a", scope, NOOP);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.feed(Event::KillRequested {
        scope: CancelScopeId::ROOT,
    });

    let delivered: Vec<Control> = h
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::DeliverControl { ctl, .. } => Some(ctl.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        delivered,
        vec![Control::Cancel, Control::Kill],
        "the kill is delivered even though the firing was already cancelling"
    );
    h.finish(starts[0].0, Outcome::cancelled());
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

/// §5 test 12: a kill lands while admitted cleanup is running. The cleanup
/// firing gets `Control::Kill`, its outcome is recorded without routing, and
/// nothing further starts.
#[test]
fn kill_during_cleanup_stops_the_cleanup() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let cleanup = b.add_step("cleanup", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(a, cleanup);
    b.link(cleanup, after);
    b.node_mut(cleanup).run_on_cancel = true;
    b.node_mut(after).run_on_cancel = true;
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::cancelled());

    let cleanup_starts = h.take_starts();
    assert_eq!(cleanup_starts.len(), 1, "the cancel admitted the cleanup");

    h.feed(Event::KillRequested {
        scope: CancelScopeId::ROOT,
    });
    assert!(
        h.commands
            .iter()
            .any(|c| matches!(c, Command::DeliverControl {
                ctl: Control::Kill,
                ..
            })),
        "the running cleanup gets Control::Kill"
    );
    h.finish(cleanup_starts[0].0, Outcome::cancelled());
    assert_eq!(h.take_starts(), vec![], "nothing further starts");
    assert!(h.state.history().iter().all(|r| r.name != "after"));
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    // §5 test 13: a log that ends in a kill still replays byte-identically.
    h.verify_replay();
}

/// A kill is scope-addressed like a cancel: killing a splice scope stops its
/// clones — outcomes recorded, nothing routed, the collector never satisfied —
/// while the rest of the run proceeds.
#[test]
fn killing_a_splice_scope_spares_the_rest_of_the_run() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let matrix = b.add_step("matrix", scope, NOOP);
    let sibling = b.add_step("sibling", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.fan_out(plan, &[matrix, sibling]);
    b.select(matrix, vec![
        ir::Arm::always(collect).with_map(collector.indexed),
    ]);
    b.set_join(collect, JoinPolicy::All);
    parallel_for_each(&mut b, matrix, items, ExpandTarget::Node, None, false);
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.finish(starts[0].0, Outcome::success(json!(["a", "b"])));
    let running = h.take_starts();
    assert_eq!(running.len(), 3, "two clones plus the sibling");

    let splice_scope = h.state.splices().first().unwrap().cancel_scope;
    h.feed(Event::KillRequested {
        scope: splice_scope,
    });
    let kills = h
        .commands
        .iter()
        .filter(|c| {
            matches!(c, Command::DeliverControl {
                ctl: Control::Kill,
                ..
            })
        })
        .count();
    assert_eq!(kills, 2, "only the clones are killed");
    assert!(!h.state.is_cancelled(), "the run itself is not cancelled");

    for (firing, name) in running {
        let outcome = if name.starts_with("matrix") {
            Outcome::cancelled()
        } else {
            Outcome::success(Value::Null)
        };
        h.finish(firing, outcome);
    }
    assert_eq!(h.status, Some(RunStatus::Success));
    assert_eq!(h.start_count("collect"), 0);
    assert!(
        h.state.history().iter().all(|r| r.name != "collect"),
        "killed outcomes did not route, so the collector has no record at all"
    );
    h.verify_replay();
}

/// `run.cancelled` is root-only; `scope_cancelled` is true wherever the
/// firing's node lies in a cancelled cancel-scope. A root cancel sets both; a
/// `fail_fast` splice cancel is visible only to `scope_cancelled`.
#[test]
fn scope_cancelled_and_run_cancelled_read_correctly() {
    use ir::placeholder::EXPR_PLACEHOLDER_KEY;

    // Root cancel: both statics are true for the admitted cleanup.
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let probe = b.add_step("probe", scope, NOOP);
    b.link(a, probe);
    let sc = b.exprs().var("scope_cancelled");
    let run = b.exprs().var("run");
    let rc = b.exprs().field(run, "cancelled");
    let config = json!({
        "sc": { EXPR_PLACEHOLDER_KEY: sc.raw() },
        "rc": { EXPR_PLACEHOLDER_KEY: rc.raw() },
    });
    b.node_mut(probe).step = ir::StepRef::new(NOOP, config.clone());
    b.node_mut(probe).run_on_cancel = true;
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(starts[0].0, Outcome::cancelled());
    let resolved = h.commands_of(|c| match c {
        Command::StartStep(r) => Some(r.config().clone()),
        _ => None,
    });
    assert_eq!(
        resolved,
        vec![json!({ "sc": true, "rc": true })],
        "a root cancel sets both statics"
    );
    let probes = h.take_starts();
    h.finish(probes[0].0, Outcome::success(Value::Null));
    h.verify_replay();

    // Fail-fast splice cancel: `scope_cancelled` true inside the splice while
    // `run.cancelled` stays false.
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let work = b.add_step("work", scope, NOOP);
    let collect = b.add_step("collect", scope, NOOP);
    let collector = collector_exprs(b.exprs());
    let items = b.exprs().var("input");
    b.link(plan, work);
    b.select(work, vec![
        ir::Arm::always(collect).with_map(collector.indexed),
    ]);
    b.set_join(collect, JoinPolicy::All);
    let sc = b.exprs().var("scope_cancelled");
    let run = b.exprs().var("run");
    let rc = b.exprs().field(run, "cancelled");
    b.node_mut(work).step = ir::StepRef::new(
        NOOP,
        json!({
            "sc": { EXPR_PLACEHOLDER_KEY: sc.raw() },
            "rc": { EXPR_PLACEHOLDER_KEY: rc.raw() },
        }),
    );
    b.node_mut(work).run_on_cancel = true;
    // One at a time, so the cancel lands before the siblings ever fire.
    parallel_for_each(&mut b, work, items, ExpandTarget::Node, Some(1), true);
    let graph = b.build();

    let seen = Rc::new(RefCell::new(Vec::<(String, Value)>::new()));
    let sink = seen.clone();
    let mut h = Harness::new(graph).respond_with(move |info| match info.base.as_str() {
        "plan" => Outcome::success(json!(["a", "b"])),
        "work" => {
            sink.borrow_mut()
                .push((info.name.clone(), info.config.clone()));
            if info.index == Some(0) {
                Outcome::failure("boom")
            } else {
                Outcome::success(Value::Null)
            }
        }
        _ => Outcome::success(Value::Null),
    });
    assert_eq!(h.run(), RunStatus::Failed);

    let seen = seen.borrow();
    let of = |name: &str| {
        seen.iter()
            .find(|(n, _)| n == name)
            .map_or(Value::Null, |(_, c)| c.clone())
    };
    assert_eq!(
        of("work#0"),
        json!({ "sc": false, "rc": false }),
        "before the fail_fast cancel, neither static is set"
    );
    assert_eq!(
        of("work#1"),
        json!({ "sc": true, "rc": false }),
        "after it, the splice scope reads cancelled while the run does not"
    );
    h.verify_replay();
}

/// A cancelled run's log — cancel, routed cancelled outcomes, admitted cleanup
/// — replays byte-identically. `Status::Cancelled` in an outcome must survive
/// the round trip like every other status.
#[test]
fn a_cancelled_runs_log_replays_byte_identically() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let cleanup = b.add_step("cleanup", scope, NOOP);
    b.link(a, cleanup);
    b.node_mut(cleanup).run_on_cancel = true;
    let graph = b.build();
    validate(&graph).expect("valid");

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.finish(
        starts[0].0,
        Outcome::new(Status::Cancelled, json!({ "cancel_escalation": "sigterm" })),
    );
    let cleanup_starts = h.take_starts();
    h.finish(cleanup_starts[0].0, Outcome::success(Value::Null));
    assert_eq!(h.status, Some(RunStatus::Cancelled));
    h.verify_replay();
}

/// Cancelling twice does not deliver the signal twice.
#[test]
fn cancelling_twice_delivers_one_signal() {
    let mut b = GraphBuilder::new();
    let scope = ir::ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let graph = b.build();
    validate(&graph).expect("valid");
    let _ = a;

    let mut h = Harness::new(graph);
    h.feed(Event::ExecutionStarted {
        start: engine::EngineStart::default(),
    });
    let starts = h.take_starts();
    h.cancel(CancelScopeId::ROOT);
    h.cancel(CancelScopeId::ROOT);
    let delivered = h
        .commands
        .iter()
        .filter(|c| matches!(c, Command::DeliverControl { .. }))
        .count();
    assert_eq!(delivered, 1);
    h.finish(starts[0].0, Outcome::cancelled());
    assert_eq!(h.status, Some(RunStatus::Cancelled));
}
