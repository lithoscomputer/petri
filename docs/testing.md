# Testing and compatibility

See [DEVELOPING.md](https://github.com/lithoscomputer/petri/blob/main/DEVELOPING.md) for setup and routine test commands.

## Reading the tests

The test suites are the executable form of the design document.

```
crates/core/engine/tests/routing.rs          §2 selection, fan-out, OR-split, guards, preconditions
crates/core/engine/tests/joins.rs            §3 All / Any / Quorum, generations, entry seeding
crates/core/engine/tests/loops.rs            §6a sequential for_each, back edges, firing budgets
crates/core/engine/tests/expansion.rs        §6 parallel for_each, collectors, max_parallel, fail_fast
crates/core/engine/tests/cancellation.rs     §5a cancel scopes, both tiers: routing, run_on_cancel, kill
crates/core/engine/tests/scopes.rs           §3 resource scopes, env, acquire/release
crates/core/engine/tests/gha.rs              the GHA mapping table, end to end
crates/core/engine/tests/retries.rs          handoff §1 attempts, backoff, exhaustion
crates/core/engine/tests/run_context.rs      handoff §2 nodes.* / kv.*, goal gates, merge order
crates/core/engine/tests/partial_success.rs  handoff §4 soft failure, is_success_like
crates/core/engine/tests/seeding.rs          seed edges for entry nodes and clone entries
crates/core/engine/tests/resolved_firing.rs  the executor boundary: no unresolved ExprId crosses it
crates/core/engine/tests/event_log.rs        §5 logging, determinism, serde round-trip, §8 seams
crates/core/engine/tests/flow_properties.rs  §3–§5 over random flows: joins, generations, budgets, retries, cancel and kill, replay
crates/core/engine/tests/resume_properties.rs  §6 resume over random flows: a log cut anywhere resumes to the live state, owes what was outstanding, and goes on to the same log
crates/core/engine/tests/lean_model.rs       the core against the Lean model in `lean/`: flows and `deterministic_pick` (`mise run test:lean`)
crates/core/ir/tests/validation.rs           §7, invariant by invariant
crates/core/ir/tests/expressions.rs          the expression language

crates/core/driver/tests/e2e.rs           exec §7 1-2: native loop and GHA-shaped, real processes
crates/core/driver/tests/cancellation.rs  exec §7 3,4,5,9: the ladder and the hard deadline
crates/core/driver/tests/kill.rs          §10 two-tier stop: cleanup after cancel, cleanup grace, KillRequested
crates/core/driver/tests/release.rs       §10 sentinel-pinned groups: release kills, reaps, then only observes
crates/core/driver/tests/timeout.rs       exec §7 6: timeouts, and the race under replay
crates/core/driver/tests/environments.rs  exec §7 7,10: acquire failure and retention
crates/core/driver/tests/secrets.rs       exec §7 8: masking, and what reaches the log
crates/core/driver/tests/docker.rs        exec §7 3,10 Docker halves; skipped without a daemon
crates/core/driver/tests/determinism.rs   §10 determinism: a seeded run on a simulated clock replays byte for byte
crates/core/driver/tests/simulation.rs    §10 simulation: seeded crashes, stops and faults against a simulated sandbox world
crates/core/executor-sandbox/tests/docker_backend.rs   the sandbox executor over the Docker plugin: the workspace in the sandbox, exit codes and signals, files over the wire, the crash fence, retention, services, one-shot actions
crates/core/executor-sandbox/tests/daytona_backend.rs  the same executor over the Daytona plugin, live (`mise run test:daytona`, `PETRI_REQUIRE_DAYTONA`): the runner VM, a nested container job, files, the fence, retention, output after idle and under a burst, a preview URL, the failure modes; `DAYTONA.md` maps Fabro's former live suite onto it
crates/petri/lib/tests/daytona.rs             the standalone host on `--backend daytona`, live: retention and `prune` with the tombstone, `resume` fencing the crashed VM

crates/core/frontend/tests/expr_grammar.rs   frontend §7 1-2: grammar, precedence, coercion, fuzz, table gate
crates/github/frontend/tests/lowering.rs     frontend §7 3-6, the pure half: what lowering produces
crates/core/frontend-native/tests/native.rs  frontend §7 7, the pure half; invariant 8 hint
crates/github/acceptance/tests/gha_e2e.rs    frontend §7 3-6, run for real: truth table, matrix, composites
crates/core/runtime/tests/native_e2e.rs      frontend §7 7: the cycle, run end to end
crates/core/runtime/tests/admission.rs       the admission seam: a pass rewrites the root and the children with the static capabilities, a problem is a diagnostic on the node's span, a changed child's digest is followed through the parent's reference
crates/github/acceptance/tests/harness.rs    frontend §7 8: every corpus workflow lowers or is rejected specifically
crates/github/acceptance/tests/e2e.rs        frontend §7 9: two real corpus workflows run on the executor

crates/attractor/frontend/tests/lowering.rs      tiers, failure policies, goal gates, parallel, budgets, rejections
crates/fabro/frontend/tests/lowering.rs        the settings layers: `workflow.toml` sections, `.fabro/project.toml`, the user settings, `[run.inputs]` defaults, hooks and MCP layering, the launch record, the bundle root
crates/attractor/frontend/tests/conditions.rs    the condition grammar against Fabro's semantics
crates/attractor/frontend/tests/fuzz.rs          arbitrary text never panics the parser
crates/fabro/acceptance/tests/harness.rs     every corpus file lowers or is rejected specifically; REPORT.md
crates/fabro/acceptance/tests/runs.rs        every lowered corpus file runs under stubs; RUNS.md
crates/fabro/acceptance/tests/routing.rs     the scripted battery, checked against the Fabro oracle
crates/fabro/acceptance/tests/exhaustion.rs  the failure policy after retries in Fabro's order (explicit routes first under `succeed`, `allow_partial` before any route, an expired human gate), expectations from the pinned source
crates/fabro/acceptance/tests/workflow.rs    nested workflows through the coordinator; one child per manager attempt
crates/attractor/steps/tests/manager.rs          the manager loop under a controlled clock (polls, stop condition, exhaustion, defaults, reattach, cancel)
crates/attractor/steps/tests/parallel.rs         branches as child invocations; static, mixed and all-failed forks, promotion, duplicate targets, empty `for_each`, item labels, repeated forks, nested forks, cancellation, resume; the fork snapshot offloaded (a 50-item fork's children, resume through the store, two forks in sequence)
crates/core/execution/tests/admission.rs      `max_parallel` as a bound on a fork's live children (dispatch in declaration order, a branch in backoff holds none, cancel while queued, redispatch on resume) and the run-wide 10,000 invocation ceiling (boundary, nested, finished children, lower limits, refusals, resume)
crates/attractor/steps/tests/prompt.rs           `attractor/prompt` against a scripted model client, contracts and repair turns, the prompted fan-in, prompt events
crates/attractor/steps/tests/admission.rs        model resolution at admission: concrete routes and frozen plans on every LLM node, `attractor.model.unknown` and `attractor.model.fallbacks` at the node's span, the graph unchanged without a client, the `start` stage's table check without a catalog
crates/fabro/acceptance/tests/e2e.rs         gh-list, hello, a for_each fan-out, random selection, two 1,000-item forks in one run, end to end
crates/petri/cli/tests/fabro_cli.rs          `petri run --auto-approve` answers a human gate
crates/petri/cli/tests/inspect_cli.rs        black box phase 2: `petri inspect` over finished, restarted, failed, cancelled and damaged run dirs, and over a fork of a run the binary wrote, which `petri resume` then continues
crates/petri/lib/tests/fork.rs               `host::fork_from` through the embedding boundary: a three-stage run forked after its first stage, a fork at the last position with and without `rerun_last`, a fork after a parallel fan-in and before it, a position inside a branch refused, a fork at a failed firing on its failure route, and a fork in a host's own store
crates/petri/cli/tests/fabro_resume_blackbox.rs  `petri resume` through the binary: a run killed with SIGKILL continues without repeating finished work, a paused run stays paused across the resume until an unpause, a waiting gate asks again, `inspect` reports `paused`, and the refusals (finished, leased, missing, corrupt)
crates/core/execution/tests/inspect.rs      black box phase 2: `inspect_run` reconstruction, retries, children, torn and corrupt logs
crates/core/execution/tests/determinism.rs  §10 determinism: a seeded coordinator run stores the same logs byte for byte, every record stamped from the simulated clock
crates/core/execution/tests/simulation.rs   §10 simulation: seeded invocation trees, forks, gates, restarts, cancels, the breaker and crashes, resumed over one store
crates/petri/cli/tests/fabro_blackbox.rs     the Fabro black box battery: the shipped binary against provider twins on loopback, scripted interviews, retention, a smoke run with no `fabro` on PATH; every read of a finished run goes through `petri inspect --json`
crates/petri/cli/tests/fabro_scenarios_blackbox.rs black box phase 4: every required (scenario, backend, agent) cell of `crates/fabro/acceptance/scenarios/matrix.json`, each a scenario file in the versioned format `scenarios/SCHEMA.md` documents, run through the shipped binary with provider twins, a fixture repository with real local Git remotes, and a scripted interviewer; `scripts/fabro-coverage-report.py` merges the per-cell records into `coverage.json`
crates/petri/cli/tests/fabro_differential.rs black box phase 5: every scenario through the shipped binary and the pinned Fabro binary, independent expectations per engine, the committed reference, and the comparison under `tests/support/fabro/compare.rs` with decision records (`crates/fabro/acceptance/decisions/`)
crates/fabro/acceptance/tests/reference_version.rs  every oracle fixture, scenario reference, decision record, staged bundle and evidence record names the pinned Fabro revision
crates/petri/lib/tests/interview.rs          the interview dispatcher on the standalone host: parallel gates, sensitive masking, a failing interviewer, cancellation, occurrence across a loop, nested invocation paths, the re-ask, expiry
crates/petri/lib/tests/controls.rs           the circuit breaker across restarts and resume, node visit totals across `loop_restart`, the stall watchdog, pause, cancel while paused, steering; the durable pause (a pause survives a dropped coordinator and holds admission on resume until an unpause; a paused resumed run can be cancelled)
crates/petri/lib/tests/interrupt.rs          the interrupt control: an interrupt with text during a native tool call ends the turn, keeps the session, and the text opens the next turn; a plain interrupt parks the turn until the next steer; a gate with no turn refuses it by name; the ACP backend's `session/cancel` without ending the agent
crates/core/driver/tests/interview_budget.rs on a controlled clock: own-stage and sibling waits, overlapping questions, active work after a wait, handler-managed nodes, cancellation during a wait, a fresh budget on redispatch
crates/petri/lib/tests/embedding.rs          a Fabro workflow without adapters, then with fake adapters (pause, skip, block, prepared results, route override, fatal and best-effort transitions, a hook service); the timeline reconstructed from public events; slow, failing and recovering consumers
crates/attractor/steps/tests/steps.rs            command, wait, human answered through deliver; Fabro's failure promotion; output references above 100 KiB; a long line stored whole, or its loss counted on the outcome
crates/attractor/steps/tests/agent.rs            the agent step against Fabro's fake ACP agent
crates/attractor/steps/tests/acp.rs              the ACP client against Petri's scripted agent (`tests/testdata/scripted_acp_agent.py`): every `session/update` variant as the `acp` envelope, the permission policy without a hook, the session usage extension in the stage's metrics, `authenticate` before `session/new`, only the named `$secret` references in the agent's environment, masked
crates/petri/lib/tests/acp_products.rs       , live (`--ignored`): Claude Code (`claude-code-acp`) and Gemini CLI (`gemini --acp`) through the ACP step, on the host and in a container the test builds, creating a file, the session's tool calls and usage on the stream, a `[[run.hooks]]` block honoured; each cell skips itself without the product's binary and credential
crates/fabro/frontend/tests/hooks.rs         `[[run.hooks]]` at every phase with Fabro's payload and order, decisions, placement, timeouts, HTTP, prompt and agent hooks, native tool hooks, threads and fidelity, project memory, `speed` and `max_tokens`, the ACP hook mapping (a block rejects the permission request, the post-tool hooks run on reported tool calls, allow once under a hook)
crates/fabro/frontend/tests/mcp.rs           `[run.agent.mcps]` against the scripted `mcp_server.py`: a stdio server's tool writes into the workspace, hooks block MCP tools, error results, timeouts, a crashed server, cancellation, a retained thread, start failures, the http and sandbox transports over streamable HTTP and SSE, every fact read from Pebble's own events
crates/petri/cli/tests/fabro_mcp_blackbox.rs  through the binary: a configured MCP server's tool called by the twin's model under its qualified name, the effect in the workspace and the result in the next request, a hook block, failures, Ctrl-C, a retained thread, a start failure on the terminal, a masked secret, refused settings
crates/petri/cli/tests/fabro_fallback_blackbox.rs  through the binary: `[run.model.fallbacks]` with the twins injecting failures: a successful primary, qualifying and non-qualifying failures, a third provider after two failures, chain exhaustion, a tool effect not repeated across a failover, cancellation during fallback, refusal versus other content filters, a request timeout, client retries spent before the chain advances, a workflow retry that is not a failover, reasoning effort mapping with `NoNearbyReasoningLevel` and `ChainEmpty`, repair turns on the plan with a target's chain inert, a retained thread on the fallback route, a prompt node
crates/petri/cli/tests/llm_client.rs         the client `petri::llm_client` builds, against a twin: its same-route retries and its call budget
crates/petri/lib/tests/fallback_events.rs    a stage's fallback plan, routes, failover decision, per-route usage and outcome rebuilt from public `RunEvent`s alone
crates/petri/lib/tests/model_admission.rs    admitted routes survive catalog changes before dispatch and before resume (alias, default provider, eligible providers), read from the plan event and Pebble's session route; a pinned route the client cannot address fails with `llm:pinned_route_unavailable`
crates/petri/cli/tests/fabro_hooks_blackbox.rs  through the binary: a configured hook blocks a real tool effect of a native agent (`a_configured_hook_blocks_a_real_tool_effect_in_the_native_backend`); two `full` nodes share one conversation
crates/attractor/steps/src/fallback.rs (unit)     chain resolution keyed by the canonical model, refused keys, skipped candidates with Fabro's notices, per-target reasoning effort with `NoNearbyReasoningLevel` and `ChainEmpty`, the plan's positions, the typed failure's eligibility (`lithos-llm`'s `failover_eligible`) for every `lithos-llm` kind, and every Pebble error's class
crates/fabro/frontend/tests/skills.rs        Fabro's skill directories in order on real scopes, precedence across the three, the reference prompt section and tool definition, `/name` expansion, `[run.agent] skills`, malformed and missing skills diagnosed (fixtures `crates/fabro/acceptance/testdata/skills`)
crates/petri/cli/tests/fabro_skills_blackbox.rs  through the binary: precedence and the reference prompt and tool on both vocabularies, a hook blocking a skill-driven tool call, a `for_each` branch keeping its loaded skill, the warnings on the terminal and in the event log
crates/attractor/steps/tests/compaction.rs         Fabro's compaction on the native backend against a scripted model: the trigger below, at and above the 80 percent threshold, continuation and thread reuse across the boundary, tool pairing, a host `CompactionPolicy`, a failed summary, cancellation, the lost-thread fallback, and the public events and usage
crates/petri/cli/tests/fabro_compaction_blackbox.rs  through the binary: a native agent compacts and finishes correct work, a later `full` node reuses the compacted thread, a failed summary call is non-fatal, a lost thread starts at `summary:high`, and a run cancelled during the summary call stops
crates/attractor/steps/tests/subagents.rs        a native agent delegates to Pebble-built children on real scopes; hooks and the question rule inside a child; the invocation ceiling never counts a child; the open-session bound; child failure, nesting, cancellation, thread reuse, resume; a child re-reads the parent's project memory and re-discovers its skills
crates/petri/cli/tests/fabro_subagents_blackbox.rs  through the binary: a parent delegates a workspace change; a hook blocks a child's effect; a child's failure is the parent's tool result; concurrent children and one invocation; a grandchild; an interrupt closes the child; a retained thread carries a child's result; accounting reconstructed from `execution::replay_run`
crates/petri/cli/tests/fabro_terminal_blackbox.rs  through the binary: retry notices, branch attribution with masked secrets, the bounded echo, `--interactive` for every question type with invalid and missing input, Docker retention after success, failure and cancellation with `petri sandbox prune`
crates/petri/cli/tests/fabro_milestone_blackbox.rs  through the binary: one workflow with explicit setup commands, a native agent editing a file under a tool hook, a retained thread, project memory, a scripted decision, a bounded fan-out consumed downstream, run-end hooks and file checks; success, failure and cancellation
crates/petri/cli/tests/fabro_readiness_blackbox.rs  through the binary: the combined workflow with every agent facility in one run (a skill-guided plan, a hooked MCP write, a hooked sub-agent, a fan-out, a compaction and a later node on the compacted thread with an MCP call, a second thread failing over to the Anthropic twin), no platform Git operation in the repository the run prepared, every family on the public stream; an exhausted chain and an interrupt inside a child's tool as separate cases
crates/petri/lib/tests/embedding_readiness.rs  through the embedding boundary: the same combined execution run in-process by a host with its own hooks, interviewer and sink against the twins; the same files and scripts, the run rebuilt from public events, replay equal to the live stream
crates/petri/cli/tests/fabro_evidence_blackbox.rs  the readiness gate's evidence: a scenario through the binary leaves a complete record with every pin, a failed scenario keeps its case directory, the coverage report counts only passed cells, a required asset fails instead of skipping
crates/petri/lib/tests/fabro_dependencies.rs  no Fabro crate anywhere in Petri's dependency graph
crates/petri/cli/tests/standalone.rs          the binary runs a Fabro workflow with no `fabro` on PATH
```

## The Fabro compatibility contract

`crates/fabro/acceptance/CONTRACT.md` freezes the Fabro reference revision
(`crates/fabro/corpus-pin.txt`), the required workflow bundles
(`crates/fabro/acceptance/bundles.lock.json`; their files are vendored under
`crates/fabro/acceptance/bundles/` with their provenance and verified against
the lock by `scripts/corpus-fetch-fabro-bundles.sh`), the feature matrix for every
construct and `workflow.toml` option those bundles use, and the accepted
differences. Petri production code never links or launches Fabro. The parity
harness in `crates/fabro/oracle/harness/` builds the pinned `fabro` binary from
the fetched corpus and drives it as a subprocess through its CLI and server
API; `scripts/oracle-regenerate.sh` uses it to refresh the oracle fixtures, and
the routing test rejects a fixture from any other Fabro revision.
`scripts/fabro-provision.sh` is the one way both harnesses obtain that binary.
The differential matrix (`crates/fabro/acceptance/DIFFERENTIAL.md`,
`mise run test:fabro:differential`) runs every scenario through the shipped
binary and the pinned Fabro, compares the runs under fixed rules, and accepts
a difference only through a committed decision record.

The complete-workflow scenarios are data, not code: one versioned JSON file
per scenario under `crates/fabro/acceptance/scenarios/`
(`SCHEMA.md` there is the format, `README.md` the layout), naming the pinned
bundle and its digest, the fixture repository and its local remotes, the twin
scripts, the interview entries, the controls and faults, and every expected
observation, written before the run. `crates/fabro/acceptance/scenarios/matrix.json`
lists every required (scenario, backend, agent) cell with its state, so a
filtered or skipped case stays visible; `mise run test:fabro:blackbox` runs
them and reports the coverage, and `mise run test:fabro:blackbox:strict`
requires Docker, the vendored bundles, and no skipped cell.
`mise run check:fabro:readiness` is the final readiness gate: the same
strict run, then a coverage report that fails unless every required cell
passed, blocked cells included; the nightly workflow runs it on the runners
that have Docker.

## CI, evidence, and the readiness gate

Required CI (`.github/workflows/ci.yml`, job `check` on Linux and macOS)
fetches the GitHub Actions corpus and the Fabro corpus, verifies the vendored
Fabro bundle set (`scripts/corpus-fetch-fabro-bundles.sh`: every tracked file
against its locked digest, never skipped, no fetch), builds the sandbox
plugins, and runs
`mise run check` with `PETRI_REQUIRE_CORPUS`, `PETRI_REQUIRE_FABRO_CORPUS`,
`PETRI_REQUIRE_FABRO_BUNDLES` and, on Linux, `PETRI_REQUIRE_DOCKER` set, so a
missing asset, binary, scenario, or backend fails instead of skipping. Every
required host scenario runs on both runners; the Docker subset runs on Linux,
which also checks that no `petri-` container is left behind. The provider
twins are `petri-cli` dev-dependencies from the public
`lithoscomputer/twins` repository, so a build is their fetch. A second required
job, `fabro compatibility`, builds the pinned `fabro` binary from the fetched
corpus (`scripts/fabro-provision.sh`; only the binary is cached, keyed by the pin
and the toolchain; about three minutes on a miss) and runs the comparison
matrix through `mise run test:fabro:differential` with
`PETRI_REQUIRE_FABRO_BINARY` set. A third required job, `lean model`,
installs elan (pinned by version and digest), builds the Lean model in
`lean/`, which checks every proof, and runs `mise run test:lean`: the engine's
`lean_model` tests compare the real core with the model's executable, with
`PETRI_REQUIRE_LEAN_MODEL` set. The Nightly workflow repeats the black box
set three times under different test schedules
(`mise run test:fabro:blackbox:repeat`), runs the long tests, and reruns the
matrix; it adds repetitions and is never the only parity evidence.

Every black box scenario writes a machine-readable evidence record
(`crates/petri/cli/tests/support/fabro/record.rs`, schema version 1) into
the run-scoped directory `PETRI_EVIDENCE_DIR` names (`mise run
test:fabro:blackbox` creates one under `target/fabro-evidence/` and links
`latest` to it): the Petri, Pebble, lithos-llm, sandbox-driver, twins, and
Fabro pins read from the manifests; the launch inputs and configuration; the
twin scenarios matched; raw and normalized observations; the final context
through `petri inspect --json`; artifacts; process output; assertions;
compatibility decisions; the cleanup result; and links to the owning
library's contract tests. A failed record keeps the whole case directory; a
test that panics before finishing still leaves a failed record.
`scripts/fabro-coverage-report.py` folds the records, the scenario matrix (`matrix.json`), the per-cell results,
the differential matrix's engine records, and Nextest's JUnit output into
`coverage.json` and `coverage.md` with required, passed, failed, skipped,
missing, blocked, and excluded cells. Only `passed` counts; a skip, an
exclusion, or an empty run never does, and a runner failure overrides a passed
record. A pinned-Fabro assertion that a decision record lists under
`known_defects` is expected and counts as passed with a note naming the
record; any other failed assertion fails the cell. CI writes the report into the job summary
and keeps the full bundles of a failed run and the compact records of a passed
run as job artifacts. The readiness checklist with each item's evidence source is the
"Readiness gate checklist" section of that contract.

### Library and repository gates

Pebble owns the agent loop, coding-agent behavior, and the MCP client;
`lithos-llm` owns provider transport and request retries. A change to either runs the owning repository's required checks
before Petri moves its lock; then the relevant Petri scenarios run again
through the shipped binary, and their evidence records cite the new commits.
A library test pass never replaces a required Petri scenario. The locked
commits live only in `Cargo.lock`; the contract's "Pinned revisions" table
names where each revision lives, the Fabro reference and the runner image
included.
See [git dependencies](https://github.com/lithoscomputer/petri/blob/main/DEVELOPING.md#git-dependencies) for how they are
named and updated.
The library batch the readiness work asked for landed on
`petri/readiness-batch` in each repository and is pinned: Pebble's
summary-call usage accounting, `continue_prompt` for a failover with no
repeated tool effect, child agents inheriting project memory and skill
directories, skipped-skill reporting, sequential sub-agent tools, a routed
scripted provider; sandbox-driver's silence-based output drain and the
preview-URL operation for servers inside containers. The later
`petri-readiness-gaps` revision exports Pebble's project-memory loader, which
prompt nodes now load through, and the tool-round budget agent hooks enforce.
Pebble `c91810f` and lithos-llm `55add45` carry token usage and cost as one
lithos-llm `Usage`; every `usage` Petri records takes that shape
(`crates/attractor/FORMAT.md`, "Usage").


`mise run test:remote` transfers an artifact through a separate Docker daemon
with no host filesystem mounts. `mise run check` includes this test. Set
`PETRI_REQUIRE_DOCKER=1` to require Docker instead of skipping unavailable tests.

`mise run test:daytona` runs the live Daytona tier against the account
`DAYTONA_API_KEY` names: the sandbox executor's Daytona battery
(`crates/core/executor-sandbox/tests/daytona_backend.rs`), the host's
retention, prune and resume cells (`crates/petri/lib/tests/daytona.rs`), the
CLI retention cell, and the GitHub Actions cell with a VM job and a nested
container job. It builds the Daytona plugin (`mise run plugins:build:daytona`)
and sets `PETRI_REQUIRE_DAYTONA=1`, so a missing credential or plugin fails
with its reason instead of skipping. Without the variable the same tests skip
with a `skipping:` notice, which is what the routine suite does. No CI job has
the credential, so the tier is run by hand before a change to the Daytona
path or the sandbox-driver pin lands. It creates billable sandboxes.
[`crates/core/executor-sandbox/DAYTONA.md`](https://github.com/lithoscomputer/petri/blob/main/crates/core/executor-sandbox/DAYTONA.md)
maps Fabro's former 22 live Daytona tests onto the tier and records the gaps.

`mise run test` runs all of them with Nextest, then runs the maintained doctests
with Cargo.

## The corpus

316 workflows from 21 repositories with `.github/workflows` (22 fetched). Two
instruments measure them, both regenerated by the harness and committed beside
the corpus pins:

- **`crates/github/corpus/REPORT.md`** — the *lowering* bar: every workflow
  either lowers or is rejected with a specific `unsupported.*` code — zero
  panics, zero generic errors. Windows/macOS workflows, callee-only reusable
  files, and broken-upstream references leave the denominator by policy.
- **`crates/github/corpus/RUNS.md`** — the *run-time* bar: every in-scope
  workflow that lowers, run end to end (`--ignored`, network + Docker) with
  `run:` scripts stubbed to `true` and `uses:` steps real, every host scope
  rewritten to a pinned [sandbox-images](https://github.com/lithoscomputer/sandbox-images)
  runner container so corpus code never executes on the host. First-failure
  classes are ranked, with server-coupled failures (OIDC, repository secrets,
  SaaS backends, credentials) kept apart from real runtime-tier gaps — the
  same denominator discipline, applied at run time.

The corpus is fetched, not committed: the workflows are their authors' property, so
`scripts/corpus-fetch.sh` downloads them at the commits pinned in
`crates/github/corpus-pins.txt`, and `crates/github/corpus/` is gitignored apart from
the two reports. Each fetched repo gets a `PROVENANCE.md` with its commit and licence.
The corpus tests skip when the corpus is absent, and `PETRI_REQUIRE_CORPUS=1` — which
CI sets — turns that skip into a failure.

Corpus workflows also run end to end in the acceptance battery
(`crates/github/acceptance/tests/e2e.rs`), with a stub `gh` on `PATH` so nothing
reaches GitHub's API; the deterministic runtime-tier batteries (artifacts across
jobs, the cache round-trip, local checkout, the tool cache) live beside it.

## Testing notes

These checks catch failures that a final exit status can hide:

- **Assert the duration, not just the result.** A `docker exec` that returns early
  looks exactly like a step that finished fast. Pinning elapsed time alongside the
  exit status in `docker_wait_follows_the_step_not_the_client` is what makes that
  regression loud instead of silent.
- **A cancellation test that only checks the process is gone will pass on teardown
  alone.** Releasing the scope kills everything either way, so the test has to pin
  *how* the step ended — which signal, which escalation — not merely that it did.
- **Docker tests skip without a plugin and a daemon, and `PETRI_REQUIRE_DOCKER`
  turns that skip into a failure.** CI sets it on the Linux job. A silently skipped
  acceptance battery is indistinguishable from a passing one, and that job exists
  precisely to say the battery ran. The live Daytona tier follows the same
  convention with `PETRI_REQUIRE_DAYTONA`, which `mise run test:daytona` sets,
  and so does the Lean model check with `PETRI_REQUIRE_LEAN_MODEL`, which
  `mise run test:lean` sets.
- **A container test reads the workspace through the sandbox, never through a
  host path.** The workspace lives in the sandbox's own volume. A test checks a
  file the step wrote with `ExecEnv::read_file`, or with `docker exec` through
  `testkit::container_read` when it has no environment handle; a heartbeat that
  must be seen to stop is measured by a step inside the sandbox and reported in
  its output.
