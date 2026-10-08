//! The driver is a function of its inputs under a simulated clock: the same
//! workflow, the same host actions and the same seed give the same log and
//! the same observed record stream, stamps included, byte for byte, run after
//! run. This is what deterministic simulation testing needs from the driver:
//! every choice it makes that the log records comes from the simulation.

mod support;

use std::collections::BTreeSet;
use std::future;
use std::sync::Arc;
use std::time::Duration;

use driver::lifecycle::{PrepareError, PrepareResult, Prepared};
use driver::{ExecutionHooks, HookContext, RunHandle};
use engine::{Event, EventLog, RouteDecision};
use ir::{
    Arm, Backoff, Budget, CancelScopeId, Candidate, Graph, GraphBuilder, Guard, JoinPolicy,
    PickPolicy, RetryPolicy, ScopeId, SelectionPolicy, StepRef, Tier, validate,
};
use serde_json::{Value, json};
use support::RunDir;
use support::sim::{SCRIPTED, SIMULATED_EPOCH_MS, Simulation, Trace, simulate};
use tokio::time;

/// `SplitMix64`: the workflows' and host plans' dice.
struct Dice(u64);

impl Dice {
    fn roll(&mut self, sides: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % sides
    }

    /// A work time from a small set, so steps often finish at the same
    /// virtual instant.
    fn work(&mut self) -> u64 {
        [0, 5, 5, 10, 20][usize::try_from(self.roll(5)).expect("below 5")]
    }
}

fn scripted(config: Value) -> StepRef {
    StepRef::new(SCRIPTED, config)
}

