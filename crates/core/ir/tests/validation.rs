//! §7: the invariants checked at load. Every check runs, so one call reports
//! every problem rather than stopping at the first.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use ir::placeholder::EXPR_PLACEHOLDER_KEY;
use ir::{
    Arm, Budget, Completion, Edge, EdgeId, EdgeTransition, ExpandTarget, Expansion, ExprId,
    ExprTable, Graph, GraphBuilder, Guard, JoinPolicy, Node, NodeId, Routing, Scope, ScopeId,
    SelectGroup, StepKindId, StepRef, ValidationError, ValidationLocation, ValidationWarning,
    Value, validate,
};
use serde_json::json;

const NOOP: StepKindId = StepKindId::new_static("noop");

fn errors(graph: &Graph) -> Vec<ValidationError> {
    validate(graph).expect_err("expected validation to fail")
}

/// A valid two-node graph, as a baseline.
#[test]
fn a_well_formed_graph_passes() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    validate(&b.build()).expect("valid");
}

#[test]
fn validation_diagnostics_own_their_shared_metadata() {
    let node = NodeId::new(3);
    let error: ValidationError = ValidationError::LoopHeadMustJoinAny(node);
    assert_eq!(error.code(), "validate.loop_head_must_join_any");
    assert_eq!(error.primary_node(), Some(node));
    assert_eq!(error.location(), ValidationLocation::Node(node));
    assert!(error.hint().is_some());

    let warning: ValidationWarning = ValidationWarning::RunOnCancelExpansion { node };
    assert_eq!(warning.code(), "lint.run_on_cancel_expansion");
    assert_eq!(warning.primary_node(), node);
}

/// Every structural error reports its own code, so the same error never shows
/// a different code depending on which caller formatted it.
#[test]
fn structural_errors_have_distinct_codes() {
    let node = NodeId::new(1);
    let cases: Vec<(ValidationError, &str)> = vec![
        (
            ValidationError::NodeIdMismatch {
                index:    0,
                declared: node,
            },
            "validate.node_id_mismatch",
        ),
        (
            ValidationError::ScopeIdMismatch {
                index:    0,
                declared: ScopeId::new(1),
            },
            "validate.scope_id_mismatch",
        ),
        (
            ValidationError::UnknownScope {
                node,
                scope: ScopeId::new(9),
            },
            "validate.unknown_scope",
        ),
        (
            ValidationError::UnknownTarget {
                from: node,
                edge: EdgeId::new(0),
                to:   NodeId::new(9),
            },
            "validate.unknown_target",
        ),
        (
            ValidationError::UnknownStepKind { node, kind: NOOP },
            "step.unknown_kind",
        ),
        (
            ValidationError::BadStepConfig {
                node,
                class: "bad_config".into(),
                message: "bad".to_string(),
            },
            "step.bad_config",
        ),
        (ValidationError::NoEntry, "validate.no_entry"),
        (
            ValidationError::UnknownEntry(node),
            "validate.unknown_entry",
        ),
        (
            ValidationError::DuplicateEntry(node),
            "validate.duplicate_entry",
        ),
        (
            ValidationError::CompletionUnknownNode(node),
            "validate.completion_unknown_node",
        ),
        (
            ValidationError::AllJoinExclusiveArms {
                node,
                from: NodeId::new(0),
                first: EdgeId::new(0),
                second: EdgeId::new(1),
            },
            "validate.all_join_exclusive_arms",
        ),
        (
            ValidationError::QuorumExceedsFanIn {
                node,
                n: 3,
                fan_in: 2,
            },
            "validate.quorum_exceeds_fan_in",
        ),
    ];
    for (error, expected) in &cases {
        assert_eq!(error.code(), *expected, "{error}");
    }
    let codes: BTreeSet<&str> = cases.iter().map(|(_, code)| *code).collect();
    assert_eq!(codes.len(), cases.len(), "no two variants share a code");
}

