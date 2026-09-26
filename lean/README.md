# Lean model of the engine core

A Lean 4 model of parts of Petri's engine core, the theorems proved about it,
and an executable that `crates/core/engine/tests/lean_model.rs` checks the
real core against.

This follows the approach AWS used for Cedar. The Lean definitions are the
specification. The theorems are proved about those definitions. A
property-based test runs the same definitions against the Rust code, so the
proofs describe what the Rust code does on every case the test generates.
Nothing here translates Rust into Lean.

## What is modeled

| Lean | Rust | Source of truth |
| --- | --- | --- |
| `PetriModel/Join.lean` | `on_token`, `try_fire`, `is_join_satisfied` in `crates/core/engine/src/apply.rs` | `engine-spec.md` §3, §4 |
| `PetriModel/Pick.lean` | `deterministic_pick` in `crates/core/engine/src/apply.rs` | `engine-spec.md` §2, §6 |
| `PetriModel/Flow.lean` | a whole run of the flows the Rust generator makes, loops and budgets included | `crates/core/engine/tests/flow/mod.rs`, `engine-spec.md` §4 |

In the flow model, a back arm starts the next generation, joins match per
`(node, generation)`, and a node fires at most its budget; a key the budget
refuses is marked fired and routes nothing (`engine-spec.md` §4, "Budget
refusal"). The model leaves out retries, cancellation, expansions,
preconditions and splices. The Rust generator does not produce them either.

A rank in `deterministic_pick` is an `f64` compared with `f64::total_cmp`.
The model carries the rank's bit pattern and compares the same key
`total_cmp` uses, so NaN, the infinities and signed zeros agree with Rust.

## What is proved

All theorems are complete. None uses `sorry`, and none depends on axioms
beyond Lean's standard three (`propext`, `Classical.choice`, `Quot.sound`).

For one `(node, generation)` key (`PetriModel/Thm/Join.lean`):

- `fires_at_most_once`: whatever tokens arrive, the node fires at most once.
- `fired_iff`: after a sequence of arrivals, the key has fired exactly when
  the distinct edges seen satisfy the join.
- `fired_depends_only_on_edges`: whether a key fires depends only on which
  edges delivered a token, not on arrival order or duplicates. The driver's
  completion order is not deterministic, and this is why a join does not
  depend on it.
- `all_fires_iff`, `any_fires_iff`, `quorum_fires_iff`: the three policies,
  stated over the arrivals.
- `all_never_fires`: an `All` join that counts two edges of which at most
  one ever delivers never fires. Two arms of one routing group are such a
  pair, and `engine-spec.md` §8 invariant 10 rejects that join at load.
  `crates/core/engine/tests/flow_properties.rs` checks the rule against the
  real core: a node it rejects never runs.
- `quorum_never_fires_by_groups`: a `Quorum n` fed by fewer than `max n 1`
  routing groups never fires, since each group delivers at most one edge.
  Invariant 10 rejects that join too, and `flow_properties.rs` checks that
  such a node never runs. `quorum_never_fires` is the case of one group per
  edge.

For whole runs, loops included (`PetriModel/Thm/Flow.lean`):

- `started_once`: in any run, each `(node, generation)` key starts at most
  once.
- `token_generation`: a routed token comes from an arm of the firing's node,
  and its generation is the firing's, plus one on a back arm.
- `firings_le_budget`: no node fires more often than its budget.
- `budget_exceeded_fails`: a run in which a budget refused a firing does not
  report success.
- `run_settles`: every run ends with nothing live within the host steps its
  budgets allow (their sum, plus one), so `run` never reports `unsettled`
  (`run_not_unsettled`).

For `deterministic_pick` (`PetriModel/Thm/Pick.lean`):

- `select_count`: a weighted draw is proportional. Of the rolls
  `0 ≤ roll < total`, exactly `weight i` pick candidate `i`.
- `select_weight_pos`: a zero-weight candidate is never picked.
- `pick_weighted_ok`: a draw that matches its proposal is never refused. The
  Rust function's last `Err` ("the weighted draw did not select a
  candidate") cannot happen.
- `pick_mem`: whatever the policy, a pick names one of the candidates.
- `highest_max`: `HighestWeightThenLexical` picks a heaviest candidate.
- `lowest_min`, `lowest_eq_none`: `LowestRankThenArmOrder` picks a candidate
  with the smallest rank key, and picks nothing only when no candidate has a
  rank.

## How the Rust code is checked

`petri-model` reads one JSON query per line and writes one answer per line
(`PetriModel/Wire.lean`). `crates/core/engine/tests/lean_model.rs` starts it
once per test and, for each generated case, compares its answer with the
real core's:

- `flow_runs_match_the_lean_model`: which `(node, generation)` firings start
  after each host step, the order they finish in, the tokens left waiting at
  the end, the budget refusals, and the run status.
- `deterministic_pick_matches_the_lean_model`: the picked edge, or the
  reason for a refusal. A new refusal message in Rust fails the test until
  the model has it too.

`crates/core/engine/tests/flow_properties.rs` checks the join, generation
and budget rules on the same generator without Lean, so it runs in every
`mise run test`.

## Commands

Install [elan](https://github.com/leanprover/elan). It reads the Lean
version from `lean-toolchain`.

```sh
mise run lean:build   # build the model and check every proof
mise run test:lean    # build, then check the core against the model
```

Without a built model, `lean_model.rs` skips. `PETRI_REQUIRE_LEAN_MODEL=1`
turns the skip into a failure, and `mise run test:lean` sets it; the required
`lean model` CI job runs that task. `PETRI_LEAN_MODEL` names a binary
somewhere else.
