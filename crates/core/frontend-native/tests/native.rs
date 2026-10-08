//! Handoff §7 test 7: the native format reaches the whole engine, and the
//! invariant-8 error reads as a hint rather than an accident.

use frontend::Severity;
use frontend_native::load;
use ir::JoinPolicy;

#[expect(
    clippy::print_stderr,
    reason = "a failing test needs the lowering diagnostics on stderr to be readable"
)]
fn lower_ok(text: &str) -> ir::Graph {
    let lowered = load("test.yml", text);
    for d in lowered.diagnostics.iter() {
        eprintln!("{d}");
    }
    lowered.graph.unwrap_or_else(|| panic!("expected a graph"))
}

fn diagnostics(text: &str) -> Vec<frontend::Diagnostic> {
    load("test.yml", text).diagnostics.into_vec()
}

const CYCLE_XOR_ANY: &str = r#"
name: poll-until-ready
nodes:
  start:
    run: echo "attempt=0" > "$CI_OUTPUT"
    next: poll
  poll:
    join: any
    budget: { max_firings: 5 }
    run: |
      n=$(( ${ATTEMPT:-0} + 1 ))
      echo "attempt=$n" > "$CI_OUTPUT"
      if [ "$n" -ge 3 ]; then echo "ready=true" >> "$CI_OUTPUT"; else echo "ready=false" >> "$CI_OUTPUT"; fi
    config:
      env:
        ATTEMPT: ${{ input.attempt }}
    select:
      - when: ${{ output.ready != 'true' }}
        to: poll
        back: true
      - to: done
  done:
    run: echo ready after ${{ input.attempt }} attempts
"#;

#[test]
fn a_cycle_with_xor_routing_and_an_any_join_lowers() {
    let graph = lower_ok(CYCLE_XOR_ANY);
    let poll = graph.nodes.iter().find(|n| n.name == "poll").unwrap();
    assert_eq!(poll.join, JoinPolicy::Any);
    assert_eq!(poll.routing.groups.len(), 1, "one group: XOR");
    assert_eq!(poll.routing.groups[0].arms.len(), 2);
    assert!(
        poll.routing.groups[0].arms[0].back,
        "the loop arm is a back edge"
    );
    assert!(!poll.routing.groups[0].arms[1].back);
    assert_eq!(poll.budget.max_firings, 5);
    ir::validate(&graph).expect("validates");
}

/// Invariant 8, surfaced with the corollary as its hint.
#[test]
fn an_all_loop_head_gets_the_hinted_diagnostic() {
    let diags = diagnostics(
        r"
nodes:
  start:
    run: echo go
    parallel: [a, b]
  a:
    run: echo a
    next: head
  b:
    run: echo b
    next: head
  head:
    join: all
    budget: { max_firings: 3 }
    run: echo loop
    select:
      - when: ${{ output.again }}
        to: head
        back: true
      - to: done
  done:
    run: echo done
",
    );
    let error = diags
        .iter()
        .find(|d| d.code == "validate.loop_head_must_join_any")
        .expect("invariant 8 is reported");
    assert_eq!(error.severity, Severity::Error);
    assert_eq!(error.span.line, 13, "points at `head`");
    let hint = error.hint.as_deref().expect("has the corollary as a hint");
    assert!(
        hint.contains("join node in front of the loop head"),
        "{hint}"
    );
}

/// Invariant 10: the default `join: all` cannot meet two arms of one
/// `select:`, since only one of them ever emits.
#[test]
fn an_all_join_on_two_arms_of_one_select_gets_the_hinted_diagnostic() {
    let text = |join: &str| {
        format!(
            r#"
nodes:
  classify:
    run: echo size=5 > "$CI_OUTPUT"
    select:
      - when: ${{{{ output.size < 3 }}}}
        to: done
      - to: done
  done:
    join: {join}
    run: echo done
"#
        )
    };
    let diags = diagnostics(&text("all"));
    let error = diags
        .iter()
        .find(|d| d.code == "validate.all_join_exclusive_arms")
        .expect("invariant 10 is reported");
    assert_eq!(error.severity, Severity::Error);
    assert_eq!(error.span.line, 10, "points at `done`");
    let hint = error.hint.as_deref().expect("has a hint");
    assert!(hint.contains("`Any`"), "{hint}");

    lower_ok(&text("any"));
}

/// Invariant 10: a quorum needs as many incoming routes as it counts. After a
/// `for_each`, the clones supply them at run time.
#[test]
fn a_quorum_needs_its_routes_unless_a_for_each_supplies_them() {
    let diags = diagnostics(
        r"
nodes:
  start:
    run: echo go
    parallel: [a, b]
  a:
    run: echo a
    next: gate
  b:
    run: echo b
    next: gate
  gate:
    join: { quorum: 3 }
    run: echo gate
",
    );
    let error = diags
        .iter()
        .find(|d| d.code == "validate.quorum_exceeds_fan_in")
        .expect("invariant 10 is reported");
    assert_eq!(error.severity, Severity::Error);
    assert_eq!(error.span.line, 13, "points at `gate`");

    lower_ok(
        r"
nodes:
  plan:
    run: echo go
    next: deploy
  deploy:
    for_each:
      items: ${{ split('a,b,c', ',') }}
    run: echo deploying
    next: report
  report:
    join: { quorum: 2 }
    run: echo report
",
    );
}