/// Invariant 1: every cycle contains at least one back edge.
#[test]
fn a_cycle_without_a_back_edge_is_rejected() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    b.link(c, a);
    b.set_budget(a, Budget::looped(5));
    b.set_budget(c, Budget::looped(5));
    b.mark_entry(a);
    let graph = b.build();

    assert!(matches!(
        errors(&graph).as_slice(),
        [.., ValidationError::CycleWithoutBackEdge(_)]
            | [ValidationError::CycleWithoutBackEdge(_), ..]
    ));

    // Marking the return edge as a back edge fixes it.
    let mut b = GraphBuilder::new();
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.set_join(a, JoinPolicy::Any);
    b.link(a, c);
    b.select(c, vec![Arm::always(a).with_back()]);
    b.set_budget(a, Budget::looped(5));
    b.set_budget(c, Budget::looped(5));
    b.mark_entry(a);
    validate(&b.build()).expect("a back edge makes the cycle legal");
}

/// Invariant 2: `Guard::Always` may only be a group's final arm.
#[test]
fn always_must_be_the_last_arm() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let first = b.add_step("first", scope, NOOP);
    let second = b.add_step("second", scope, NOOP);
    b.select(a, vec![Arm::always(first), Arm::always(second)]);
    let graph = b.build();

    assert!(
        errors(&graph)
            .iter()
            .any(|e| matches!(e, ValidationError::AlwaysNotLast { arm: 0, .. }))
    );
}

/// Invariant 3: groups are non-empty, but a node may have no groups at all.
#[test]
fn groups_must_have_arms_but_a_node_need_not_have_groups() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    b.node_mut(a).routing = Routing::groups(vec![SelectGroup::new(vec![])]);
    let graph = b.build();
    assert!(
        errors(&graph)
            .iter()
            .any(|e| matches!(e, ValidationError::EmptyGroup { group: 0, .. }))
    );

    let mut b = GraphBuilder::new();
    b.add_step("terminal", scope, NOOP);
    validate(&b.build()).expect("a terminal node is fine");
}

/// Invariant 4: `max_firings >= 1`, and anything downstream of a back edge
/// needs a finite cap.
#[test]
fn budgets_must_be_at_least_one_and_finite_inside_loops() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    b.set_budget(a, Budget::new(0, Duration::from_secs(1)));
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::ZeroBudget(_)))
    );

    let mut b = GraphBuilder::new();
    let head = b.add_step("head", scope, NOOP);
    let tail = b.add_step("tail", scope, NOOP);
    b.set_join(head, JoinPolicy::Any);
    b.link(head, tail);
    b.select(tail, vec![Arm::always(head).with_back()]);
    b.set_budget(
        head,
        Budget::new(Budget::UNBOUNDED_FIRINGS, Duration::from_secs(1)),
    );
    b.mark_entry(head);
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::UnboundedLoopBudget(_)))
    );
}

/// Invariant 5: edge ids are unique, and the seed id is reserved.
#[test]
fn edge_ids_must_be_unique() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    b.fan_out(a, &[left, right]);
    let mut graph = b.build();
    // Force a collision.
    let clashing = graph.nodes[a.index()].routing.groups[0].arms[0].id;
    graph.body.nodes[a.index()].routing.groups[1].arms[0].id = clashing;
    assert!(
        errors(&graph)
            .iter()
            .any(|e| matches!(e, ValidationError::DuplicateEdgeId(_)))
    );

    let mut b = GraphBuilder::new();
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.node_mut(a).routing = Routing::next(Edge::always(ir::EdgeId::SEED, c));
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::ReservedEdgeId(_)))
    );
}

/// Invariant 6: expression references resolve.
#[test]
fn expression_references_must_resolve() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.select(a, vec![Arm::when(c, ExprId::new(99))]);
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::UnknownExpr { .. }))
    );
}

