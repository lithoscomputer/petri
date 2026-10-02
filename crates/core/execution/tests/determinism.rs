//! The execution layer is a function of its inputs under a simulated clock:
//! the same graphs, host and seed store the same logs, byte for byte, the
//! coordinator's and the sandbox resource log included, run after run. The
//! coordinator's loop takes its inputs in a fixed order, every record is
//! stamped from the runtime's recording clock, step output goes where the
//! runtime says, and weighted routing draws come from its seed.

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use execution::host::{self, HostRun};
use ir::{
    Arm, Candidate, Graph, GraphBuilder, Guard, PickPolicy, ScopeId, SelectionPolicy, StepRef,
    Tier, validate,
};
use serde_json::{Value, json};
use support::{
    Call, INVOKE, SIMULATED_EPOCH_MS, SimHost, StoredLogs, digest_of, files_under, run_paused,
};
use testkit::sim::{Faults, SANDBOXED};

const FAULTS: Faults = Faults {
    acquire_failure: 0,
    acquire_ms:      5,
    release_ms:      5,
};

fn sandboxed(work_ms: u64) -> StepRef {
    StepRef::new(SANDBOXED, json!({ "work_ms": [work_ms], "lines": 1 }))
}

/// Two steps, one after the other.
fn child() -> Graph {
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let first = b.add_node("first", scope, sandboxed(5));
    let second = b.add_node("second", scope, sandboxed(10));
    b.link(first, second);
    let graph = b.build();
    validate(&graph).expect("valid");
    graph
}

/// A step, then a call of two children through a one-slot gate, one in its
/// own sandbox and one in the caller's, then a weighted draw between two
/// ends.
fn root(child: &Graph) -> Graph {
    let digest = digest_of(child);
    let mut b = GraphBuilder::new();
    let scope = ScopeId::new(0);
    let start = b.add_node("start", scope, sandboxed(5));
    let calls = [false, true].map(|inherit| Call {
        graph: digest,
        inherit,
        gate: Some(("fork".to_owned(), 1)),
    });
    let call = b.add_node(
        "call",
        scope,
        StepRef::new(INVOKE, json!({ "calls": calls })),
    );
    let pick = b.add_node("pick", scope, sandboxed(5));
    b.link(start, call);
    b.link(call, pick);
    let left = b.add_node("left", scope, sandboxed(5));
    let right = b.add_node("right", scope, sandboxed(5));
    let yes = b.exprs().lit(true);
    let arms = b.select(pick, vec![Arm::when(left, yes), Arm::always(right)]);
    b.node_mut(pick).routing.groups[0].policy = SelectionPolicy::Tiered(vec![Tier {
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

fn run(seed: u64) -> StoredLogs {
    run_paused(async move {
        let host = SimHost::new("execution-determinism", seed, FAULTS);
        let child = child();
        let run = HostRun::new(root(&child)).with_children(vec![child]);
        host::run_configured(&host.runtime(), run, |_, _| {})
            .await
            .expect("the run ends");
        // Step output went to the runtime's store, not to files.
        let files = files_under(host.dir.path());
        assert!(files.is_empty(), "the run wrote {files:?}");
        assert!(host.logs.lines() > 0, "the steps' output reached the store");
        host.stored().await
    })
}

/// The edges the root's weighted draw picked.
fn drawn(logs: &StoredLogs) -> BTreeSet<u64> {
    logs.executions[&0]
        .iter()
        .filter_map(|line| {
            let record: Value = serde_json::from_str(line).expect("a stored line is JSON");
            let group = record["body"]["groups"].get(0)?;
            (!group["draw"].is_null()).then(|| group["decision"]["emit"].as_u64())?
        })
        .collect()
}

/// The same seed twice stores the same logs, byte for byte: the coordinator
/// log, the resource log and each execution's engine log. Different seeds
/// draw both edges of the weighted pick.
#[test]
fn a_seeded_run_stores_the_same_logs_byte_for_byte() {
    let mut reached = BTreeSet::new();
    for seed in 0..16 {
        let first = run(seed);
        assert_eq!(first, run(seed), "seed {seed} ran two ways");
        assert_eq!(
            first.executions.len(),
            3,
            "the root and its two children ran"
        );
        reached.extend(drawn(&first));
    }
    assert_eq!(reached.len(), 2, "the seeds drew {reached:?}");
}

/// Every stored record is stamped from the simulated clock, the coordinator's
/// and the resource log's included.
#[test]
fn every_record_reads_the_simulated_clock() {
    let logs = run(7);
    let stamps = logs
        .coordinator
        .iter()
        .chain(&logs.resources)
        .chain(logs.executions.values().flatten())
        .map(|line| {
            let record: Value = serde_json::from_str(line).expect("a stored line is JSON");
            record["recorded_at"]
                .as_u64()
                .expect("a record has a stamp")
        });
    let limit =
        SIMULATED_EPOCH_MS + u64::try_from(Duration::from_secs(60).as_millis()).unwrap_or(0);
    for stamp in stamps {
        assert!(
            (SIMULATED_EPOCH_MS..limit).contains(&stamp),
            "a record was stamped {stamp}, outside the simulated minute"
        );
    }
    assert!(!logs.resources.is_empty(), "the run recorded its sandboxes");
}