/// An entry that fans out to four steps and a join: one plain, one that fails
/// once and retries after a jittered backoff, one that outlives its timeout,
/// and one that ignores a cancel. The join picks one of two ends at random,
/// weighted; the draw comes from the seed. Work times come from the seed too.
fn workflow(seed: u64) -> Graph {
    let mut dice = Dice(seed);
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_node(
        "start",
        scope,
        scripted(json!({ "work_ms": [dice.work()] })),
    );
    let plain = b.add_node(
        "plain",
        scope,
        scripted(json!({ "work_ms": [dice.work()] })),
    );
    let flaky = b.add_node(
        "flaky",
        scope,
        scripted(json!({
            "work_ms": [dice.work(), dice.work()],
            "outcomes": ["failure", "success"],
        })),
    );
    b.node_mut(flaky).retry = RetryPolicy::attempts(2).with_backoff(Backoff {
        initial: Duration::from_millis(3),
        factor:  2.0,
        max:     Duration::from_secs(1),
        jitter:  true,
    });
    let slow = b.add_node("slow", scope, scripted(json!({ "work_ms": [30] })));
    b.set_budget(slow, Budget::new(1, Duration::from_millis(7)));
    let stubborn = b.add_node(
        "stubborn",
        scope,
        scripted(json!({ "work_ms": [dice.work() + 5], "honor_stop": false })),
    );
    let join = b.add_node("join", scope, scripted(json!({})));
    b.fan_out(start, &[plain, flaky, slow, stubborn]);
    for branch in [plain, flaky, slow, stubborn] {
        b.link(branch, join);
    }
    b.set_join(join, JoinPolicy::Any);
    let left = b.add_node("left", scope, scripted(json!({})));
    let right = b.add_node("right", scope, scripted(json!({})));
    let yes = b.exprs().lit(true);
    let arms = b.select(join, vec![Arm::when(left, yes), Arm::always(right)]);
    b.node_mut(join).routing.groups[0].policy = SelectionPolicy::Tiered(vec![Tier {
        candidates: arms
            .into_iter()
            .map(|edge| Candidate {
                edge,
                when: Guard::Always,
                rank: None,
            })
            .collect(),
        pick:       PickPolicy::WeightedRandom,
    }]);
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// The host's plan: half the seeds cancel the run at a virtual time, and some
/// of those cancel again, which kills it.
async fn host(seed: u64, handle: RunHandle) {
    let mut dice = Dice(seed ^ 0x5EED);
    if dice.roll(2) == 0 {
        return;
    }
    time::sleep(Duration::from_millis(dice.roll(25))).await;
    handle.cancel(CancelScopeId::ROOT).await;
    if dice.roll(2) == 0 {
        time::sleep(Duration::from_millis(dice.roll(5))).await;
        handle.cancel(CancelScopeId::ROOT).await;
    }
}

fn run(dir: &RunDir, seed: u64) -> Trace {
    simulate(
        workflow(seed),
        Simulation::new(dir.path(), seed),
        move |handle| host(seed, handle),
    )
}

/// What a run went through, for checking that the seeds reach every path.
fn paths(log: &EventLog) -> BTreeSet<String> {
    log.events()
        .filter_map(|event| match event {
            Event::RetryElapsed { .. } => Some("a retry".to_owned()),
            Event::CancelRequested { .. } => Some("a cancel".to_owned()),
            Event::KillRequested { .. } => Some("a kill".to_owned()),
            Event::StepFinished { outcome, .. } => {
                Some(format!("a {} attempt", outcome.status.tag()))
            }
            Event::RoutingResolved { groups, .. } => groups
                .iter()
                .find(|group| group.draw.is_some())
                .map(|group| match group.decision {
                    RouteDecision::Emit(edge) => format!("a weighted draw to edge {edge}"),
                    _ => "a weighted draw to nothing".to_owned(),
                }),
            _ => None,
        })
        .collect()
}

/// The same seed twice gives the same run, byte for byte, and different seeds
/// give different runs, through retries, timeouts, cancels, kills and both
/// ends of the weighted draw.
#[test]
fn a_seeded_run_replays_byte_for_byte() {
    let dir = RunDir::new("determinism");
    let mut logs = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for seed in 0..64 {
        let first = run(&dir, seed);
        assert_eq!(first, run(&dir, seed), "seed {seed} ran two ways");
        let log: EventLog = serde_json::from_str(&first.log).expect("the log decodes");
        seen.extend(paths(&log));
        logs.insert(first.log);
    }
    assert!(
        logs.len() > 32,
        "only {} distinct runs from 64 seeds",
        logs.len()
    );
    let draws = seen
        .iter()
        .filter(|path| path.starts_with("a weighted draw to edge"))
        .count();
    for path in [
        "a retry",
        "a cancel",
        "a kill",
        "a failure attempt",
        "a timed_out attempt",
        "a cancelled attempt",
    ] {
        assert!(seen.contains(path), "no seed reached {path}: {seen:?}");
    }
    assert_eq!(draws, 2, "the draws reached {draws} ends: {seen:?}");
}

/// The durations the log records and the stamps observers see come from the
/// simulated clock: acquiring the scope takes the simulation's 10 ms, and the
/// last record is stamped after every step's work.
#[test]
fn the_log_and_its_stamps_read_the_simulated_clock() {
    let dir = RunDir::new("determinism-clock");
    let trace = simulate(workflow(7), Simulation::new(dir.path(), 7), |_| async {});
    let log: EventLog = serde_json::from_str(&trace.log).expect("the log decodes");
    let acquired = log.events().find_map(|event| match event {
        Event::ScopeAcquired { duration_ms, .. } => Some(*duration_ms),
        _ => None,
    });
    assert_eq!(acquired, Some(10), "the acquisition took simulated time");
    let (_, first, _) = trace.records.first().expect("a record");
    let (_, last, _) = trace.records.last().expect("a record");
    assert!(*first >= SIMULATED_EPOCH_MS);
    assert!(
        *last >= SIMULATED_EPOCH_MS + 10 + 30,
        "the last stamp is {last}, before the slowest work ended"
    );
}

/// A host that prepares the entry's result and never answers for any other.
struct PreparesOnlyStart;

#[async_trait::async_trait]
impl ExecutionHooks for PreparesOnlyStart {
    async fn prepare_result(
        &self,
        _context: &HookContext,
        request: PrepareResult,
    ) -> Result<Prepared, PrepareError> {
        if request.view.node.name == "start" {
            Ok(Prepared::unchanged())
        } else {
            future::pending().await
        }
    }
}

/// A kill records every result its host was still preparing, in firing
/// order: the order does not depend on how the driver stores them.
#[test]
fn a_kill_records_the_results_it_abandons_in_firing_order() {
    let dir = RunDir::new("determinism-abandon");
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_node("start", scope, scripted(json!({})));
    let branches: Vec<_> = (0..4)
        .map(|index| {
            b.add_node(
                &format!("b{index}"),
                scope,
                scripted(json!({ "work_ms": [5] })),
            )
        })
        .collect();
    b.fan_out(start, &branches);
    let graph = b.build();
    validate(&graph).expect("valid");

    let kill = |handle: RunHandle| async move {
        time::sleep(Duration::from_millis(50)).await;
        handle.cancel(CancelScopeId::ROOT).await;
        handle.cancel(CancelScopeId::ROOT).await;
    };
    let mut traces = Vec::new();
    for _ in 0..8 {
        let mut sim = Simulation::new(dir.path(), 0);
        sim.hooks = Some(Arc::new(PreparesOnlyStart));
        traces.push(simulate(graph.clone(), sim, kill));
    }
    let log: EventLog = serde_json::from_str(&traces[0].log).expect("the log decodes");
    let finished: Vec<u64> = log
        .events()
        .filter_map(|event| match event {
            Event::StepFinished { firing, .. } => Some(firing.raw()),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 5, "start and the four abandoned branches");
    assert!(
        finished.is_sorted(),
        "the abandoned results were recorded out of firing order: {finished:?}"
    );
    assert!(traces.windows(2).all(|pair| pair[0] == pair[1]));
}