/// Invariant 7: an edge into the middle of an expansion region is a boundary
/// crossing.
#[test]
fn expansion_regions_may_only_be_entered_at_the_entry() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let outside = b.add_step("outside", scope, NOOP);
    let entry = b.add_step("entry", scope, NOOP);
    let middle = b.add_step("middle", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(entry, middle);
    b.link(middle, exit);
    b.link(exit, after);
    // `outside` jumps straight into the middle of the region.
    b.link(outside, middle);
    let items = b.exprs().lit(json!([1]));
    b.set_expansion(entry, Expansion::ForEach {
        items,
        target: ExpandTarget::Subgraph { entry, exit },
        max_parallel: None,
        fail_fast: false,
    });
    b.mark_entry(outside);
    b.mark_entry(entry);

    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::BoundaryCrossing { .. }))
    );
}

/// Invariant 7: the exit must postdominate the entry, so a region member that
/// can finish without reaching the exit is rejected.
#[test]
fn the_expansion_exit_must_postdominate_the_entry() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let entry = b.add_step("entry", scope, NOOP);
    let stops = b.add_step("stops", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    b.fan_out(entry, &[stops, exit]);
    let items = b.exprs().lit(json!([1]));
    b.set_expansion(entry, Expansion::ForEach {
        items,
        target: ExpandTarget::Subgraph { entry, exit },
        max_parallel: None,
        fail_fast: false,
    });
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::ExitNotPostdominator { .. }))
    );
}

/// A well-formed expansion region passes.
#[test]
fn a_well_formed_expansion_region_passes() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let entry = b.add_step("entry", scope, NOOP);
    let middle = b.add_step("middle", scope, NOOP);
    let exit = b.add_step("exit", scope, NOOP);
    let after = b.add_step("after", scope, NOOP);
    b.link(entry, middle);
    b.link(middle, exit);
    b.link(exit, after);
    let items = b.exprs().lit(json!([1, 2]));
    b.set_expansion(entry, Expansion::ForEach {
        items,
        target: ExpandTarget::Subgraph { entry, exit },
        max_parallel: Some(2),
        fail_fast: true,
    });
    validate(&b.build()).expect("valid");
}

/// Structure: node ids equal their index, targets exist, entries are seeded.
#[test]
fn structural_problems_are_reported() {
    let graph = Graph {
        body:       ir::GraphBody {
            nodes:  vec![Node::new(
                NodeId::new(7),
                "misnumbered",
                ScopeId::new(0),
                StepRef::new(NOOP, Value::Null),
            )],
            scopes: vec![Scope::new(ScopeId::new(0))],
            exprs:  ExprTable::default(),
            entry:  vec![],
        },
        policy:     ir::RunPolicy::default(),
        params:     BTreeMap::default(),
        completion: Completion::default(),
        result:     ir::ResultProjection::None,
    };
    let found = errors(&graph);
    assert!(
        found
            .iter()
            .any(|e| matches!(e, ValidationError::NodeIdMismatch { .. }))
    );
    assert!(found.iter().any(|e| matches!(e, ValidationError::NoEntry)));

    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let mut graph = b.build();
    graph.body.nodes[a.index()].routing =
        Routing::next(Edge::always(ir::EdgeId::new(0), NodeId::new(9)));
    assert!(
        errors(&graph)
            .iter()
            .any(|e| matches!(e, ValidationError::UnknownTarget { .. }))
    );
}

/// A step kind that is not registered is reported when a registry is supplied.
#[test]
fn unknown_step_kinds_are_reported() {
    struct NoKinds;
    impl ir::StepKinds for NoKinds {
        fn get(&self, _id: &ir::StepKindId) -> Option<&dyn ir::StepKind> {
            None
        }
    }

    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    b.add_step("a", scope, StepKindId::new("unregistered"));
    let graph = b.build();

    validate(&graph).expect("valid without a registry");
    assert!(
        ir::validate_with(&graph, Some(&NoKinds))
            .expect_err("kind `unregistered` is not registered")
            .iter()
            .any(|e| matches!(e, ValidationError::UnknownStepKind { .. }))
    );
}

