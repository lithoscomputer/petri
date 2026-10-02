//! §3/§4/§5 over random flows, loops, retries, stops and decisions the host
//! holds open included: the join, generation, budget, retry, cancel and kill
//! rules hold for every graph and every order a host takes its steps and
//! answers its decisions in, not only for the hand-written cases in
//! `joins.rs`, `loops.rs`, `retries.rs` and `cancellation.rs`. §8 invariant 10
//! is checked against the same runs: a join it rejects never runs.

mod flow;
mod support;

use std::collections::{BTreeMap, BTreeSet};

use engine::{Command, EngineState, Event, RunError};
use flow::{FlowCase, JoinSpec, Run, Start, Stop, Target};
use ir::{
    CancelScopeId, EdgeId, FiringId, Graph, NodeId, Status, UnderlyingFailure, ValidationError,
    validate,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{TestCaseError, TestRunner};

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

/// A run's firings: every attempt started, grouped by firing, and each
/// firing's first start in order.
struct Firings<'a> {
    attempts: BTreeMap<FiringId, Vec<&'a Start>>,
    in_order: Vec<&'a Start>,
}

/// A firing's attempts are numbered 1, 2, … with no gap.
fn firings(run: &Run) -> Result<Firings<'_>, TestCaseError> {
    let mut firings = Firings {
        attempts: BTreeMap::new(),
        in_order: Vec::new(),
    };
    for start in &run.starts {
        let seen = firings.attempts.entry(start.firing).or_default();
        prop_assert_eq!(
            start.attempt as usize,
            seen.len() + 1,
            "{} attempt {} out of order",
            start.firing,
            start.attempt
        );
        if seen.is_empty() {
            firings.in_order.push(start);
        }
        seen.push(start);
    }
    Ok(firings)
}