/// A sequential `for_each` in one branch and a plain step in another, meeting
/// at `join: all`: the join matches only a loop that never iterated, so
/// loading warns.
#[test]
fn a_join_after_a_sequential_for_each_and_another_branch_gets_a_warning() {
    let lowered = load(
        "test.yml",
        r"
nodes:
  start:
    run: echo go
    parallel: [plan, side]
  plan:
    run: echo plan
    next: work
  work:
    for_each:
      items: ${{ split('a,b', ',') }}
      parallel: false
    run: echo work
    next: report
  side:
    run: echo side
    next: report
  report:
    run: echo report
",
    );
    assert!(lowered.graph.is_some(), "a warning does not block the load");
    let warning = lowered
        .diagnostics
        .iter()
        .find(|d| d.code == "lint.join_across_generations")
        .expect("the join is warned about");
    assert_eq!(warning.severity, Severity::Warning);
}

/// `quorum: 1` on a loop head is normalized, not rejected.
#[test]
fn quorum_one_on_a_loop_head_is_normalized_to_any() {
    let graph = lower_ok(
        r"
nodes:
  start:
    run: echo go
    next: head
  head:
    join: { quorum: 1 }
    budget: { max_firings: 3 }
    run: echo loop
    select:
      - when: ${{ output.again }}
        to: head
        back: true
      - to: done
  done:
    run: echo done
",
    );
    let head = graph.nodes.iter().find(|n| n.name == "head").unwrap();
    assert_eq!(head.join, JoinPolicy::Any);
}

/// Fan-out takes `parallel:`; `next:` is always one group.
#[test]
fn fan_out_is_explicit() {
    let graph = lower_ok(
        r#"
nodes:
  start:
    run: echo go
    parallel:
      - lint
      - { to: docs, when: "${{ input.docs }}" }
      - - { to: unit, when: "${{ input.fast }}" }
        - { to: integration }
  lint: { run: echo lint }
  docs: { run: echo docs }
  unit: { run: echo unit }
  integration: { run: echo integration }
"#,
    );
    let start = graph.nodes.iter().find(|n| n.name == "start").unwrap();
    assert_eq!(start.routing.groups.len(), 3, "three groups");
    assert_eq!(
        start.routing.groups[2].arms.len(),
        2,
        "the third is a guarded select"
    );
}

#[test]
fn sequential_for_each_desugars_to_the_documented_cycle() {
    let graph = lower_ok(
        r"
nodes:
  plan:
    run: echo plan
    next: deploy
  deploy:
    for_each:
      items: ${{ split(output.regions, ',') }}
      parallel: false
      max_iterations: 10
    run: echo ${{ item }}
    next: report
  report:
    run: echo done
",
    );
    let plan = graph.nodes.iter().find(|n| n.name == "plan").unwrap();
    let deploy = graph.nodes.iter().find(|n| n.name == "deploy").unwrap();
    assert!(
        plan.routing.groups[0].arms[0].map.is_some(),
        "the entry edge carries the loop state"
    );
    assert_eq!(deploy.join, JoinPolicy::Any, "the head joins with any");
    assert_eq!(
        deploy.routing.groups[0].arms.len(),
        2,
        "back arm + exit arm"
    );
    assert!(deploy.routing.groups[0].arms[0].back);
    assert_eq!(deploy.budget.max_firings, 10);
    ir::validate(&graph).expect("validates");
    // `item` in the body became a lookup on the loop state.
    let printed = frontend::print_graph(&graph);
    assert!(printed.contains("input.items[input.idx]"), "{printed}");
}

#[test]
fn parallel_for_each_becomes_an_expansion() {
    let graph = lower_ok(
        r"
nodes:
  plan:
    run: echo plan
    next: deploy
  deploy:
    for_each:
      items: ${{ split(input.regions, ',') }}
      max_parallel: 2
      fail_fast: true
    run: echo ${{ item }}
    next: report
  report:
    join: all
    run: echo done
",
    );
    let deploy = graph.nodes.iter().find(|n| n.name == "deploy").unwrap();
    match &deploy.expand {
        Some(ir::Expansion::ForEach {
            max_parallel,
            fail_fast,
            target,
            ..
        }) => {
            assert_eq!(*max_parallel, Some(2));
            assert!(fail_fast);
            assert_eq!(*target, ir::ExpandTarget::Node);
        }
        other => panic!("expected an expansion, got {other:?}"),
    }
}

#[test]
fn unknown_bindings_and_keys_are_errors_with_hints() {
    let diags = diagnostics(
        r"
nodes:
  a:
    run: echo ${{ github.sha }}
    nxt: b
  b:
    run: echo b
",
    );
    let binding = diags
        .iter()
        .find(|d| d.code == "expr.unknown_binding")
        .expect("unknown binding");
    assert!(
        binding.hint.as_deref().unwrap_or("").contains("nodes"),
        "{:?}",
        binding.hint
    );
    assert!(diags.iter().any(|d| d.code == "yaml.unknown_key"));
}

#[test]
fn malformed_yaml_is_a_diagnostic_not_a_panic() {
    let diags = diagnostics("nodes: [unclosed");
    assert!(diags.iter().any(|d| d.code == "yaml.syntax"));
    let diags = diagnostics("nodes:\n  a:\n    run: echo ${{ 1 +");
    assert!(
        diags.iter().any(|d| d.code.starts_with("expr.")),
        "{diags:?}"
    );
}