/// An entry node with incoming edges is a contradiction: entries are seeded.
#[test]
fn entry_nodes_may_not_have_incoming_edges() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    b.mark_entry(a);
    b.mark_entry(c);
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::EntryHasIncoming(_)))
    );
}

/// `Guard::Always` on the only arm of a group is fine.
#[test]
fn a_single_always_arm_is_legal() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.select(a, vec![Arm::always(c)]);
    let graph = b.build();
    validate(&graph).expect("valid");
    assert_eq!(
        graph.node(a).unwrap().routing.groups[0].arms[0].guard,
        Guard::Always
    );
}

/// Invariant 8: a node with an incoming back edge is a loop head, and must join
/// with `Any`.
#[test]
fn a_loop_head_must_join_with_any() {
    let build = |join: JoinPolicy| {
        let mut b = GraphBuilder::new();
        let scope = ScopeId::new(0);
        let head = b.add_step("head", scope, NOOP);
        let tail = b.add_step("tail", scope, NOOP);
        b.set_join(head, join);
        b.link(head, tail);
        b.select(tail, vec![Arm::always(head).with_back()]);
        b.set_budget(head, Budget::looped(5));
        b.set_budget(tail, Budget::looped(5));
        b.mark_entry(head);
        b.build()
    };

    for join in [JoinPolicy::All, JoinPolicy::Quorum { n: 2 }] {
        let graph = build(join);
        assert!(
            errors(&graph)
                .iter()
                .any(|e| matches!(e, ValidationError::LoopHeadMustJoinAny(_))),
            "{join:?} should be rejected on a loop head"
        );
    }
    validate(&build(JoinPolicy::Any)).expect("Any is the only legal loop-head join");
}

/// The corollary: a node cannot be both a multi-branch `All` join and a loop
/// head. The fix is a dedicated join node in front of the head.
#[test]
fn a_multi_branch_join_needs_its_own_node_in_front_of_a_loop_head() {
    // Broken: `head` tries to be both the `All` join for two branches and the
    // target of the back edge.
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let head = b.add_step("head", scope, NOOP);
    b.fan_out(start, &[left, right]);
    b.link(left, head);
    b.link(right, head);
    b.set_join(head, JoinPolicy::All);
    b.select(head, vec![Arm::always(head).with_back()]);
    b.set_budget(head, Budget::looped(5));
    assert!(
        errors(&b.build())
            .iter()
            .any(|e| matches!(e, ValidationError::LoopHeadMustJoinAny(_)))
    );

    // Fixed: `gate` does the `All` join, `head` does the looping.
    let mut b = GraphBuilder::new();
    let start = b.add_step("start", scope, NOOP);
    let left = b.add_step("left", scope, NOOP);
    let right = b.add_step("right", scope, NOOP);
    let gate = b.add_step("gate", scope, NOOP);
    let head = b.add_step("head", scope, NOOP);
    b.fan_out(start, &[left, right]);
    b.link(left, gate);
    b.link(right, gate);
    b.set_join(gate, JoinPolicy::All);
    b.link(gate, head);
    b.set_join(head, JoinPolicy::Any);
    b.select(head, vec![Arm::always(head).with_back()]);
    b.set_budget(head, Budget::looped(5));
    validate(&b.build()).expect("valid");
}

/// Invariant 10: an `All` join may not count two arms of one routing group.
/// The group emits one token, so the join would wait forever.
#[test]
fn an_all_join_cannot_wait_on_two_arms_of_one_group() {
    let build = |join: JoinPolicy| {
        let mut b = GraphBuilder::new();
        let scope = ScopeId::new(0);
        let classify = b.add_step("classify", scope, NOOP);
        let done = b.add_step("done", scope, NOOP);
        let small = b.exprs().call("success", Vec::new());
        b.select(classify, vec![Arm::when(done, small), Arm::always(done)]);
        b.set_join(done, join);
        (b.build(), classify, done)
    };

    let (graph, classify, done) = build(JoinPolicy::All);
    let errors = errors(&graph);
    assert_eq!(errors, vec![ValidationError::AllJoinExclusiveArms {
        node:   done,
        from:   classify,
        first:  EdgeId::new(0),
        second: EdgeId::new(1),
    }]);
    assert_eq!(errors[0].primary_node(), Some(done));
    assert!(errors[0].hint().is_some());

    let (graph, ..) = build(JoinPolicy::Any);
    validate(&graph).expect("`Any` runs on whichever arm the group picks");
}

