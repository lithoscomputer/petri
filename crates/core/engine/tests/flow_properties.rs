//! §3/§4 over random flows, loops included: the join, generation and budget
//! rules hold for every graph and every order a host finishes steps in, not
//! only for the hand-written cases in `joins.rs` and `loops.rs`. §8 invariant
//! 10 is checked against the same runs: a join it rejects never runs.

mod flow;
mod support;

use std::collections::{BTreeMap, BTreeSet};

use engine::{EngineState, Event, RunError};
use flow::{FlowCase, JoinSpec};
use ir::{EdgeId, FiringId, Graph, NodeId, ValidationError, validate};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

/// The edges that count toward a node's join: its declared incoming edges
/// plus the seed edge an entry node gets.
fn incoming(state: &EngineState, node: NodeId) -> BTreeSet<EdgeId> {
    state
        .graph()
        .edges()
        .filter(|edge| edge.to == node)
        .map(|edge| edge.id)
        .chain(
            state
                .seed_edges()
                .filter(|(_, target)| *target == node)
                .map(|(edge, _)| edge),
        )
        .collect()
}

/// §3 `JoinPolicy`, restated independently of the core.
fn satisfies(join: JoinSpec, incoming: &BTreeSet<EdgeId>, tokens: &BTreeSet<EdgeId>) -> bool {
    !tokens.is_empty()
        && match join {
            JoinSpec::All => incoming.is_subset(tokens),
            JoinSpec::Any => true,
            JoinSpec::Quorum { n } => tokens.len() >= n.max(1) as usize,
        }
}

/// The joins §8 invariant 10 rejects. The generator also breaks invariant 8
/// on purpose; that rule is deliberately strict, and a loop head it rejects
/// can still fire in some generations, so there is nothing to check at run
/// time. It breaks no other rule.
fn rejected_joins(graph: &Graph) -> Result<BTreeSet<NodeId>, TestCaseError> {
    let mut joins = BTreeSet::new();
    let Err(errors) = validate(graph) else {
        return Ok(joins);
    };
    for error in &errors {
        match error {
            ValidationError::AllJoinExclusiveArms { node, .. }
            | ValidationError::QuorumExceedsFanIn { node, .. } => {
                joins.insert(*node);
            }
            ValidationError::LoopHeadMustJoinAny(_) => {}
            other => {
                return Err(TestCaseError::fail(format!(
                    "the generator built an invalid graph: {other}"
                )));
            }
        }
    }
    Ok(joins)
}

