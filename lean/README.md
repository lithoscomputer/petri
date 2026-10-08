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
| `PetriModel/Flow.lean` | a whole run of the flows the Rust generator makes, loops and budgets included, as routing sees it | `crates/core/engine/tests/flow/mod.rs`, `engine-spec.md` §4 |
| `PetriModel/Retry.lean` | `RetryPolicy` (`should_retry`, `finalize`, `base_delay`) and the retry path of `on_step_finished` | `crates/core/ir/src/graph.rs`, `engine-spec.md` §3.1, §4 |
| `PetriModel/Control.lean` | a whole run with stops: `stop_scope`, `on_kill`, the cancel admission and quiet budget refusal in `try_fire`, the kill checks in `on_token` and `on_step_finished`, and a host that can run a single attempt and hold its decisions open | `crates/core/engine/tests/flow/mod.rs`, `engine-spec.md` §4, §5 |

In the flow model, a back arm starts the next generation, joins match per
`(node, generation)`, and a node fires at most its budget; a key the budget
refuses is marked fired and routes nothing (`engine-spec.md` §4, "Budget
refusal"). Routing sees only the status each firing records; the retry
layer turns a generated case, with its retry policies and scripted attempts,
into that view, and adds the attempt counts and each retry's base delay.

The control model runs the same cases with the host's stops: a cancel or
kill of the root or of a declared group, `run_on_cancel`, the `cancelled()`
guard, a host that honors or ignores a stop signal, and a firing left waiting
on its retry backoff. It follows the engine's order exactly. The core hands
the host an admission for each attempt it starts and a routing for each
outcome it records. A host that answers at once answers them depth first. A
host that holds its decisions, as a driver whose hooks and resolver take
time does, keeps them open, oldest first, until a step answers one, so stops
and finishes land in between. A stop settles a firing waiting on its
admission, as it settles one waiting on its backoff, and withdraws the
decision; a kill also withdraws the open routings of what it reached. The
host also varies what it answers: an admission may be skipped, recording a
success, a failure or a cancel without running, or blocked, which records a
failure and fails the run; a routing group may take another of its arms, or
be blocked. A firing's number among its node's started firings, which picks
its script, is fixed when its first admission is answered. Firing ids order
the signals and the settles. All models leave out expansions, preconditions and splices. The Rust
generator does not produce them either.

The base delay is computed in `Float`, which is IEEE 754 double precision
like Rust's `f64`, with the same order of operations, so the comparison
checks it bit for bit. Nothing about it is proved.

A partial success keeps the failure it was converted from, a timeout
included, in the model and in the engine (`Status::PartialSuccess` carries an
`UnderlyingFailure`). The comparison checks it: a partial success's tag names
its underlying failure (`partial_success/timed_out`).

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

For stops (`PetriModel/Thm/Control.lean`). The control model's run keeps a
log of its stops, starts, retries and routings, and each entry carries the
proof of its rule against the entries before it, so no run of the model can
break one. These theorems read the rules back out of any run:

- `unmarked_never_starts_after_cancel`: after a stop reached a node, or on a
  token from a firing that recorded `cancelled`, the node starts only when it
  is `run_on_cancel`.
- `nothing_starts_after_kill`: nothing starts after a kill reached it.
- `cancelled_not_retried`: a firing is never retried once a stop reached its
  node.
- `killed_routes_nothing`: nothing routes out of a killed closure.
- `stop_status`: a settled run ends `cancelled` exactly when its log holds a
  stop of the root.
- `run_settles`: every run ends with nothing live and no decision open, so
  it never reports `unsettled` (`run_not_unsettled`), even when the host
  holds its decisions, stops land between them, and the host skips, blocks
  or overrides what the core proposes. Two invariants carry it.
  The budget counts stay within the budgets. And every live firing was
  admitted, or its admission is open (`Held`). A stop keeps `Held` only
  because it settles each firing whose admission it withdraws. A kill that
  dropped the open admissions and left their firings live, as the engine's
  once did, breaks the proof. After the schedule, each host step answers a
  decision or finishes a firing, and lowers what the host owes: its open
  decisions, twice its live firings, and three times the firings the budgets
  have left.

For the control model without stops (`PetriModel/Thm/Agree.lean`):

- `control_free_agrees`: a host that only finishes firings, never holds a
  decision and answers each one with the default observes the same run in
  `Control.run` as in `Spec.run`. The theorem assumes no script reports
  `cancelled`, which only a host honoring a stop does. The steps, records,
  attempts, retries, parked tokens, budget refusals and status all match, and
  there is no stop signal and no key completed without running. The proof
  runs both models side by side. The host picks the same firing, runs its
  attempts as the retry model counts them, and admits what the flow model
  delivers, in the same order and with the same firing numbers.
- The flow and retry theorems therefore hold for those runs of the control
  model. `control_free_started_once`, `control_free_budget_exceeded_fails`
  and `control_free_retries_invisible` carry three of them over.

For retries (`PetriModel/Thm/Retry.lean`):

- `attempts_le_limit`: a firing never takes more attempts than its limit.
- `retried_only_retryable`: every attempt before the last is one the policy
  retries; so `success_is_final`: a success always ends the firing.
- `attempts_exhausted`: a firing stops early only on an outcome its policy
  does not retry.
- `accept_partial_keeps_the_failure`: a partial success is recorded only under
  `AcceptPartial`, and keeps the retryable failure it came from.
- `retries_invisible`: a run and the same run with every firing cut to its
  last attempt, one attempt allowed, look the same to routing and the run
  context. Only the attempt counts and the retries differ.

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
  the end, the budget refusals, the run status, each firing's attempts and
  recorded status, each retry's base delay, the stop signals, and the keys
  that completed without running. It runs `Control.run`, stops, held
  decisions and the host's other answers included.
- `deterministic_pick_matches_the_lean_model`: the picked edge, or the
  reason for a refusal. A new refusal message in Rust fails the test until
  the model has it too.

`crates/core/engine/tests/flow_properties.rs` checks the join, generation,
budget, retry, cancel and kill rules on the same generator without Lean, so
it runs in every `mise run test`.

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