/// Separate groups each emit their own token, so an `All` join may count an
/// arm from each.
#[test]
fn an_all_join_may_wait_on_arms_of_separate_groups() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_step("start", scope, NOOP);
    let done = b.add_step("done", scope, NOOP);
    let group = || vec![Arm::always(done)];
    b.fan_out_groups(start, vec![group(), group()]);
    validate(&b.build()).expect("both groups emit");
}

/// A successor execution enters a restart target directly, without its join,
/// so invariant 10 leaves the target alone.
#[test]
fn a_restart_target_is_exempt_from_invariant_10() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let classify = b.add_step("classify", scope, NOOP);
    let done = b.add_step("done", scope, NOOP);
    let again = b.exprs().call("failure", Vec::new());
    b.select(classify, vec![Arm::when(done, again), Arm::always(done)]);
    b.node_mut(classify).routing.groups[0].arms[0].transition = EdgeTransition::Restart;
    validate(&b.build()).expect("the restart target is exempt");
}

/// Invariant 10: a `Quorum { n }` needs `n` routing groups that can feed it.
#[test]
fn a_quorum_needs_as_many_feeding_groups_as_it_counts() {
    let build = |n: u32| {
        let mut b = GraphBuilder::new();
        let scope = ScopeId::new(0);
        let start = b.add_step("start", scope, NOOP);
        let left = b.add_step("left", scope, NOOP);
        let right = b.add_step("right", scope, NOOP);
        let gate = b.add_step("gate", scope, NOOP);
        b.fan_out(start, &[left, right]);
        b.link(left, gate);
        b.link(right, gate);
        b.set_join(gate, JoinPolicy::Quorum { n });
        (b.build(), gate)
    };

    let (graph, gate) = build(3);
    let errors = errors(&graph);
    assert_eq!(errors, vec![ValidationError::QuorumExceedsFanIn {
        node:   gate,
        n:      3,
        fan_in: 2,
    }]);
    assert!(errors[0].hint().is_some());
    validate(&build(2).0).expect("two groups feed a quorum of two");
}

/// Two arms of one group count once toward a quorum, and an entry's seed
/// counts as one group.
#[test]
fn arms_of_one_group_and_an_entry_seed_count_once_toward_a_quorum() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let classify = b.add_step("classify", scope, NOOP);
    let gate = b.add_step("gate", scope, NOOP);
    let small = b.exprs().call("success", Vec::new());
    b.select(classify, vec![Arm::when(gate, small), Arm::always(gate)]);
    b.set_join(gate, JoinPolicy::Quorum { n: 2 });
    b.set_join(classify, JoinPolicy::Quorum { n: 2 });
    let graph = b.build();
    let found: BTreeSet<(NodeId, usize)> = errors(&graph)
        .iter()
        .filter_map(|error| match error {
            ValidationError::QuorumExceedsFanIn { node, fan_in, .. } => Some((*node, *fan_in)),
            _ => None,
        })
        .collect();
    assert_eq!(found, BTreeSet::from([(classify, 1), (gate, 1)]));
}

/// The node a `for_each` body exits to gains one edge per clone at run time,
/// so its quorum is not checked at load.
#[test]
fn a_quorum_after_a_for_each_is_left_to_run_time() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let plan = b.add_step("plan", scope, NOOP);
    let deploy = b.add_step("deploy", scope, NOOP);
    let report = b.add_step("report", scope, NOOP);
    b.link(plan, deploy);
    b.link(deploy, report);
    let items = b.exprs().lit(json!(["a", "b", "c"]));
    b.set_expansion(deploy, Expansion::ForEach {
        items,
        target: ExpandTarget::Node,
        max_parallel: None,
        fail_fast: false,
    });
    b.set_join(report, JoinPolicy::Quorum { n: 2 });
    validate(&b.build()).expect("the clones feed the quorum");
}

