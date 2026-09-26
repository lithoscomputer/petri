//! §3/§4 over random acyclic flows: the join rules hold for every graph and
//! every order a host finishes steps in, not only for the hand-written cases
//! in `joins.rs`. §8 invariant 10 is checked against the same runs: a join it
//! rejects never runs.

mod flow;
mod support;

use std::collections::{BTreeMap, BTreeSet};

use engine::EngineState;
use flow::{FlowCase, JoinSpec};
use ir::{EdgeId, Graph, NodeId, ValidationError, validate};
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

/// The nodes §8 invariant 10 rejects. The generator breaks no other
/// invariant.
fn rejected_joins(graph: &Graph) -> Result<BTreeSet<NodeId>, TestCaseError> {
    let Err(errors) = validate(graph) else {
        return Ok(BTreeSet::new());
    };
    errors
        .iter()
        .map(|error| match error {
            ValidationError::AllJoinExclusiveArms { node, .. } => Ok(*node),
            other => Err(TestCaseError::fail(format!(
                "the generator built an invalid graph: {other}"
            ))),
        })
        .collect()
}

fn check_join_rules(case: &FlowCase) -> Result<(), TestCaseError> {
    let rejected = rejected_joins(&case.graph())?;

    let run = flow::run(case);
    let state = &run.harness.state;
    prop_assert_ne!(&run.observed.status, "unsettled", "every acyclic run ends");

    // At most one firing per (node, generation); every run here is generation 0.
    let mut started = BTreeSet::new();
    for start in &run.starts {
        prop_assert!(started.insert(start.node), "{} started twice", start.node);
    }

    // Sound: a node starts only once its join is satisfied.
    for start in &run.starts {
        let join = case.nodes[start.node.raw() as usize].join;
        let tokens: BTreeSet<EdgeId> = start.inputs.iter().copied().collect();
        prop_assert!(
            satisfies(join, &incoming(state, start.node), &tokens),
            "{} started on {tokens:?} under {join:?}",
            start.node
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

    // Complete: when the run ends, no parked token set satisfies its join.
    let mut parked: BTreeMap<NodeId, BTreeSet<EdgeId>> = BTreeMap::new();
    for ((node, _), token) in state.pending_tokens() {
        parked.entry(node).or_default().insert(token.edge);
    }
    for (node, tokens) in &parked {
        let join = case.nodes[node.raw() as usize].join;
        prop_assert!(!started.contains(node), "{node} fired but kept tokens");
        prop_assert!(
            !satisfies(join, &incoming(state, *node), tokens),
            "{node} was left waiting on {tokens:?} under {join:?}"
        );
    }

    // Every started node finished, and the run failed exactly when one of them
    // failed (`Completion::AnyFailure`).
    prop_assert_eq!(run.observed.finished.len(), run.starts.len());
    let any_failed = run
        .observed
        .finished
        .iter()
        .any(|node| case.nodes[*node as usize].fails);
    prop_assert_eq!(
        run.observed.status.as_str(),
        if any_failed { "failed" } else { "success" }
    );

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

    /// Joins are sound and complete, each node fires at most once, a join
    /// invariant 10 rejects never runs, the run status folds from the
    /// records, and replay is byte-identical — for every generated graph and
    /// host schedule.
    #[test]
    fn random_acyclic_flows_keep_the_join_rules(case in flow::flow_case()) {
        check_join_rules(&case)?;
    }
}
