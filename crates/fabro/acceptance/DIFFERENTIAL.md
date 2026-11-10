# The differential matrix

Black box phase 5: every scenario runs through the shipped `petri` binary
and through the pinned Fabro binary with the same bundle, inputs, provider
twins and interview script, and the two runs are compared under fixed rules.
This page says how the reference binary is provisioned, how the Fabro
adapter drives it, what is compared, how differences are decided, and how a
baseline is refreshed.

Run it with `mise run test:fabro:differential`. The test files are
`crates/petri/cli/tests/fabro_differential.rs` (the cells) and
`crates/fabro/acceptance/tests/reference_version.rs` (the pin checks).

## Provisioning

`scripts/fabro-provision.sh` builds `fabro-cli` from the fetched corpus
(`scripts/corpus-fetch-fabro.sh`, at the pin in `crates/fabro/corpus-pin.txt`)
into the gitignored cache `crates/fabro/corpus/fabro-target/`, or reuses the
binary there. It prints the binary's path. It refuses a binary whose
`--version` string does not carry the pin's short SHA, and it never resolves
`fabro` from `PATH`. `FABRO_BIN` overrides the path and is checked the same
way. `--check` reports without building (exit 3 when nothing valid exists).
`scripts/oracle-regenerate.sh` provisions through the same script.

The harness (`tests/support/fabro/fabro_adapter.rs`, `FabroBinary::provisioned`)
looks in the cache (or `FABRO_BIN`). A missing binary makes a cell compare
Petri against the committed reference and say so in its evidence;
`PETRI_REQUIRE_FABRO_BINARY=1` makes it a failure instead, which CI's
compatibility job sets. A binary that exists but reports another commit
always fails.

## The Fabro adapter

Per cell the adapter starts one private Fabro server: its own `HOME` and
`FABRO_HOME`, storage directory, loopback TCP port, dev token and session
secret, in-memory store, and a `settings.toml` with dev-token auth, a
`local` environment and one `[llm.providers.<id>] base_url = "http://127.0.0.1:<port>/v1"`
per twin the cell uses (Fabro's adapters append `/responses` and
`/messages`). It logs the CLI in, stores the case namespace as each
provider's secret (`fabro secret set`), and hands the same namespace to the
server environment, so every request that reaches a twin is attributable
to that engine. The server runs in its own process group; the adapter
signals only that group, only while the server is alive, and never matches
processes by name.

A run is `fabro validate --json` (a rejection is recorded as
`status = rejected` with the diagnostics, not patched around), then
`fabro run --detach --json --environment local --provider <first twin> -I key=value ... <file>`
in the staged bundle, which is a git repository with one commit because
Fabro's `local` environment works in place. The adapter polls
`GET /api/v1/runs/{id}/state` and `GET /api/v1/runs/{id}/questions`, and
answers each pending question through
`POST /api/v1/runs/{id}/questions/{qid}/answer` from the same `cli::answer`
script the Petri side ran with: `choice` becomes `{"kind":"yes"|"no"}` on a
`yes_no` or `confirmation` gate and `{"kind":"selected","option_key"}`
otherwise, `choices` becomes `multi_selected`, `text` becomes `text`,
`negative` is `no`, `cancel` cancels the run, `withhold` leaves the
question. It never bypasses the human gate. It emits a receipt in Petri's
`interviews.json` shape (questions with node, kind, text, options, reply,
delivery; errors; script entries with consumed counts), so unmatched,
ambiguous, rejected and unused entries are errors on both sides alike.

After the run: `fabro events --json`, the state document, and `fabro dump`
(whose `stages/<n>-<node>@<visit>/parallel_results.json` carries the inline
values the event log offloads to `blob://` references). The deadline
cancels a run through `POST /api/v1/runs/{id}/cancel`.

## What is compared

`tests/support/fabro/compare.rs` projects both engines onto one shape:

| Field | Petri source | Fabro source |
|---|---|---|
| `status` | `petri inspect --json` `status` | `run.completed` / `run.failed` (or the validator's rejection, the deadline's `timed_out`) |
| `path` | the root execution's `history` records in order, node and outcome | `stage.completed` / final `stage.failed` events outside any branch |
| `forks[].branches[]` | child invocations by slot `branch:<fork>@<firing>:<index>:<node>`, envelopes from the join's `parallel.results`, stages from the child's history | `parallel.branch.*` events by `parallel_branch_id`, envelopes from the dump (else `parallel.completed`) |
| `context` and `bookkeeping` | the root invocation's final `context` | the last `context_values` plus updates (or the last checkpoint) |
| `artifacts` | files the scenario names, read from the workspace | the same, from the run directory |
| `interviews` | the interview receipt | the adapter's receipt |
| `requests` and `platform_requests` | the twins' request logs for the engine's namespace | the same, for Fabro's namespace |
| `counts` | provider, platform and interview counts | the same |

Vocabulary: run status `succeeded`, `failed`, `cancelled`; stage outcome
`succeeded`, `partially_succeeded`, `failed`, `skipped`, `cancelled`,
`timed_out`.

Rules, as the plan states them:

- The main path must match exactly and in order after two named removals
  on the Petri side, each reported as its own difference: a `skipped`
  stage (`path.skipped_stage`) and a parallel branch's delegate stage
  (`path.branch_stage`). Causal order inside a branch (`branches[].stages`)
  and the ordered envelopes at a join (`id`, `index`, `item_label`,
  `status`, `context_updates`) must match with exact counts and identities.
  Parallel completion order is free.