/// A `for_each` node's own quorum is counted like any other node's; its
/// clones start on their seeds without it.
#[test]
fn a_for_each_node_counts_its_own_quorum() {
    let build = |n: u32| {
        let mut b = GraphBuilder::new();
        let scope = ScopeId::new(0);
        let start = b.add_step("start", scope, NOOP);
        let left = b.add_step("left", scope, NOOP);
        let right = b.add_step("right", scope, NOOP);
        let deploy = b.add_step("deploy", scope, NOOP);
        b.fan_out(start, &[left, right]);
        b.link(left, deploy);
        b.link(right, deploy);
        let items = b.exprs().lit(json!(["a", "b"]));
        b.set_expansion(deploy, Expansion::ForEach {
            items,
            target: ExpandTarget::Node,
            max_parallel: None,
            fail_fast: false,
        });
        b.set_join(deploy, JoinPolicy::Quorum { n });
        (b.build(), deploy)
    };

    validate(&build(2).0).expect("two groups feed the quorum");
    let (graph, deploy) = build(3);
    assert_eq!(errors(&graph), vec![ValidationError::QuorumExceedsFanIn {
        node:   deploy,
        n:      3,
        fan_in: 2,
    }]);
}

/// A join that meets a loop's exit and a branch that skips the loop fires only
/// when the loop exits in generation 0. It is warned about, not refused: it
/// works for a loop that does not iterate, and nothing else can express the
/// wait yet.
#[test]
fn a_join_across_generations_is_a_warning() {
    let build = |join: JoinPolicy| {
        let mut b = GraphBuilder::new();
        let scope = ScopeId::new(0);
        let start = b.add_step("start", scope, NOOP);
        let plan = b.add_step("plan", scope, NOOP);
        let work = b.add_step("work", scope, NOOP);
        let side = b.add_step("side", scope, NOOP);
        let report = b.add_step("report", scope, NOOP);
        b.fan_out(start, &[plan, side]);
        ir::sequential_for_each(&mut b, plan, work, work, report, 10);
        b.link(side, report);
        b.set_join(report, join);
        (b.build(), report, work, side)
    };

    let (graph, report, work, side) = build(JoinPolicy::All);
    let checked = ir::check(&graph);
    assert!(checked.is_ok(), "{:?}", checked.errors);
    assert_eq!(checked.warnings, vec![
        ValidationWarning::JoinAcrossGenerations {
            node:    report,
            inside:  work,
            outside: side,
        }
    ]);
    assert_eq!(checked.warnings[0].code(), "lint.join_across_generations");
    assert!(checked.warnings[0].hint().is_some());

    // A quorum of two needs both sides, so it waits the same way.
    assert_eq!(
        ir::check(&build(JoinPolicy::Quorum { n: 2 }).0)
            .warnings
            .len(),
        1
    );
    // `Any`, and a quorum either side reaches alone, can fire in every generation.
    for join in [JoinPolicy::Any, JoinPolicy::Quorum { n: 1 }] {
        assert!(ir::check(&build(join).0).warnings.is_empty(), "{join:?}");
    }
}