fn check_flow(case: &FlowCase) -> Result<(), TestCaseError> {
    let rejected = rejected_joins(&case.graph())?;
    let run = flow::run(case);
    let state = &run.harness.state;
    prop_assert_ne!(&run.observed.status, "unsettled", "every run ends");

    // At most one firing per (node, generation).
    let mut started = BTreeSet::new();
    for start in &run.starts {
        prop_assert!(
            started.insert((start.node, start.generation)),
            "{} started twice in generation {}",
            start.node,
            start.generation
        );
    }

    // Sound: a node starts only once its join is satisfied.
    for start in &run.starts {
        let join = case.nodes[start.node.index()].join;
        let tokens: BTreeSet<EdgeId> = start.inputs.iter().copied().collect();
        prop_assert!(
            satisfies(join, &incoming(state, start.node), &tokens),
            "{} started in generation {} on {tokens:?} under {join:?}",
            start.node,
            start.generation
        );
    }

    // Invariant 10 rejects only joins that can never run. The core runs a
    // graph whether or not it validated, and never starts one.
    for start in &run.starts {
        prop_assert!(
            !rejected.contains(&start.node),
            "invariant 10 rejected {}, which ran",
            start.node
        );
    }

    // Generations: a token's generation is its source firing's, plus one on a
    // back arm. A seed token starts generation 0.
    let generation_of: BTreeMap<FiringId, u32> = run
        .starts
        .iter()
        .map(|start| (start.firing, start.generation))
        .collect();
    let back: BTreeSet<EdgeId> = state
        .graph()
        .edges()
        .filter(|edge| edge.back)
        .map(|edge| edge.id)
        .collect();
    let seeds: BTreeSet<EdgeId> = state.seed_edges().map(|(edge, _)| edge).collect();
    for event in state.log.events() {
        let Event::TokenEmitted { token } = event else {
            continue;
        };
        let expected = if seeds.contains(&token.edge) {
            0
        } else {
            let source = generation_of.get(&token.from).copied();
            prop_assert!(
                source.is_some(),
                "a token came from {}, which never started",
                token.from
            );
            source.unwrap_or_default() + u32::from(back.contains(&token.edge))
        };
        prop_assert_eq!(
            token.generation.raw(),
            expected,
            "the token on edge {}",
            token.edge
        );
    }

    // Budgets: no node fires more often than its budget allows. A refused
    // firing comes only after the budget is spent, and it fails the run.
    let mut firings: BTreeMap<NodeId, u32> = BTreeMap::new();
    for start in &run.starts {
        *firings.entry(start.node).or_default() += 1;
    }
    for (node, count) in &firings {
        let budget = case.nodes[node.index()].max_firings;
        prop_assert!(
            *count <= budget,
            "{node} fired {count} times on a budget of {budget}"
        );
    }
    for node in &run.observed.budget_exceeded {
        let budget = case.nodes[*node as usize].max_firings;
        let count = firings
            .get(&NodeId::new(*node))
            .copied()
            .unwrap_or_default();
        prop_assert_eq!(
            count,
            budget,
            "n{} was refused before its budget was spent",
            node
        );
    }
    prop_assert!(
        state
            .errors()
            .iter()
            .all(|error| matches!(error, RunError::BudgetExceeded { .. })),
        "only a budget can stop a generated run: {:?}",
        state.errors()
    );

    // Complete: when the run ends, no parked token set satisfies its join.
    let mut parked: BTreeMap<(NodeId, u32), BTreeSet<EdgeId>> = BTreeMap::new();
    for ((node, generation), token) in state.pending_tokens() {
        parked
            .entry((node, generation.raw()))
            .or_default()
            .insert(token.edge);
    }
    for ((node, generation), tokens) in &parked {
        let join = case.nodes[node.index()].join;
        prop_assert!(
            !started.contains(&(*node, *generation)),
            "{node} fired in generation {generation} but kept tokens"
        );
        prop_assert!(
            !satisfies(join, &incoming(state, *node), tokens),
            "{node} was left waiting in generation {generation} on {tokens:?} under {join:?}"
        );
    }

    // Every started firing finished, and the run failed exactly when a firing
    // failed or a budget refused one (`Completion::AnyFailure`).
    prop_assert_eq!(run.observed.finished.len(), run.starts.len());
    let any_failed = run.starts.iter().any(|start| start.fails);
    let expected = if any_failed || !run.observed.budget_exceeded.is_empty() {
        "failed"
    } else {
        "success"
    };
    prop_assert_eq!(run.observed.status.as_str(), expected);

    prop_assert!(
        state.held_scopes().next().is_none(),
        "a finished run holds no scopes"
    );
    if let Err(mismatch) = engine::verify_replay(run.harness.original_graph.clone(), &state.log) {
        return Err(TestCaseError::fail(format!("replay diverged: {mismatch}")));
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Joins are sound and complete per generation, each (node, generation)
    /// fires at most once, tokens carry the right generation, budgets hold, a
    /// join invariant 10 rejects never runs, the run status folds from the
    /// records and the budget errors, and replay is byte-identical — for
    /// every generated graph and host schedule.
    #[test]
    fn random_flows_keep_the_join_generation_and_budget_rules(case in flow::flow_case()) {
        check_flow(&case)?;
    }
}
