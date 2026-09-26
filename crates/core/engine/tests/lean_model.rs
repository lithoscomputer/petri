//! The Lean model check: the real core agrees with `lean/PetriModel`.
//!
//! Each generated case runs through the Rust engine and through
//! `petri-model`, the executable the Lean project builds, and the two
//! answers must be equal. This is what ties the theorems in
//! `lean/PetriModel/Thm` to the Rust code: they are proved about the same
//! functions this test runs against it.
//!
//! Build the model with `mise run lean:build`. Without it the tests skip,
//! unless `PETRI_REQUIRE_LEAN_MODEL` is set. `PETRI_LEAN_MODEL` names a
//! binary somewhere else.

mod flow;
mod support;

use std::env;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};

use engine::{RoutingCandidate, RoutingProposal, WeightedDraw, deterministic_pick};
use ir::{EdgeId, EdgeTransition, PickPolicy};
use proptest::prelude::*;
use serde_json::{Value, json};
use smol_str::SmolStr;

/// A running `petri-model`: one JSON query per line in, one answer out. It
/// exits when its stdin closes, which is when this test process ends.
struct Model {
    _child: Child,
    input:  ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Model {
    fn start(path: &Path) -> Self {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("could not start {}: {error}", path.display()));
        let input = child.stdin.take().expect("stdin is piped");
        let output = BufReader::new(child.stdout.take().expect("stdout is piped"));
        Self {
            _child: child,
            input,
            output,
        }
    }

    fn ask(&mut self, query: &Value) -> Value {
        writeln!(self.input, "{query}").expect("the model keeps its stdin open");
        self.input.flush().expect("the model keeps its stdin open");
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .expect("the model answers every query");
        let answer: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("the model answered `{line}`: {error}"));
        if let Some(error) = answer.get("error") {
            panic!("the model could not read {query}: {error}");
        }
        answer
    }
}

/// Where the model binary is, or `None` to skip. The skip-or-require
/// convention the Docker batteries use: `PETRI_REQUIRE_LEAN_MODEL` turns a
/// missing binary into a failure.
#[expect(
    clippy::print_stderr,
    reason = "the skip notice belongs to the test runner's output, which no subscriber reads"
)]
fn model_path() -> Option<PathBuf> {
    let path = env::var_os("PETRI_LEAN_MODEL").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../lean/.lake/build/bin/petri-model"),
        PathBuf::from,
    );
    if path.is_file() {
        return Some(path);
    }
    assert!(
        !env::var("PETRI_REQUIRE_LEAN_MODEL").is_ok_and(|v| !v.is_empty()),
        "PETRI_REQUIRE_LEAN_MODEL is set, but there is no model at {}",
        path.display()
    );
    eprintln!(
        "skipping: no Lean model at {} (run `mise run lean:build`)",
        path.display()
    );
    None
}

/// The shared model process, or `None` when there is no binary to run.
fn model() -> Option<&'static Mutex<Model>> {
    static MODEL: OnceLock<Option<Mutex<Model>>> = OnceLock::new();
    MODEL
        .get_or_init(|| model_path().map(|path| Mutex::new(Model::start(&path))))
        .as_ref()
}

fn ask(query: &Value) -> Option<Value> {
    let model = model()?;
    let mut model = model.lock().unwrap_or_else(PoisonError::into_inner);
    Some(model.ask(query))
}

// ── Flows ─────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Which nodes start after each host step, the order they finish in, the
    /// tokens left waiting and the run status all match the model. Acyclic
    /// cases only, until the model has generations and budgets.
    #[test]
    fn flow_runs_match_the_lean_model(case in flow::acyclic_flow_case()) {
        let Some(answer) = ask(&json!({ "flow": &case })) else {
            return Ok(());
        };
        let expected: flow::Observed = serde_json::from_value(answer)
            .expect("the model answers an Observed");
        prop_assert_eq!(flow::run(&case).observed, expected);
    }
}

// ── Deterministic pick ────────────────────────────────────────────────────

fn rank() -> impl Strategy<Value = Option<f64>> {
    prop_oneof![
        Just(None),
        prop_oneof![
            Just(f64::NAN),
            Just(-f64::NAN),
            Just(f64::INFINITY),
            Just(f64::NEG_INFINITY),
            Just(0.0),
            Just(-0.0),
        ]
        .prop_map(Some),
        (-2i8..3).prop_map(|n| Some(f64::from(n))),
        any::<f64>().prop_map(Some),
    ]
}