/// A scope a path can leave and return to gets a warning, not an error: release
/// is irreversible, so re-entry acquires a fresh runtime and workspace.
#[test]
fn re_entering_a_scope_is_a_warning() {
    let mut b = GraphBuilder::bare();
    let job = b.add_scope(Scope::new(ScopeId::new(0)));
    let helper = b.add_scope(Scope::new(ScopeId::new(0)));
    let first = b.add_step("first", job, NOOP);
    let away = b.add_step("away", helper, NOOP);
    let back = b.add_step("back", job, NOOP);
    b.link(first, away);
    b.link(away, back);
    let graph = b.build();

    let report = ir::check(&graph);
    assert!(report.is_ok(), "re-entry is legal, just risky");
    assert_eq!(report.warnings, vec![ValidationWarning::ScopeReentry {
        scope: job,
        at:    back,
        via:   away,
    }]);

    // A scope nothing leaves and returns to draws no warning.
    let mut b = GraphBuilder::bare();
    let job = b.add_scope(Scope::new(ScopeId::new(0)));
    let helper = b.add_scope(Scope::new(ScopeId::new(0)));
    let first = b.add_step("first", job, NOOP);
    let second = b.add_step("second", job, NOOP);
    let after = b.add_step("after", helper, NOOP);
    b.link(first, second);
    b.link(second, after);
    assert!(ir::check(&b.build()).warnings.is_empty());
}

/// The diamond is suppressed: an `All` re-entry node with an incoming forward
/// edge from inside the scope cannot fire before the scope would be released.
///
/// Either the inside token is emitted while the scope is still held — and then
/// it is a pending token pinning the scope until the join resolves — or the
/// inside arm never emits, in which case the `All` join is unsatisfiable and
/// the node never fires at all.
#[test]
fn a_diamond_back_into_its_own_scope_is_not_a_re_entry() {
    let mut b = GraphBuilder::bare();
    let main = b.add_scope(Scope::new(ScopeId::new(0)));
    let other = b.add_scope(Scope::new(ScopeId::new(0)));
    let start = b.add_step("start", main, NOOP);
    let inside = b.add_step("inside", main, NOOP);
    let outside = b.add_step("outside", other, NOOP);
    let join = b.add_step("join", main, NOOP);
    b.fan_out(start, &[inside, outside]);
    b.link(inside, join);
    b.link(outside, join);
    b.set_join(join, JoinPolicy::All);
    let graph = b.build();

    let report = ir::check(&graph);
    assert!(report.is_ok());
    assert!(
        report.warnings.is_empty(),
        "the All join on an inside edge makes re-entry impossible: {:?}",
        report.warnings
    );
}

/// `Any` and `Quorum` re-entry nodes keep the warning: they can fire on the
/// outside token alone, after the scope has been released.
#[test]
fn a_permissive_join_does_not_suppress_the_re_entry_warning() {
    for join in [JoinPolicy::Any, JoinPolicy::Quorum { n: 1 }] {
        let mut b = GraphBuilder::bare();
        let main = b.add_scope(Scope::new(ScopeId::new(0)));
        let other = b.add_scope(Scope::new(ScopeId::new(0)));
        let start = b.add_step("start", main, NOOP);
        let inside = b.add_step("inside", main, NOOP);
        let outside = b.add_step("outside", other, NOOP);
        let land = b.add_step("land", main, NOOP);
        b.fan_out(start, &[inside, outside]);
        b.link(inside, land);
        b.link(outside, land);
        b.set_join(land, join);
        let report = ir::check(&b.build());
        assert_eq!(
            report.warnings.len(),
            1,
            "{join:?} can fire from outside after release"
        );
    }
}

/// A loop head that is also a re-entry point always warns. Suppression needs an
/// `All` join, and invariant 8 forces `Any` on anything with an incoming back
/// edge, so the two can never both hold. The `!back` filter in the suppression
/// test is therefore defensive: a back edge carries only later generations and
/// could not pin the scope for the generation arriving from outside anyway.
#[test]
fn a_loop_head_can_never_suppress_the_re_entry_warning() {
    let mut b = GraphBuilder::bare();
    let main = b.add_scope(Scope::new(ScopeId::new(0)));
    let other = b.add_scope(Scope::new(ScopeId::new(0)));
    let start = b.add_step("start", main, NOOP);
    let away = b.add_step("away", other, NOOP);
    let head = b.add_step("head", main, NOOP);
    let tail = b.add_step("tail", main, NOOP);
    b.link(start, away);
    b.link(away, head);
    b.set_join(head, JoinPolicy::Any);
    b.link(head, tail);
    b.select(tail, vec![Arm::always(head).with_back()]);
    b.set_budget(head, Budget::looped(5));
    b.set_budget(tail, Budget::looped(5));
    b.mark_entry(start);
    let graph = b.build();

    let report = ir::check(&graph);
    assert!(report.is_ok(), "{:?}", report.errors);
    assert_eq!(report.warnings, vec![ValidationWarning::ScopeReentry {
        scope: main,
        at:    head,
        via:   away,
    }]);
}