- The workflow-owned context is compared key by key, value by value, with
  missing versus `null` preserved. Engine bookkeeping keys are removed only
  when the scenario names them one by one (a `prefix.` names a prefix), and
  the removed keys stay in the projection under `bookkeeping`.
- Artifacts compare byte for byte. Interviews compare node, kind, text,
  options, reply and delivery. Requests compare provider, model, reasoning
  effort and the matched twin scenario, in order; a platform request (the
  pinned Fabro's run-title call, named by `compare::platform_request`) is
  listed apart and counted.
- Normalization replaces only named identities: the run id, the run's
  working directory, and `blob://sha256/<hex>` references, through an
  explicit map the projection carries. Nothing sorts arrays, coerces
  types, drops nulls, drops context updates, or collapses a failed stage.
- One exact string shape has its own kind: Petri's value equal to Fabro's
  plus one final newline is `value.trailing_newline`. That departure was
  retired (command output is byte-exact since the sandbox-driver re-pin);
  the kind stays named so a recurrence is reported as itself, and no record
  accepts it.

Order of checks in a cell: Petri's independent expectation, Fabro's
independent expectation, the committed reference, then the comparison. A
Fabro violation of the independent expectation is a baseline defect: it is
recorded in the evidence and fails the cell unless a decision record for
the scenario lists the assertion's exact name under `known_defects`
(`decisions/README.md`). A listed defect is printed on the test's stderr and
recorded under `baseline_defects.known` in the Fabro engine record, with
the record's id; it never relaxes Petri's expectation. The coverage report
(`scripts/fabro-coverage-report.py`) reads the same decision records: a
Fabro engine record's failed assertion that a record lists is expected, and
the cell passes with a note naming the record. A failed assertion no record
lists fails the cell, and a Petri engine record never gets the allowance.

## Decisions

Every intentional difference is one record in
`crates/fabro/acceptance/decisions/<id>.toml` (format in
`decisions/README.md`). A record names its scope (`scenarios`, `bundles`,
globs allowed), the observed Fabro and Petri behaviour, the user-visible
effect, the reason, the acceptance criteria, and, for a migration, the old
and new bundle digests. Its `accepts` list names the exact difference kinds
(and optionally a field pattern) the comparison may accept under it. A
difference no record accepts fails the cell. `reference_version.rs` checks
that every record is complete and accepts only kinds the comparison emits.

## Evidence

Every cell is a required cell of `scenarios/matrix.json`
(`<scenario>@host/<agent>`, `test` naming this suite's test), so the
coverage report takes it from the engine records below and never lists it
as unlisted. Every cell writes, under `PETRI_EVIDENCE_DIR` (default
`target/fabro-differential/<scenario>/`), one record per engine
(`petri.json`, `fabro.json`) with the pins of everything that produced it
(Petri commit and dirty flag, Pebble, `lithos-llm`, sandbox-driver and twin
revisions, the Fabro pin, version and binary, the bundle digest, the
sandbox plugins, tool versions), the launch, the process result, the
normalized projection, pointers to the raw observations (the Fabro events,
state and dump are copied beside the record), every independent assertion
with its outcome, the baseline defects, and the cleanup; plus
`comparison.json` with every difference, the decision that accepted each,
and the unresolved count.

## Baselines and refresh

`crates/fabro/acceptance/scenarios/<name>/fabro-reference/reference.json` is
the committed projection of the pinned Fabro for that scenario, with the
Fabro revision and version, the bundle digest (over the files the cell
declares as the bundle, so scenario metadata beside them never moves it),
the twins used and the normalization applied.
A live Fabro run must reproduce it exactly (the identity map aside); a
difference fails the cell with a pointer-by-pointer diff. Only
`PETRI_FABRO_REFERENCE_RECORD=1` rewrites the file, so a refresh after a
source, bundle or twin change is a reviewable `git diff`, never a silent
replacement. Without the binary, a cell compares Petri against this file.

To refresh after a change: `scripts/fabro-provision.sh`, then
`PETRI_FABRO_REFERENCE_RECORD=1 mise run test:fabro:differential`, review
the diff of every `reference.json`, and commit it with the change that
caused it.

## Adding a cell

Declare the scenario in `fabro_differential.rs` (until task 17's schema
lands): the bundle directory under `scenarios/`, the bundle's files (what
both engines run; the digest covers exactly these), the workflow file, the
inputs (`{bundle}` is the staged path), the comparison rules (bookkeeping
keys, artifacts), the shared interview script, the twins with their
scripts per namespace, skill directories to seed under each engine's
`$FABRO_HOME/skills`, the independent expectation, and an optional
engine-specific request probe over the raw request bodies. A known
baseline defect of the pinned Fabro goes into a decision record's
`known_defects` (the assertion's exact name, the scenario named exactly),
not into the cell. Add the cell to `scenarios/matrix.json` as
`<scenario>@host/<agent>` with `test` set to
`petri-cli::fabro_differential::<test>`, so the coverage report requires it.

Fixture state must reach both engines the same way: Fabro's `local`
environment runs inside the staged bundle, and Petri's workspace starts
empty, so a scenario that needs files in the workspace copies them there
itself (`skills-precedence` does it in a `setup` node from the bundle path
it receives as an input). `parallel-results` needs no files: its helper runs
from the bundle path input and writes its state under `runtime/`. Record the reference once with
the binary, review it, and commit it.