fn candidate() -> impl Strategy<Value = RoutingCandidate> {
    let weight = prop_oneof![0u32..4, Just(u32::MAX)];
    let target = prop::sample::select(vec!["", "a", "b", "B", "é", "z"]);
    (0u32..6, weight, target, rank()).prop_map(|(edge, weight, target, rank)| RoutingCandidate {
        edge: EdgeId::new(edge),
        weight,
        target: SmolStr::new(target),
        rank,
        transition: EdgeTransition::Continue,
    })
}

fn proposal() -> impl Strategy<Value = RoutingProposal> {
    let pick = prop::option::weighted(
        0.8,
        prop::sample::select(vec![
            PickPolicy::First,
            PickPolicy::HighestWeightThenLexical,
            PickPolicy::WeightedRandom,
            PickPolicy::LowestRankThenArmOrder,
        ]),
    );
    (
        prop::option::of(0u32..2),
        pick,
        prop::collection::vec(candidate(), 0..5),
    )
        .prop_map(|(tier, pick, candidates)| RoutingProposal {
            group: 0,
            tier,
            pick,
            candidates,
        })
}

/// A draw belongs to a weighted pick. Most cases follow that rule with a
/// draw that matches the proposal; the rest break one check each: a stray
/// draw, no draw, the wrong tier, a roll past the total, the wrong total, or
/// a different candidate list.
fn pick_case() -> impl Strategy<Value = (RoutingProposal, Option<WeightedDraw>)> {
    (proposal(), 0u8..10, any::<u64>()).prop_map(|(proposal, mode, noise)| {
        let total: u64 = proposal
            .candidates
            .iter()
            .map(|c| u64::from(c.weight))
            .sum();
        let mut draw = WeightedDraw {
            tier: proposal.tier.unwrap_or(0),
            candidates: proposal.candidates.iter().map(|c| c.edge).collect(),
            roll: if total == 0 { 0 } else { noise % total },
            total,
        };
        let weighted = proposal.pick == Some(PickPolicy::WeightedRandom);
        match (weighted, mode) {
            (false, 0..=7) | (true, 0) => return (proposal, None),
            (false, _) | (true, 1..=4) => {}
            (true, 5) => draw.tier += 1,
            (true, 6) => draw.roll = total.saturating_add(noise % 2),
            (true, 7) => draw.total = total.wrapping_add(1 + noise % 2),
            (true, _) => draw.candidates.push(EdgeId::new(0)),
        }
        (proposal, Some(draw))
    })
}

fn draw_json(draw: &WeightedDraw) -> Value {
    json!({
        "tier": draw.tier,
        "candidates": draw.candidates.iter().map(|edge| edge.raw()).collect::<Vec<_>>(),
        "roll": draw.roll,
        "total": draw.total,
    })
}

/// The query, with each rank as its bit pattern: JSON cannot carry NaN or
/// the infinities.
fn pick_query(proposal: &RoutingProposal, draw: Option<&WeightedDraw>) -> Value {
    let candidates: Vec<Value> = proposal
        .candidates
        .iter()
        .map(|c| {
            json!({
                "edge": c.edge.raw(),
                "weight": c.weight,
                "target": c.target.as_str(),
                "rank": c.rank.map(f64::to_bits),
            })
        })
        .collect();
    json!({
        "pick": {
            "proposal": { "tier": proposal.tier, "pick": proposal.pick, "candidates": candidates },
            "draw": draw.map(draw_json),
        }
    })
}

/// The Rust answer in the model's shape. Each refusal message maps to the
/// model's name for that `Err`; a new message fails here until the model
/// learns it.
fn rust_answer(proposal: &RoutingProposal, draw: Option<&WeightedDraw>) -> Value {
    match deterministic_pick(proposal, draw) {
        Ok(edge) => json!({ "ok": edge.map(EdgeId::raw) }),
        Err(message) => {
            let reason = match message.as_str() {
                "an empty proposal cannot carry a draw" => "draw_on_empty",
                "only weighted random routing may carry a draw" => "draw_on_unweighted",
                "weighted routing requires a draw" => "missing_draw",
                "the draw names the wrong tier" => "wrong_tier",
                "the draw candidate list differs from the proposal" => "candidates_differ",
                "the weighted draw has an invalid total or roll" => "invalid_total",
                "the weighted draw did not select a candidate" => "no_selection",
                other => panic!("`deterministic_pick` has a new refusal the model lacks: {other}"),
            };
            json!({ "refused": reason })
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// Every policy, tie-break, rank (NaN, infinities and signed zeros
    /// included) and draw check picks what the model picks.
    #[test]
    fn deterministic_pick_matches_the_lean_model((proposal, draw) in pick_case()) {
        let Some(expected) = ask(&pick_query(&proposal, draw.as_ref())) else {
            return Ok(());
        };
        prop_assert_eq!(rust_answer(&proposal, draw.as_ref()), expected);
    }
}