/// `check` reports errors and warnings together; `validate` is the errors-only
/// view.
#[test]
fn check_reports_both_errors_and_warnings() {
    let mut b = GraphBuilder::bare();
    let job = b.add_scope(Scope::new(ScopeId::new(0)));
    let helper = b.add_scope(Scope::new(ScopeId::new(0)));
    let first = b.add_step("first", job, NOOP);
    let away = b.add_step("away", helper, NOOP);
    let back = b.add_step("back", job, NOOP);
    b.link(first, away);
    b.link(away, back);
    b.set_budget(back, Budget::new(0, Duration::from_secs(1)));
    let graph = b.build();

    let report = ir::check(&graph);
    assert!(!report.is_ok());
    assert!(!report.warnings.is_empty());
    assert!(report.into_result().is_err());
}

/// An unresolved placeholder is reported with its path, so a deep config says
/// where.
#[test]
fn placeholder_paths_point_at_the_offending_field() {
    use ir::placeholder::{contains_placeholder, placeholder_path};
    let config = json!({ "env": { "TOKEN": { EXPR_PLACEHOLDER_KEY: 3 } } });
    assert!(contains_placeholder(&config));
    assert_eq!(placeholder_path(&config).as_deref(), Some("env.TOKEN"));

    let in_array = json!({ "args": ["--flag", { EXPR_PLACEHOLDER_KEY: 1 }] });
    assert_eq!(placeholder_path(&in_array).as_deref(), Some("args[1]"));

    assert_eq!(placeholder_path(&json!({ "plain": 1 })), None);
}

/// A frontend that produces `Quorum { n: 1 }` on a loop head normalizes it to
/// `Any` rather than the invariant being relaxed.
#[test]
fn loop_head_normalization_rewrites_quorum_one_to_any() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let head = b.add_step("head", scope, NOOP);
    let tail = b.add_step("tail", scope, NOOP);
    b.set_join(head, JoinPolicy::Quorum { n: 1 });
    b.link(head, tail);
    b.select(tail, vec![Arm::always(head).with_back()]);
    b.set_budget(head, Budget::looped(5));
    b.set_budget(tail, Budget::looped(5));
    b.mark_entry(head);
    let mut graph = b.build();

    assert!(
        errors(&graph)
            .iter()
            .any(|e| matches!(e, ValidationError::LoopHeadMustJoinAny(_))),
        "the invariant stays strict"
    );

    assert_eq!(graph.normalize_loop_heads(), 1);
    assert_eq!(graph.node(head).unwrap().join, JoinPolicy::Any);
    validate(&graph).expect("valid after normalization");

    // It only touches loop heads, and only `Quorum { n: 1 }`.
    assert_eq!(graph.normalize_loop_heads(), 0);
}

/// `Completion::TerminalNode` must name a node that exists — and nothing more:
/// terminal-shaped routing is not required (resolved decision 2; the semantics
/// only need a final record), so a node with outgoing edges is accepted.
#[test]
fn completion_terminal_node_must_exist() {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let a = b.add_step("a", scope, NOOP);
    let c = b.add_step("c", scope, NOOP);
    b.link(a, c);
    let mut graph = b.build();

    graph.completion = ir::Completion::TerminalNode(NodeId::new(7));
    assert!(
        errors(&graph).contains(&ValidationError::CompletionUnknownNode(NodeId::new(7))),
        "an unknown node is rejected"
    );

    // `a` routes onward, and is still accepted.
    graph.completion = ir::Completion::TerminalNode(a);
    validate(&graph).expect("an existing node is accepted; empty routing is not required");
}
