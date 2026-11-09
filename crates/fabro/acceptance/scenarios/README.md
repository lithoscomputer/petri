# Black box scenarios

The complete-workflow scenarios of the Fabro black box battery (black box
plan phases 3 to 5). Each scenario runs an inline graph from its own
directory through the shipped `petri` binary with provider
twins on loopback, real shell, file and Git tools, and a scripted
interviewer, and asserts the final context, files, side effects, provider
requests, interviews, and lifecycle it declared before the run.

## Layout

```
scenarios/
  SCHEMA.md                     the scenario format, version 1
  README.md                     this file
  matrix.json                   every required (scenario, backend, agent) cell
  <family>/<name>.scenario.json one scenario
  <family>/fixtures/…           files a scenario's fixture or services reference
  <family>/expected/…           expected file contents a scenario references
  parallel-results/             task 3's regression (its own CONTRACT.md and Fabro capture)
```

Families: `implement`, `interview`,
`provider-faults`, `routing`, `backend` (the backend-matrix cases), and
`acp` (the ACP agent backend). The `acp` family runs inline graphs whose
`acp.command` starts `acp/fixtures/scripted_acp_agent.py`, a standard-library
Python agent the fixture commits into the repository together with a JSON
script that says what each prompt does (text chunks, a routing directive, a
file written, an exit before answering, a permission request, a wait for
`session/cancel`); the agent reads nothing from the environment, so the same
fixture runs on the host and in a container. The family's minimum scenarios
and critical outcomes are the phase 4 table of
`.ai/plans/fabro-black-box-e2e.md`; `../CONTRACT.md` records which
obligation each scenario satisfies and which remain blocked.

## Coverage states

`matrix.json` declares each cell `planned`, `blocked` or `excluded`, and the
report resolves it to one state: `passed`, `external` (a cell verified by a
test in another suite, named in the report), `failed`, `skipped` (a gated
resource was missing), `missing` (a planned cell no test reported, which
counts as failed), `blocked` (a required cell that cannot run yet, with its
reason) or `excluded` (not required, with its reason).

## Running

Every scenario file has a host cell and a Docker cell; the Docker cell
reuses the file with `modes.backend` overridden by the matrix entry, and a
file-triggered interrupt is read inside the container. The Docker cells run
under the bounded Nextest group `fabro-docker` (`.config/nextest.toml`), so
the full gate cannot starve them past their deadlines.

`mise run test:fabro:blackbox` runs every planned cell of `matrix.json` and
then the coverage report, allowing a skipped cell (a machine with no Docker).
`mise run test:fabro:blackbox:strict` requires Docker
(`PETRI_REQUIRE_DOCKER=1`) and fails on any skipped cell; the nightly
workflow runs it on the runners that have Docker (through
`check:fabro:readiness`), so the repeated container work stays out of the
routine subset. `mise run check:fabro:readiness` is the final readiness gate: the
same strict run, then the coverage report with `--readiness`, which fails
unless every required cell passed; a cell the matrix declares `blocked`
fails it, where the routine `--strict` report only shows it.

Each cell is one Nextest test in `crates/petri/cli/tests/fabro_scenarios_blackbox.rs`.
The test stages the fixture repository under a fresh directory, starts the
twins, runs `petri run`, reads the run back through `petri inspect --json`,
and applies the scenario's `expect` block through
`crates/petri/cli/tests/support/fabro/scenario.rs`.

## Coverage report

Every test writes its cell's result to `$PETRI_FABRO_COVERAGE_DIR`
(default `target/fabro-coverage/results/`) when it starts, as a failure, and
rewrites it as a pass only when every expectation held, so a panic or a
killed process leaves a failure on record. `scripts/fabro-coverage-report.py`
merges those results with `matrix.json` into `coverage.json` and prints the
table; its exit code is nonzero when a planned cell is not accounted for. CI
publishes that file. The report also reads the differential matrix's
per-engine records (`<evidence>/<scenario>/petri.json` and `fabro.json`) for
the `fabro_differential` cells, and the decision records under
`../decisions/`: a pinned-Fabro assertion a record lists under
`known_defects` counts as passed with a note naming the record; any other
failed assertion, on either engine, fails the cell.

## Writing a scenario

1. Pick the bundle and copy its `bundle_hash` from the lock file.
2. Write the fixture: the smallest repository the helpers accept, committed
   in order, with local remotes when the workflow pushes.
3. Script every model turn the run will make, matched on text that only
   that request carries (a job id, a candidate id, a tool result marker).
4. Write `expect` completely, before running: every row, including the
   empty ones.
5. Add the cell to `matrix.json` and the test to
   `fabro_scenarios_blackbox.rs`.

A scenario never patches the bundle's graph, prompts, or helper scripts. It
adapts only the declared environment (`fixture.env`, `fixture.bin`) and the
fixture data.