/// Joins are sound per generation, a key starts at most once, a join
/// invariant 10 rejects never runs, and when the run ends no parked token
/// set satisfies its join.
fn check_joins(
    case: &FlowCase,
    run: &Run,
    firings: &Firings<'_>,
    rejected: &BTreeSet<NodeId>,
) -> Result<(), TestCaseError> {
    let state = &run.harness.state;
    let mut started = BTreeSet::new();
    for start in &firings.in_order {
        prop_assert!(
            started.insert((start.node, start.generation)),
            "{} started twice in generation {}",
            start.node,
            start.generation
        );
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

    // A key records at most once, whether it ran or completed without
    // running.
    let mut recorded = BTreeSet::new();
    for record in state.history() {
        prop_assert!(
            recorded.insert((record.node, record.generation)),
            "{} recorded twice in generation {}",
            record.node,
            record.generation
        );
    }

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
    Ok(())
}

/// A token's generation is its source firing's, plus one on a back arm. A
/// seed token starts generation 0. A firing that completed without running
/// routes too, so sources come from the records as well as the starts.
fn check_generations(run: &Run) -> Result<(), TestCaseError> {
    let state = &run.harness.state;
    let generation_of: BTreeMap<FiringId, u32> = run
        .starts
        .iter()
        .map(|start| (start.firing, start.generation))
        .chain(
            state
                .history()
                .iter()
                .map(|record| (record.firing, record.generation.raw())),
        )
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
                "a token came from {}, which never started or recorded",
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
    Ok(())
}

/// No node fires more often than its budget allows, counting the firings
/// that completed without running. A refused firing comes only after the
/// budget is spent, and only a budget raises an engine error.
fn check_budgets(case: &FlowCase, run: &Run) -> Result<(), TestCaseError> {
    let state = &run.harness.state;
    let mut firings: BTreeMap<NodeId, u32> = BTreeMap::new();
    for record in state.history() {
        *firings.entry(record.node).or_default() += 1;
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
    // Only a key that would run is an error. Once a stop reached an unmarked
    // node, its keys only complete `Cancelled`, so its budget stops them
    // quietly (§4, "Budget refusal").
    for (step, node) in &run.refusals {
        prop_assert!(
            case.nodes[node.index()].run_on_cancel
                || !run
                    .stops
                    .iter()
                    .any(|stop| stop.step <= *step && case.covers(stop.target, node.raw())),
            "the budget refused unmarked {} with an error in step {}, after a stop reached it",
            node,
            step
        );
    }
    Ok(())
}

/// Whether a stop covering `node` came before the host step `step`: after
/// it, the node's firings are cancelled, and are never retried (§5).
fn stopped_before(case: &FlowCase, run: &Run, node: NodeId, step: usize) -> bool {
    run.stops
        .iter()
        .any(|stop| stop.step < step && case.covers(stop.target, node.raw()))
}

/// A firing retries only an outcome its policy retries, never a success,
/// never past its attempt limit, and never once a stop reached it; it stops
/// at the first outcome it does not retry; and its record is the last
/// attempt after the exhaustion policy, or `Cancelled` when a stop settled it
/// during its backoff. An accepted partial success keeps the failure it came
/// from, a timeout included (§3.1 rule 3). The rules are restated in
/// `RetrySpec`, not read from the core's `RetryPolicy`.
fn check_retries(case: &FlowCase, run: &Run, firings: &Firings<'_>) -> Result<(), TestCaseError> {
    let state = &run.harness.state;
    for (firing, tries) in &firings.attempts {
        let node = tries[0].node;
        let retry = &case.nodes[node.index()].retry;
        prop_assert!(
            tries.len() <= retry.max_attempts as usize,
            "{firing} tried too often"
        );
        for tried in &tries[..tries.len() - 1] {
            let reported = tried.reported.unwrap_or_default();
            prop_assert!(
                retry.retries(tried.outcome),
                "{firing} retried {:?}",
                tried.outcome
            );
            prop_assert!(
                !stopped_before(case, run, node, reported),
                "{firing} was retried after a stop reached it"
            );
        }
        let last = tries[tries.len() - 1];
        let settled = run.settled.contains_key(firing);
        let stopped = last
            .reported
            .is_some_and(|step| stopped_before(case, run, node, step));
        prop_assert!(
            settled
                || stopped
                || !retry.retries(last.outcome)
                || last.attempt == retry.max_attempts,
            "{firing} stopped at attempt {} with {:?} left to retry",
            last.attempt,
            last.outcome
        );
        let raw = last.outcome.outcome();
        let expected = if settled {
            Status::Cancelled
        } else {
            retry.recorded(last.attempt, last.outcome)
        };
        let record = state
            .history()
            .iter()
            .rev()
            .find(|record| record.firing == *firing);
        prop_assert!(record.is_some(), "{firing} has no record");
        let record = &record.expect("checked above").outcome.status;
        prop_assert_eq!(record, &expected, "{}'s record", firing);
        if let Status::PartialSuccess { underlying } = record {
            prop_assert_eq!(
                underlying,
                &UnderlyingFailure::of(&raw.status),
                "{}'s partial success lost its failure",
                firing
            );
        }
    }

    // Retries are invisible outside the log (§4): the same case with every
    // firing cut to its final outcome runs the same way. Only when the host
    // finishes every firing in one step: a single attempt or a stop can land
    // between two attempts.
    if case.only_finishes() {
        let finalized = flow::run(&case.finalized());
        prop_assert_eq!(finalized.observed.routing(), run.observed.routing());
    }
    Ok(())
}

/// Cancel and kill (§5), checked against what the host saw at each stop.
fn check_stops(case: &FlowCase, run: &Run, firings: &Firings<'_>) -> Result<(), TestCaseError> {
    let state = &run.harness.state;
    let node_of: BTreeMap<FiringId, NodeId> = run
        .starts
        .iter()
        .map(|start| (start.firing, start.node))
        .chain(
            state
                .history()
                .iter()
                .map(|record| (record.firing, record.node)),
        )
        .collect();
    let cancelled = |firing: &FiringId| {
        state
            .history()
            .iter()
            .any(|record| record.firing == *firing && record.outcome.status == Status::Cancelled)
    };

    // Unmarked work never starts after a cancel reaches it, and nothing
    // starts after a kill reaches it. A stop's own step counts: what it
    // starts, it starts after the stop.
    for start in &firings.in_order {
        let node = &case.nodes[start.node.index()];
        for stop in &run.stops {
            if stop.step > start.step || !case.covers(stop.target, start.node.raw()) {
                continue;
            }
            prop_assert!(
                stop.stop != Stop::Kill,
                "{} started in step {} after a kill in step {}",
                start.node,
                start.step,
                stop.step
            );
            prop_assert!(
                node.run_on_cancel,
                "unmarked {} started in step {} after a cancel in step {}",
                start.node,
                start.step,
                stop.step
            );
        }
        // Work fed by cancelled work is admitted the same way.
        prop_assert!(
            node.run_on_cancel || !start.sources.iter().any(cancelled),
            "unmarked {} started on the tokens of a cancelled firing",
            start.node
        );
    }

    // A key that completes without running is unmarked work a stop reached,
    // directly or through its inputs, or a firing a stop settled while its
    // admission was open, and it records `Cancelled`.
    let started: BTreeSet<FiringId> = run.starts.iter().map(|start| start.firing).collect();
    let settled_admitting = |record: &engine::FiringRecord| {
        run.stops.iter().any(|stop| {
            stop.admitting.contains(&record.firing) && case.covers(stop.target, record.node.raw())
        })
    };
    for record in state
        .history()
        .iter()
        .filter(|record| !started.contains(&record.firing))
    {
        prop_assert_eq!(
            &record.outcome.status,
            &Status::Cancelled,
            "{} completed without running",
            record.node
        );
        prop_assert!(
            settled_admitting(record)
                || !case.nodes[record.node.index()].run_on_cancel && !run.stops.is_empty(),
            "{} completed without running, but it is marked or nothing was stopped",
            record.node
        );
    }

    for stop in &run.stops {
        let covered = |firing: &FiringId| case.covers(stop.target, node_of[firing].raw());
        // Each live firing the stop reaches gets one signal, unless it waits
        // on its backoff or its admission, which the stop settles instead. A
        // cancel skips a firing already signalled; a kill reaches it anyway.
        let expected: BTreeSet<FiringId> = stop
            .live
            .iter()
            .filter(|firing| {
                covered(firing)
                    && !stop.waiting.contains(firing)
                    && !stop.admitting.contains(firing)
            })
            .filter(|firing| stop.stop == Stop::Kill || !stop.signalled.contains(firing))
            .copied()
            .collect();
        let delivered: BTreeSet<FiringId> = run
            .controls
            .iter()
            .filter(|control| control.step == stop.step)
            .map(|control| control.firing)
            .collect();
        prop_assert_eq!(
            &delivered,
            &expected,
            "the signals of the {:?} in step {}",
            stop.stop,
            stop.step
        );
        prop_assert!(
            run.controls
                .iter()
                .filter(|control| control.step == stop.step)
                .all(|control| control.stop == stop.stop),
            "a {:?} sent the other tier",
            stop.stop
        );
        // A firing waiting on its backoff is settled at once, and records
        // `Cancelled`; so is one waiting on its admission, started before or
        // not.
        for firing in stop.waiting.iter().filter(|firing| covered(firing)) {
            prop_assert_eq!(
                run.settled.get(firing),
                Some(&stop.step),
                "{} waited on its backoff through the {:?} in step {}",
                firing,
                stop.stop,
                stop.step
            );
        }
        for firing in &stop.admitting {
            let record = state
                .history()
                .iter()
                .find(|record| record.firing == *firing);
            let Some(record) = record else {
                continue;
            };
            if !case.covers(stop.target, record.node.raw()) {
                continue;
            }
            prop_assert!(
                !stop.live.contains(firing) || run.settled.get(firing) == Some(&stop.step),
                "{} waited on its admission through the {:?} in step {}",
                firing,
                stop.stop,
                stop.step
            );
            prop_assert_eq!(
                &record.outcome.status,
                &Status::Cancelled,
                "{} settled awaiting its admission",
                firing
            );
        }
    }
    prop_assert!(
        run.controls
            .iter()
            .all(|control| run.stops.iter().any(|stop| stop.step == control.step)),
        "only a stop signals a firing"
    );
    prop_assert!(
        run.settled.keys().all(cancelled),
        "a settled firing records `Cancelled`"
    );

    // After a kill, no token leaves the killed closure, and none is left
    // parked inside it.
    let mut killed: BTreeSet<NodeId> = BTreeSet::new();
    let mut everything_killed = false;
    for event in state.log.events() {
        match event {
            Event::KillRequested { scope } if *scope == CancelScopeId::ROOT => {
                everything_killed = true;
            }
            Event::KillRequested { scope } => {
                killed.extend(
                    state
                        .cancel_scope(*scope)
                        .into_iter()
                        .flat_map(|scope| scope.nodes.iter().copied()),
                );
            }
            Event::TokenEmitted { token } => {
                if let Some(node) = node_of.get(&token.from) {
                    prop_assert!(
                        !everything_killed && !killed.contains(node),
                        "{} routed after a kill reached it",
                        node
                    );
                }
            }
            _ => {}
        }
    }
    for ((node, generation), token) in state.pending_tokens() {
        prop_assert!(
            !everything_killed && !killed.contains(&node),
            "the token on edge {} is parked at killed {} in generation {}",
            token.edge,
            node,
            generation.raw()
        );
    }
    Ok(())
}

fn check_flow(case: &FlowCase) -> Result<(), TestCaseError> {
    let rejected = rejected_joins(&case.graph())?;
    let run = flow::run(case);
    let state = &run.harness.state;
    prop_assert_ne!(&run.observed.status, "unsettled", "every run ends");

    let firings = firings(&run)?;
    check_joins(case, &run, &firings, &rejected)?;
    check_generations(&run)?;
    check_budgets(case, &run)?;
    check_retries(case, &run, &firings)?;
    check_stops(case, &run, &firings)?;

    // Every started firing finished or was settled. A stop of the root ends
    // the run `cancelled`, ahead of every failure; otherwise the run failed
    // exactly when a record is a failure or a budget refused a firing
    // (`Completion::AnyFailure`).
    prop_assert_eq!(run.observed.finished.len(), firings.in_order.len());
    let any_failed = state
        .history()
        .iter()
        .any(|record| record.outcome.status.is_failure());
    let expected = if run.stops.iter().any(|stop| stop.target == Target::Root) {
        "cancelled"
    } else if any_failed || !run.observed.budget_exceeded.is_empty() {
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

/// The generated cases reach each path a held decision opens, so the rules
/// above are checked on them: a stop settles a firing waiting on its first
/// admission, and one waiting on its next attempt's, and a kill of the root
/// or of a group withdraws the routings still open inside it.
#[test]
fn held_decisions_reach_their_paths() {
    let mut runner = TestRunner::deterministic();
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for _ in 0..2048 {
        let case = flow::flow_case()
            .new_tree(&mut runner)
            .expect("a case generates")
            .current();
        let run = flow::run(&case);
        let state = &run.harness.state;
        let started: BTreeSet<FiringId> = run.starts.iter().map(|start| start.firing).collect();
        for stop in &run.stops {
            for firing in &stop.admitting {
                let covered = state.history().iter().any(|record| {
                    record.firing == *firing && case.covers(stop.target, record.node.raw())
                });
                let path = if run.settled.get(firing) == Some(&stop.step) {
                    "a stop settled a firing waiting on its next attempt's admission"
                } else if covered && !started.contains(firing) {
                    "a stop settled a firing waiting on its first admission"
                } else {
                    continue;
                };
                *seen.entry(path).or_default() += 1;
            }
        }
        for (step, command) in &run.withdrawn {
            let Command::ResolveRouting { .. } = command else {
                continue;
            };
            let stop = run
                .stops
                .iter()
                .find(|stop| stop.step == *step)
                .expect("only a stop withdraws a decision");
            let path = match stop.target {
                Target::Root => "a root kill withdrew an open routing",
                Target::Group(_) => "a group kill withdrew an open routing",
            };
            *seen.entry(path).or_default() += 1;
        }
    }
    for path in [
        "a stop settled a firing waiting on its first admission",
        "a stop settled a firing waiting on its next attempt's admission",
        "a root kill withdrew an open routing",
        "a group kill withdrew an open routing",
    ] {
        assert!(
            seen.get(path).copied().unwrap_or_default() > 0,
            "no case reached {path}: {seen:?}"
        );
    }
}

proptest! {
    // Stops reach some rules only through a few steps in a row, a cancel
    // group's cancelled work admitted outside it among them; 2048 cases run in
    // about a second and meet each of them tens of times.
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// Joins are sound and complete per generation, each (node, generation)
    /// fires at most once, tokens carry the right generation, budgets hold,
    /// retries follow the policy and are invisible outside the log, a cancel
    /// or kill admits, signals, settles and routes as §5 says, a join
    /// invariant 10 rejects never runs, the run status folds from the stops,
    /// the records and the budget errors, and replay is byte-identical — for
    /// every generated graph and host schedule.
    #[test]
    fn random_flows_keep_the_join_generation_budget_retry_and_stop_rules(case in flow::flow_case()) {
        check_flow(&case)?;
    }
}
