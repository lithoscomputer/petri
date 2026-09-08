# The Fabro execution compatibility contract

This page freezes what Petri promises for Fabro workflows, against one Fabro
revision, for one declared set of complete workflow bundles. It is the
contract the black box battery and the differential comparison test against.
`crates/fabro/FORMAT.md` says how each construct lowers; this page says which
constructs the required bundles use, and whether each one is supported, a
tracked defect, an accepted migration, or an explicit exclusion.

## Reference revision

| What | Value |
|---|---|
| Fabro repository | `fabro-sh/fabro` |
| Reference commit | `b6482910e517d00dfc3c4a2f2d3e417c9348f7f6` (committed 2026-09-05T18:05Z) |
| Where it lives | `refs/pull/844/head`; not on `main` at freeze time |
| Pin file | `crates/fabro/corpus-pin.txt` (the `# ref:` line names the pull ref) |
| Fabro version string | `fabro 0.347.0-nightly.0 (b648291 ...)` |
| Fixtures | `crates/fabro/oracle/expected/*.json`, each with `fabro_commit` equal to the pin |

Rules:

- `scripts/corpus-fetch-fabro.sh` fetches the pinned commit. When the remote
  refuses a bare SHA it fetches the `# ref:` and fails unless that ref still
  resolves to the pin.
- `crates/fabro/acceptance/tests/routing.rs` rejects a fixture whose
  `fabro_commit` differs from the pin. Fixtures and parity runs always use one
  reference version.
- `scripts/oracle-regenerate.sh` builds `fabro` from the fetched checkout into
  `crates/fabro/corpus/fabro-target/` and drives it as a subprocess. The
  harness checks the binary reports the pinned short SHA. A `fabro` on `PATH`
  is never used.
- Petri never links a Fabro crate. `crates/petri/lib/tests/fabro_dependencies.rs`
  scans the full `cargo metadata` resolve graph, every dependency kind, and
  nested manifests and build scripts. `crates/petri/cli/tests/standalone.rs`
  runs the shipped binary with no `fabro` on `PATH`.

## The parity harness

`crates/fabro/oracle/harness/` runs the shared oracle cases through the pinned
Fabro binary and public interfaces only:

| Need | Public interface used |
|---|---|
| A Fabro server | `fabro server start --foreground --no-web --bind 127.0.0.1:<port> --storage-dir <dir> --config <settings.toml>`, dev-token auth, `[environments.local] provider = "local"` |
| Run creation | `fabro run --detach --environment local --provider openai --json <file>` in a fresh git repository; a fake `OPENAI_API_KEY` secret satisfies default model selection and is never called |
| Scripted agent and prompt stages | `backend="acp"`, `acp.command` launching `scripted_acp_agent.py`; the agent's text ends with Fabro's routing directive JSON (`outcome`, `failure_reason`, `preferred_next_label`, `suggested_next_ids`, `context_updates`) |
| Scripted human gates | `GET /api/v1/runs/{id}/questions`, `POST /api/v1/runs/{id}/questions/{qid}/answer` with `{"kind":"selected","option_key":...}` |
| A stage that asks for a retry | a human gate with `timeout="2s"` and no `human.default_choice`; Fabro's handler returns `retry_classify`, the only public path to `retry_requested` |
| Observation | `fabro events <run>` (JSON lines: `stage.completed`, `stage.failed` with `will_retry`, `loop.restart`, `run.completed`, `checkpoint.completed` with `context_values`) |

Cases that a public interface cannot reproduce exactly are recorded in the
fixture's `harness.realization` map so a reviewer can see which node ran as an
ACP agent and which as a human gate. The old in-process generator that linked
`fabro-workflow` is deleted.

## The differential matrix

Black box phase 5 (`crates/fabro/acceptance/DIFFERENTIAL.md`). Every
scenario under `crates/fabro/acceptance/scenarios/` runs through the shipped
`petri` binary and through the pinned Fabro binary
(`scripts/fabro-provision.sh`, never `PATH`'s `fabro`) with the same bundle,
inputs, provider twins and interview script, each engine in its own
workspace, server and twin namespace. The Fabro adapter
(`crates/petri/cli/tests/support/fabro/fabro_adapter.rs`) uses the same
public interfaces as the parity harness plus `fabro validate --json`,
`fabro dump` and `GET /api/v1/runs/{id}/state`, redirects Fabro's providers
through `[llm.providers.<id>] base_url` in its private `settings.toml`, and
answers gates through the questions API with `yes`, `no`, `selected`,
`multi_selected` and `text`.

Comparison rules (`support/fabro/compare.rs`): terminal status; the main
stage path in order; each fork's branch envelopes in dispatch order and the
causal order inside each branch, with exact counts; the workflow-owned
context key by key with named bookkeeping kept beside it; artifacts byte
for byte; interviews; provider requests (provider, model, effort, matched
twin scenario) with Fabro's platform requests listed apart; side-effect
counts. Only generated ids, the working directory and blob references are
normalized, through an explicit identity map. Each scenario asserts its
independent expectation on both engines first; a pinned-Fabro violation is
a recorded baseline defect, never permission for Petri to match it.

A difference is accepted only by a decision record in
`crates/fabro/acceptance/decisions/` (format in its `README.md`) that names
the difference kind and the scenario scope. The committed reference of a
scenario (`fabro-reference/reference.json`) is the baseline the live Fabro
run must reproduce; `PETRI_FABRO_REFERENCE_RECORD=1` is the only way to
refresh it. `crates/fabro/acceptance/tests/reference_version.rs` checks
every fixture, reference, capture, decision and evidence record against the
pin. `mise run test:fabro:differential` runs the matrix.

Cells captured live from the pinned binary: `parallel-results` (commands,
two fan-outs), `interview` (the required bundle, five gate kinds and the
summary prompt node), `edit-and-verify` (a native agent with a real shell
tool and a gate), `fallback-failover` (task 12's capture: a 503 after a
completed tool effect and the fall back to Anthropic), `skills-precedence`
(task 14's capture: the three skill directories, the reference prompt
section and `use_skill` tool, the repository's copy winning). Baseline defects of
the pinned Fabro found by the matrix: it repeats a completed tool effect
on failover (`fallback-repeated-tool-effect`).

## Required bundles

`crates/fabro/acceptance/bundles.lock.json` (schema version 1) is the
replacement set. `scripts/corpus-fetch-fabro-bundles.sh` materializes it on a
clean machine and verifies every digest.

| Bundle | Source and revision | Status | Scenario obligations |
|---|---|---|---|
| `code-review` | `lithoscomputer/code-review` `0c81ffb4f68039ca56842ab3328fde03b9714350` (private; HEAD, clean) | required | empty diff; findings in multiple branches; no surviving findings; invalid output repaired; repair exhausted; one failed branch; reverse branch completion; multi-level fan-out |
| `security-review` | `lithoscomputer/security-review` `c14279e9cdac4f7553b5e010718ada1a76e5c1f8` (public; HEAD, clean) | required | no vulnerabilities; several verified; rejected candidate; partial branch failure; malformed response; timeout and cancel |
| `fix-ci` | `veniceai/factory` `1b50f791ac4811aad62788c2a4282a75d1e92422` (private; bundle files match HEAD, other files in that working tree were dirty) | excluded (owner decision 2026-09-07) | none; see below |
| `implement-issue` (with `implement-plan`) | `fabro-sh/fabro` at the pin | required, blocked | child runs; input and model inheritance; multiple manager cycles; stop condition; child failure; parent cancellation |
| `interview` | `fabro-sh/fabro` at the pin | required | scripted choice, refusal, free text; repeated and concurrent questions; child interviews; delayed and withheld reply; invalid, unexpected and unused answers; timeout and cancel; terminal EOF |

"Blocked" names a required bundle whose scenarios cannot run yet. The
blockers are listed per bundle in the lock file. They are not exclusions.

`fix-ci` is excluded by an owner decision of 2026-09-07: `fix_ci.py` resolves
its target through `api.github.com` and clones `veniceai/interface`, and its
setup step needs yarn 4.12.0 through corepack with no network, so a
deterministic run needs a scripted stand-in for the GitHub API and an
offline-installable Node target. The owner judged that overkill. The repair
loop's behavior that does not depend on GitHub is covered elsewhere: a real
deterministic check driving a repair loop until it passes, with the visit
totals surviving the jump, is `routing/goal-gate-restart-and-visit-limit`;
a provider failure, an exhausted retry budget, a hanging request and a
cancellation are the `provider-faults` family; a failing stage's policy is
`routing/failure-policy`. What stays uncovered is the bundle's own GitHub
target resolution and its `publish` step's single push after success. The
bundle keeps its provenance in `bundles.lock.json` with
`status: "excluded"` and the reason, so the decision is visible and
reversible.

### Inventory and classification

The discovery scan covered `~/p/` with these exclusions: `node_modules`,
`target`, `.git`, `.cargo`, `vendor`, `dist`, `build`. It found 6,727
`.fabro` and `.dot` files with 332 distinct contents. Every file has a
location class and a content class (evidence file: task1 review). Summary:

| Class | Files | Disposition |
|---|---|---|
| Fabro repository checkouts, worktrees and backups (`~/p/fabro-sh/*`) | 5,842 | the pinned corpus covers them; other checkouts are duplicates of other revisions |
| Petri corpus mirrors and probe evidence | 251 | ignored: fetched data and review artefacts |
| Foreman and Conveyor scratch (run dumps, smoke graphs, release repairs) | 241 | generated or stale; excluded |
| Conveyor and its worktrees (`.fabro/workflows/*`) | 137 | valid Fabro variants of the canonical review bundles and Conveyor-only workflows; not in the first set |
| Attractor-era and third-party Attractor implementations | 439 | legacy Attractor; expected `unsupported.attractor` rejections or unrelated |
| Ordinary Graphviz | 6 | excluded |
| `veniceai/factory` workflows other than fix-ci | 9 | valid Fabro; later candidates |
| Canonical review bundles, fix-ci, corpus implement and interview | required set above | |

## Feature matrix

Every construct the required bundles use, with its disposition. "Supported"
means lowering and execution exist and are tested today. "Tracked defect"
names the task in `.ai/plans/fabro-unified-task-list.md` that owns the fix.

### Graph and node constructs

| Feature | Used by | Disposition |
|---|---|---|
| `Mdiamond` start, `Msquare` exit, `rankdir`, `label` | all | supported |
| `parallelogram` command with `script`, `timeout`, `output_schema="routing"`, `stdin_source` | code-review, security-review, fix-ci, implement-plan | supported |
| `stdin_source="context.internal.run_id"` | code-review, security-review | supported: `kv` carries the run id |
| `stdin_source="context.parallel.results"` | code-review, security-review | supported: the fan-in publishes `parallel.results` (`id, index, item_label, status, context_updates` in branch order) and `parallel.branch_count`; `context_updates` is the branch's diff against the fork snapshot and never merges into the parent (`crates/fabro/acceptance/scenarios/parallel-results/CONTRACT.md`, black box `parallel_*` and `for_each_*`) |
| agent node (`prompt`, `@prompts/*.j2`, `{% include %}`) | all but fix-ci commands | supported |
| `output_schema="@schemas/*.json"`, `output_retries` | code-review, security-review | supported |
| `on_failure="route"`, `"exit"` (node and graph) | code-review, security-review | supported |
| `on_failure="succeed"` | code-review, security-review | supported with Fabro's promotion order: a failure an explicit route matches stays failed, an unmatched failure is promoted and reports `succeeded` (oracle cases `succeed_*`) |
| `max_retries`, `default_max_retries` | code-review, security-review, implement-plan | supported |
| `component` with `for_each`, `max_parallel`, `tripleoctagon` fan-in | code-review, security-review | supported: each branch is a child invocation from the fork snapshot, `max_parallel` is one slot per attempt (missing or invalid is 4, zero is 1, a branch in backoff holds none), static and `for_each` branches share the fan-in, empty lists join with no model call (`crates/fabro/steps/tests/parallel.rs`, `crates/core/execution/tests/admission.rs`) |
| `class` with `model_stylesheet` (including `{% set %}`, `{% if %}`, `inputs.*`) | code-review, security-review, implement-issue | supported |
| `default_fidelity`, `fidelity="truncate"`, `fidelity="summary:high"`, `thread_id`, `default_thread` | code-review, security-review, implement-issue, interview | supported: Fabro's preambles for every mode, resolution edge, node, graph, `compact`; threads resolved edge, node, graph, class, previous node, retained on the native backend at `full` (`crates/fabro/FORMAT.md`, "Fidelity and threads") |
| `project_memory=false` | code-review, security-review | supported: a prompt node reads no instruction files; agents read Fabro's per-profile files from the Git root to the working directory |
| Sub-agents (`spawn_agent`, `send_input`, `wait`, `close_agent` on native agents) | any bundle's agent may delegate | supported: every `backend="api"` agent has Pebble's sub-agent tools, as every API-backend agent has Fabro's; children share the scope, model and tool hooks, never the question tool; results, failure, cancellation, shutdown order and event identities match the reference (`crates/fabro/FORMAT.md`, "Native Pebble"); differences below |
| `stall_timeout` | code-review, security-review | supported: the stall watchdog (default 30 m, `0s` disables); a pending question parks it |
| `loop_restart_signature_limit`, `loop_restart` circuit breaker | fix-ci, implement-plan | supported: default 3, minimum 1, restart edges admit only `transient_infra`, counts survive restart and resume |
| `human.default_choice`, `review_target`, human gate `timeout` | interview | supported: expiry takes the default choice or Fabro's retry outcome; the review target is validated and shown |
| `goal_gate`, `retry_target` (graph and node), `max_visits`, `max_node_visits` | fix-ci, implement-plan | supported |
| `house` manager loop, `stack.child_workflow`, `manager.max_cycles` | implement-issue | supported: one child per manager attempt at one durable call site, Fabro's 45 s poll and `max_cycles` normalization, stop condition at each poll, cancellation and failure propagation (`crates/fabro/steps/tests/manager.rs`, `crates/fabro/acceptance/tests/workflow.rs`) |
| `hexagon` human gate, `question_type` (`yes_no`, `confirmation`, `multiple_choice`, `multi_select`, `freeform`), accelerator labels, `freeform=true` edge | interview | supported; a `multi_select` answer is `Answer::choices` (Fabro's `multi_selected` `option_keys`), the first key routes and every key and label is recorded |
| `tab` prompt node | interview | supported: `fabro/prompt`, one tool-free `lithos-llm` call with the output contract and repair turns; ACP is refused as Fabro refuses it (`crates/fabro/steps/tests/prompt.rs`, black box `a_prompt_node_makes_one_tool_free_model_call`) |
| `model`, `provider`, `reasoning_effort` on nodes | fix-ci, implement-plan | supported for `backend="api"` and prompt nodes; `provider="openrouter"` resolves through the catalog to OpenAI's chat completions protocol, which the OpenAI twin serves (black box `run_model_defaults_reach_openrouter_through_chat_completions`) |
| Conditions: `outcome=succeeded`, `outcome!=succeeded`, `context.K=V`, `&&` | all | supported (`!=` is in the grammar: `conditions.rs`, `holds("outcome!=failed", …)`) |
| `{{ inputs.* }}` in `goal`, `script`, prompts | code-review, security-review, fix-ci | supported; `[run.inputs]` defaults are read |
| `import` placeholders | none of the bundles; corpus and docs | supported at load with Fabro's prefixing, boundary rules, inherited defaults, class propagation, retry-target rewriting, nesting and cycle rejection (`lowering.rs`, black box `an_import_is_expanded_at_load_and_its_nodes_run_under_the_prefix`) |
| Output values above 100 KiB | code-review, security-review (large command output) | supported: the `OutputStore` capability with the local blob store; durable `blob://sha256/…` references, logical reads through `stdin_source` and prompts (`steps.rs`, `large_command_output_is_offloaded_and_reads_back_logically`) |

### `workflow.toml`

Fabro parses `workflow.toml` with the same schema as `settings.toml` and
`project.toml` (`lib/foundation/fabro-config/src/layers/settings.rs`).
Top-level keys are `_version`, `project`, `workflow`, `environments`, `run`,
`cli`, `server`, `llm`; anything else is a hard error. Nested tables use
`deny_unknown_fields`. Precedence, highest first: CLI overrides, `workflow.toml`,
`.fabro/project.toml`, `~/.fabro/settings.toml`, server defaults, built-in
defaults. `[run.inputs]` replaces wholesale across layers; `--input` wins per
key.

Petri acts on `[workflow] graph`, `[run.inputs]`, `[run] goal`,
`[run.model]`, `[run.execution]`, `[run.environment]` with `[environments.*]`,
and `[run.prepare]`. Every other section is diagnosed at load
(`crates/fabro/frontend/src/lower/workflow_toml.rs`) under the readiness
plan's item 1 rule: a platform-only option that has no effect warns
(`ignored.workflow_toml.<section>`, with why); a feature essential to the
requested work fails with a specific `unsupported.workflow_toml.<section>`
error before any node runs; a key Fabro's own parser refuses is
`unsupported.workflow_toml.key` with Fabro's rename hint. Nothing is dropped
silently. The whole file is validated before `[run.prepare]` runs. The
disposition column below is what `petri check` and `petri run` do today;
"tracked" names the task that implements the option, at which point its
diagnostic goes away.

| Section and options | Effect in Fabro | Petri disposition today |
|---|---|---|
| `_version` (`1`) | schema version | supported: any other value is `unsupported.workflow_toml.version`; the legacy `version` key is `unsupported.workflow_toml.key` |
| `[workflow]` `name`, `description`, `graph`, `metadata` | `graph` names the entry point | supported: `graph` selects the file; the rest is metadata |
| `[run]` `goal` (string or `{file}`), `working_dir`, `metadata` | goal text, local cwd | `goal`: supported, the run goal when the graph sets none (the graph attribute wins, as in Fabro); `working_dir`, `metadata`: warn (platform-only) |
| `[run.inputs]` | `{{ inputs.* }}` defaults | supported |
| `[run.model]` `provider`, `name`, `controls.reasoning_effort`, `controls.speed` | default model and request controls | supported: the defaults an LLM node gets below the graph's `default_model` / `default_provider`; `controls.speed` is the default `speed` |
| `[run.model.fallbacks]` `"<model>" = [ "provider:model", ... ]` | model fallback chain | supported: chains keyed by the canonical requested model, resolved against the runner's catalog and available providers with Fabro's notices, reasoning effort mapped per target (`NoNearbyReasoningLevel`, `ChainEmpty`); a provider-local model error (the reference's `failover_eligible` mapping) moves a native agent or prompt stage to the next target, the conversation kept; see "Model fallback" in `crates/fabro/FORMAT.md`. Used by code-review and security-review |
| `[run.prepare]` `steps[].script`/`command`/`env`, `timeout` (default 5m) | runs before the first node | supported: lowered as command nodes `run_prepare_N` between `start` and its successors, in the selected environment, with the step `env`, the section `timeout` and `on_failure="exit"`; exactly one of `script`/`command` per step (else `unsupported.workflow_toml.run.prepare`) |
| `[run.execution]` `mode` (`normal`, `dry_run`), `approval` (`prompt`, `auto`) | dry run and auto approve | supported as launch defaults (`Graph.params["fabro.launch"]`, read by the CLI); `--dry-run`, `--auto-approve`, `--interactive`, `--interview-script` win |
| `[run.environment]` `id`, `image`, `resources`, `network`, `lifecycle`, `labels`, `env` and `[environments.<id>]` `provider` (`local`, `docker`, `daytona`), `image.docker`, `image.dockerfile` (inline or `{path}`), `resources`, `network`, `lifecycle`, `labels`, `env` | sandbox selection | supported: `provider` selects the backend when `--backend` is absent (`local` host, `docker` Docker plugin, `daytona` Daytona plugin); `image.docker` is the scope's container image; `env` is the scope environment, with `{{ secrets.NAME }}` a `$secret` reference resolved at spawn from `PETRI_SECRET_NAME` (a missing secret fails the command `secret_unavailable`); `resources` size a Daytona runner. An unknown `id` or provider is an error. `cwd`, `network`, `lifecycle`, `labels`, `image.dockerfile` warn `ignored.workflow_toml.environments.<id>.<key>` (platform-only; the runner builds no image) |
| `[run.agent]` `fabro_tools` | run-management tools for agents | explicit exclusion: platform-only, warn `ignored.workflow_toml.run.agent.fabro_tools` when `true` |
| `[run.agent]` `skills` | refused: `[run.agent]` denies unknown keys; skills come from `$FABRO_HOME/skills`, `<root>/.fabro/skills`, `<root>/skills` (`fabro-agent/src/skills.rs`) | a Petri extension (accepted difference below): a list of extra directories searched after Fabro's three, warned `fabro.petri_extension`; any other shape is `unsupported.workflow_toml.run.agent.skills`. Fabro's three directories, their order and precedence are implemented for native agents (`crates/fabro/FORMAT.md`, "Skills"; fixtures `testdata/skills`) |
| `[run.agent]` `compaction` | refused: `[run.agent]` denies unknown keys; compaction is always on with hardcoded values (`fabro-agent`'s `SessionOptions`: 80 percent trigger, six preserved turns) | refused `unsupported.workflow_toml.key`, with a message naming the values in force; those values are lowered onto every agent node and run through Pebble's automatic compaction (`crates/fabro/FORMAT.md`, "Compaction") |
| `[run.agent.mcps.<name>]` `type` (`http`, `stdio`, `sandbox`) with `url`, `headers`, `protocol`, `script`, `command`, `env`, `port`, `startup_timeout`, `tool_timeout`, `enabled`; or `id` | MCP servers for agent sessions | supported: every inline field with Fabro's rules and defaults, merged across the settings, project and workflow layers by name (`crates/fabro/FORMAT.md`, "MCP servers"); stdio servers on Petri's host, http servers from the host, sandbox servers in the scope's environment, reached through the provider's preview URL (the host's loopback, the Docker plugin's port forward, Daytona's preview link); tools registered with Pebble under `mcp__<server>__<tool>` and run through the normal tool path, hooks included; explicit exclusions: `id` (a server-managed catalog reference, `unsupported.workflow_toml.run.agent.mcps.reference`), `protocol = "sse"` (`unsupported.workflow_toml.run.agent.mcps.protocol`), a secret token outside a whole `env`/`headers` value (`unsupported.workflow_toml.run.agent.mcps.secret`); `{{ env.* }}` is an error as in Fabro |
| `[[run.hooks]]` `id`, `name`, `event`, `matcher`, `blocking`, `timeout`, `sandbox`, and one of `script`/`command`, `url`+`headers`+`tls`, `prompt`+`model`, `agent="enabled"`+`prompt`+`model`+`max_tool_rounds` | local hooks | supported: every field, transport and event, merged with `.fabro/project.toml` and the host's settings layer by `id`, run by the local hook service with Fabro's matching, decisions, placement and timeouts (`crates/fabro/FORMAT.md`, "Hooks"); a layer that cannot be read is an error, so a configured hook is never skipped silently; `checkpoint_saved` warns and does not run (accepted difference below); `stage_retrying` is dispatched (the reference never fires it) |
| `[run.checkpoint]` `exclude_globs`, `skip_git_hooks`, `commit_timeout` | Git checkpoints | explicit exclusion: platform Git; warn `ignored.workflow_toml.run.checkpoint` |
| `[run.clone]` `enabled`, `depth` | server clone depth | supported (task 17): the root `start` stage checks the repository the run was loaded from out into the workspace at `depth` commits (default 100, `0` full), on the host and in a container through the executor; `enabled = false` starts empty. Difference: the clone is of the local checkout at `HEAD`, not a fetch from the remote, and `origin` is the checkout's own `origin` |
| `[run.run_branch]`, `[run.meta_branch]` `enabled`, `push` | Git branches | explicit exclusion: platform Git; warn `ignored.workflow_toml.run.run_branch` / `run.meta_branch` |
| `[run.pull_request]` `enabled`, `draft`, `auto_merge`, `merge_strategy` | PR creation | explicit exclusion: platform publication; warn `ignored.workflow_toml.run.pull_request` |
| `[run.artifacts]` `include` | artifact upload globs | explicit exclusion for upload; local retention keeps the files; warn `ignored.workflow_toml.run.artifacts` |
| `[run.integrations.github]` `permissions`, `additional_repositories` | minted `GITHUB_TOKEN` | explicit exclusion: the run inherits the ambient token or none; warn `ignored.workflow_toml.run.integrations`. A bundle whose work needs the minted token (fix-ci) is blocked on a stand-in, task 17 |
| `[run.git.author]` `name`, `email` | checkpoint commit identity | explicit exclusion; warn `ignored.workflow_toml.run.git` |
| `[run.notifications.<name>]`, `[run.interviews]` `provider`, `slack.channel` | Slack | explicit exclusion; warn `ignored.workflow_toml.run.notifications` / `run.interviews` |
| `[run.scm]` | manifest metadata | explicit exclusion; warn `ignored.workflow_toml.run.scm` |
| `[project]`, `[cli.*]`, `[server.*]`, `[llm.*]` | inert in a workflow file | accepted but inert; warn `ignored.workflow_toml.<section>`; the legacy `[llm]` keys (`provider`, `model`, `temperature`, `max_tokens`, `fallbacks`, `fallback`) are `unsupported.workflow_toml.key` with the `[run.model]` hint, as in Fabro. `[environments.*]` without a `[run.environment]` that names one is inert |
| Rejected legacy top-level keys (`version`, `vars`, `setup`, `sandbox`, `hooks`, `mcp_servers`, ...) and unknown `[run]` keys | hard error with a rename hint | `unsupported.workflow_toml.key` with Fabro's rename hint |

Per-bundle `workflow.toml` use is recorded under `workflow_config.platform_only_sections`
in the lock file.

## Tracked departures to retire

Found by the differential matrix (task 18) and recorded as a decision record
with its retirement condition:

- **The interview bundle needs a `[run.model]` default**
  (`decisions/interview-run-model-migration.toml`). Fabro takes the run's
  model from the launch; the standalone runner has no launch-level model
  default, so the matrix runs a recorded migration of the bundle's
  `workflow.toml`. Retire when `petri run` gains a launch-level default.

Resolved by the matrix (Petri fixed, no departure): a `yes_no` or
`confirmation` gate records `yes`/`no` under `human.gate.<node>.answer` as
Fabro does, and a freeform answer sets `human.gate.label`.

Retired with the sandbox-driver re-pin (`a225832`): **command output gained
a final newline**. The plugin's exec path was byte-exact all along; the
newline came from Petri's line pump, which now records whether the process
terminated each line (`LogLine::terminated`), and the command step joins the
lines accordingly, so `command.output` is the script's output byte for byte
(`printf 'a\nb'` yields `a\nb`). The `command-output-trailing-newline`
record is deleted; the differential cells pass with `petri == fabro`
exactly, and a recurrence is reported as an unresolved
`value.trailing_newline` difference.

Retired by task 6: **parallel branch context**. Each branch now runs as a
child invocation from the fork snapshot, the fan-in publishes
`parallel.results` with every branch's `context_updates`, and nothing merges
into the parent context. Oracle case `static_fan_out_joins_all_branches`
records an empty context on both sides; its remaining difference is the path
(Petri lists the branch stages, Fabro records branch events), listed below.

Retired by task 7: **failure promotion order**. Petri now follows Fabro's
executor: a failure an explicit route matches stays failed and takes that
route; an unmatched failure under `on_failure="succeed"` is promoted and
reports `succeeded`. Oracle cases `succeed_keeps_a_failure_an_explicit_edge_matches`,
`succeed_keeps_a_failure_a_preferred_label_matches` and
`succeed_promotes_a_failure_no_explicit_edge_matches` match the pinned
Fabro with no departure. The `partially_succeed` spelling stays as an
accepted difference (below).

## Accepted differences

These stay in the contract. They are tested as Petri differences, not as
reference expectations. Each row is one decision record under
`crates/fabro/acceptance/decisions/`, which is the index the differential
comparison loads; two records found by the matrix have no row here:
`fabro-run-title-call` (Fabro's run-title request on the provider's small
default model before the first stage) and `fallback-repeated-tool-effect`
(a baseline defect, not an accepted Petri behaviour).

| Difference | Petri | Fabro |
|---|---|---|
| Firing cap | 500 firings per looped node; an explicit limit above 500 is refused at load | unlimited when visit limits are unset or zero |
| Invocation maximum | a hard 10,000 workflow invocations per run; cannot be raised or disabled | no counterpart |
| `outcome=success` | matched as `succeeded` with a warning until 2026-10-04, then refused | accepted, never matches |
| ACP tool hooks | best effort at `session/request_permission`, with a `fabro.hook.warning` naming the backend, hook, event and missing boundary for each unenforceable hook | ignored silently |
| `stage_retrying` hook | dispatched before each retry attempt; its decision is ignored | declared, never dispatched |
| Run-level hook reports | `run_complete`, `run_failed` and `sandbox_cleanup` reports are logged, not recorded in the event log (they belong to no firing) | platform events |
| Terminal echo | a stage's echoed output is bounded at 64 KiB (one marker names the log file, which keeps everything); a branch's lines carry its invocation (`[invocation-N/node#firing]`) | the platform's log viewer |
| `[run.agent]` keys | `subagents`, `compaction` and any key other than `fabro_tools`, `mcps` and `skills` are refused (`unsupported.workflow_toml.key`): the pinned Fabro's `[run.agent]` denies unknown keys and has no setting for sub-agents or compaction; sub-agents are always on for native agents, so the `subagents` refusal says so | refused by the parser |
| Context compaction | always on with Fabro's values (80 percent trigger, six preserved turns); Pebble's automatic compaction summarizes and replaces the history, and Petri never rewrites it (`crates/fabro/FORMAT.md`, "Compaction") | identical values, self-contained in `fabro-agent` |
| Compaction summary usage | the summary call is part of the prompt's bill: Pebble puts its usage and cost in `pebble.usage` and `pebble.cost_usd_micros`, and Petri breaks the same share out as a `fabro.compaction` event and the `pebble.compaction_*` metrics, read from the `Compaction` turn in Pebble's history | the summary usage is dropped entirely (`compact_context` discards `response.usage`) |
| `[run.agent] skills` | a Petri extension: extra skill directories searched after Fabro's three, warned `fabro.petri_extension` | refused by the parser |
| Skill files that do not parse | reported: a `fabro.skills.warning` event and a stderr line per skipped `SKILL.md` and per unsearchable directory, from Pebble's `SkillsDiscovered.skipped` report, and per workflow-named directory that does not exist, from Petri's own probe; the file is still skipped, as Pebble skips it | skipped silently |
| A prompt naming a missing skill (`/name`) | the node fails with class `skill_missing` | the stage fails with an `InvalidState` error |
| Sub-agent nesting and concurrency | a child may delegate again; one tree holds at most 4 sessions open at once (the root included; a finished child holds its slot until closed), and a spawn over the bound is the tool's answer (`Cannot spawn another agent`) | one level only (a child has no sub-agent tools; `max_subagent_depth = 1`); no bound on concurrent children |
| Sub-agent MCP tools | a child inherits the workflow's `[run.agent.mcps]` tools (task 13 registers them `allow_in_subagents`) and calls them through the parent's one connection, under the parent's hooks and attribution; readiness items 9b and 9d ask for inherited tools | a child session is built without `mcp_servers`, so it has no MCP tools |
| Sub-agent usage | the stage's `pebble.usage` is the parent session's own; the children's usage is the `pebble.subagents` metric and the `agent_activity` events | child usage merges into the stage's usage total through the event stream |
| `checkpoint_saved` hook | warning; the hook does not run | dispatched with no built-in behavior |
| Sensitive answers (`sensitive=true`, `$secret`) | a Petri extension | not defined |
| Skipped stages in the path | Petri records a `skipped` final outcome (oracle case `skipped_outcome_routes_like_success`) | no stage record |
| Parallel branches in the path | each branch's stage has a final record in its child invocation, so the path lists the branches in branch order between the fork and the join (oracle case `static_fan_out_joins_all_branches`) | branches are `parallel` events, not stages; the path skips them |
| `context_updates` of a branch | the diff against the fork snapshot: a key a branch writes back with the value it already had is not reported; a failure with an empty `failure_class` is dropped | every key the branch wrote |
| `for_each` over an empty list | the template fires once with the placeholder item `petri.parallel.empty`; the fan-in strips it and joins zero results with no model call | zero branches |
| `on_failure="partially_succeed"` | a Petri extension: Fabro's `succeed` promotion order, but the promoted stage reports `partially_succeeded` (oracle case `partially_succeed_policy_classifies_before_routing`, warned `fabro.petri_extension`) | refused by the validator (`on_failure_valid`) |
| `[environments.*] image.dockerfile` | a warning; the scope runs on the selected backend's default runner image, or on `image.docker` when named | builds the image on the platform |
| Workflow secrets | `{{ secrets.NAME }}` resolves from `PETRI_SECRET_NAME` in the standalone runner; an embedding host supplies its own `SecretProvider` | the platform vault |
| Output references | `blob://sha256/<hex>` in a local store under `<run_dir>/blobs`, replaceable through the `OutputStore` capability; a structured value's reference carries `#json`. Below Fabro's 100 KiB threshold, a fork also offloads the `for_each` source list (any size) and other fork snapshot values above 4 KiB from every branch child's context, and a fan-in publishes `parallel.results` above 4 KiB as a reference; the agent and prompt preambles and a nested workflow's start context put those back, `stdin_source` and prompted fan-ins read them back, and a condition sees the reference text | `blob://sha256/<hex>` in platform storage, materialized as `file://…/blobs/<hex>.json` for handlers; nothing under 100 KiB is ever a reference |
| Prompt events | `StepEvent::Custom` with `kind = "fabro.prompt"` / `"fabro.prompt.completed"` (see `crates/fabro/FORMAT.md`) | `stage.prompt` / `prompt.completed` |
| MCP events | `StepEvent::Custom` with `kind = "fabro.mcp.server"` (`starting`, `ready`, `failed`, `disconnected`, `stopped`) and `"fabro.mcp.tool"` (one per proxied call, with `status` and `duration_ms`); the failure line `mcp server \`<name>\` failed to start: ...` on the node's stderr | `agent.mcp.ready` / `agent.mcp.failed`; tool activity only through the generic tool events |
| MCP catalog references | `[run.agent.mcps.<name>] id = "..."` is refused at load: the standalone runner has no server-managed catalog | resolved from the server's catalog |
| MCP legacy SSE | `protocol = "sse"` is refused at load; only streamable HTTP is spoken (the pinned `rmcp` has no legacy SSE client; Fabro carries its own) | supported |
| MCP secrets | `{{ secrets.NAME }}` resolves as a whole `env` or `headers` value only, from `PETRI_SECRET_NAME` (or the host's provider) at launch; a token inside a command, script or URL is refused at load | resolved anywhere in the transport strings at the run boundary from the vault |
| MCP `sandbox` transport | launched through the scope's execution environment and reached through the provider's preview URL (the host's loopback, the Docker plugin's port forward into the container, Daytona's preview link with its token header); a provider without preview URLs fails the server with a named reason | a Daytona preview URL; local sandboxes fall back to localhost |
| MCP stdio working directory | the scope's workspace when the scope shares the host filesystem, else Petri's own directory | the run worker's directory |
| Model fallback: session handoff | the failed session's record resumes on the next route (`ResumeMode::UseModel`, same session id); the next model continues the unfinished turn with no new input (Pebble's `continue_prompt`), so a tool effect that already ran is never repeated | the session is discarded and a new one runs the original prompt from scratch on the next route, repeating any tool effect |
| Model fallback: provider-only candidates | a bare provider in a chain resolves to the same model id on that provider when its catalog lists it, else `NoCompatibleModel` | picks the provider's closest model by feature profile and price |
| Model fallback: unknown selectors | a selector no catalog row names is skipped with a notice unless the provider allows passthrough models | passed through for the provider to validate |
| Model fallback: configuration errors | a bad chain (provider-named or qualified key, two keys for one model, unknown key or provider) fails the first LLM stage with class `bad_config` | fails run start |
| Model fallback: ACP agents | no plan; the ACP command owns its model | the same |
| Model fallback: events | `StepEvent::Custom` kinds `fabro.fallback.{plan,route,usage,failover,stop}` and the once-per-run stderr notices | `agent.failover` events and run notices |
| Model fallback: recovery | a resumed node starts a new plan at position 0 on the primary; a request in flight at the crash may be sent again | sessions persist server-side |
| MCP server lifetime | one set of servers per agent node session, started before the agent and stopped after it (a retained thread's next node starts its own); a resumed run starts them again | one set per agent session; the same |

## Advanced agent milestones (item 9, milestones C1 to C5)

The overall item 9 gate: all five advanced-agent stages pass their own
acceptance gates. Status at task 16's finishing time (2026-09-07). Task 16
owns C5 and this checklist; the other four stages ran in parallel, so a stage
whose branch had not merged onto `swarm/integration` when task 16 finished is
marked "not verified" with the reason, not "passed".

| Stage | Acceptance gate | Branch / commit | Status |
|---|---|---|---|
| C1 model fallback and failover (item 9a, task 12) | scripted provider failures exercise selection order, session handling, terminal outcome, and complete usage/events | `swarm/task12-fallback`, commits `240e0c9`..`f0ce08e` and after (evidence `task12-fallback.md`) | passed on the branch with integration merged (tasks 13 to 16 in): selection order and the reference's notices (`fallback::tests`, 9 unit tests); through the binary with both twins injecting failures (`fabro_fallback_blackbox`, 15 cases: primary, qualifying and non-qualifying, a third provider, exhaustion, a tool effect kept across the handoff, cancellation, refusal, timeout, client retries, a workflow retry, effort mapping, repair turns, a retained thread, a prompt node); the outcome and per-route accounting rebuilt from public events (`fallback_events`, 3); the client's own retries and budget (`llm_client`, 2). Reference sequences derived from the pinned source, not captured from the binary |
| C2 MCP execution (item 9b, task 13) | a configured local MCP server's tool effect, hooks, output, events, cancellation and shutdown are verified | `swarm/task13-mcp`, commits `b67d3ee`..`72be261` (evidence `task13-mcp.md`) | passed: merged; the MCP suites and the scripted stdio server (`fabro_mcp_blackbox`, `testdata`) pass in the gate (1192) |
| C3 skills (item 9c, task 14) | versioned fixtures verify skill discovery, precedence, loading, prompt/tool behavior and events without Fabro | `swarm/task14-skills`, commits `c27fc3e`, `bdd5c5a` (evidence `task14-skills.md`) | passed: merged; the skills suites and fixtures (`testdata/skills`) pass in the gate; skill context is loaded into the system prompt, outside the agent history, so compaction cannot remove it (verified below) |
| C4 sub-agents (item 9d, task 15) | a parent delegates real work; results, ownership, cancellation, hooks and child identities match the reference | `swarm/task15-subagents`, commits `8861076`..`303f682`, `e8d20e1`, `269daca`, `ad2d472`, the merges `bd14208`, `3cbc1bc` and the task 16 merge (evidence `task15-subagents.md`) | passed: every native agent has Pebble's sub-agent tools (the pinned Fabro has no setting either; `[run.agent] subagents` stays refused because Fabro's parser refuses it); a parent delegates a workspace change to a child, hooks block inside a child, a child's failure is the parent's tool result, concurrent children and a grandchild, an interrupt closes the child, a retained thread keeps a child's result, resume restarts the stage, children never count against the invocation ceiling, accounting reconstructs from the public events. In-process `petri-fabro-steps::subagents` (14, the child memory and skills contract tests included since the Pebble re-pin), black box `fabro_subagents_blackbox` (8, one with an inherited MCP tool). Accepted differences above: nesting under the open-session bound, usage beside the parent's, inherited MCP tools |
| C5 context compaction (item 9e, task 16) | controlled histories trigger compaction and preserve required conversation/tool state, later thread use, usage, and events | `swarm/task16-compaction`, this branch | passed: the trigger below, at and above the 80 percent threshold; continuation, thread reuse, tool pairing across the boundary, summary failure, cancellation, resume fallback; public events and usage. In-process `petri-fabro-steps::compaction` (7), black box `fabro_compaction_blackbox` (4) |

Skill context across compaction (item 9e's cross-feature check, C3 landed):
skills reach a native session through Pebble's `with_skill_dirs`, which folds
the discovered skills into the session's system prompt and registers the
`use_skill`/`Skill` tool. The system prompt and the tool registry live on the
session outside `History`; compaction only ever replaces turns inside
`History` (`pebble-coding-agent` `compact_from`). So a compacted session keeps
its skills and its skill tool.

Sub-agents (C4) landed: the supervisor lives on the session outside
`History`, so a parent that compacts keeps it; verified end to end by
`petri-fabro-steps::subagents::a_parent_still_delegates_after_its_own_compaction`
(the turn after the parent's compaction spawns a child and waits for it, and
the later request carries the summary, not the discarded output). A child
inherits the parent's compaction settings and policy through Pebble's
`ChildDeps`: `a_child_compacts_under_the_inherited_settings_and_its_events_name_the_child`
shows a child crossing the trigger, its own summary call, Pebble's
`CompactionStarted`/`CompactionCompleted` on the shared stream under the
child's session naming the parent, and the stage's `pebble.subagents.sessions`
entry counting the compaction. A child's compaction produces no
`fabro.compaction` event and no `pebble.compaction_usage`: that event is read
from the parent agent's own history. Pebble bills the child's summary call to
the child's own prompt, so the ledger's `pebble.subagents.sessions` usage
carries it.

MCP (C2) landed: MCP tools are registered on the session with Pebble's
`tools(mcp.tools())` (`pebble.rs`), so they live in the session's tool
registry, outside `History`; compaction replaces only `History`, so a compacted
session keeps its MCP tools. Verified end to end by the readiness suites
(milestone D below): the `review` node calls the MCP tool on the compacted
thread, and the call is on the public stream (`fabro.mcp.tool` on `review`).


## Milestone D: the final readiness audit (item 10, task 20)

Status at task 20's finishing time (2026-09-07), audited against the code
and tests on `swarm/task20-readiness` with `swarm/integration` `dbddbfb`
merged, not against evidence prose. **Final readiness is met** with the
recorded gaps below, each with an owner; no required row keeps a deferred
status without one. The integration handoff is
[`crates/fabro/HANDOFF.md`](../HANDOFF.md); the event contract and its
coverage matrix are `crates/core/execution/EVENTS.md`. The evidence file is
`.ai/reviews/fabro-unified/task20-readiness.md`.

### Milestones

| Milestone | Passed | Reference |
|---|---|---|
| A (items 1, 2 initial) | yes | `fabro_blackbox::milestone_a_smoke_run_without_fabro_on_path`, `standalone.rs`, `fabro_dependencies.rs`; evidence `task5-milestone-a.md` |
| B (items 2 to 8) | yes | `fabro_milestone_blackbox` (3 cases, rerun at task 20: 4 of 4), `embedding::the_milestone_workflow_runs_through_the_embedding_boundary`, `fabro_terminal_blackbox` (10); evidence `task11-milestone-b.md` |
| C1 to C5 (item 9) | yes, the table above | `fabro_fallback_blackbox` (15), `fabro_mcp_blackbox` (9), `fabro_skills_blackbox` (4), `fabro_subagents_blackbox` (8), `fabro_compaction_blackbox` (4), their in-process suites |
| D (item 10) | yes, with the gaps below | the combined suites next |

### Combined, recovery and embedding tests

- Combined execution through the binary: `fabro_readiness_blackbox` (cells
  `readiness/combined@host/openai`, `readiness/combined-failure@host/openai`,
  `readiness/combined-cancellation@host/openai`): one run with a skill-guided
  plan, a hooked MCP write, a hooked sub-agent, a scripted decision, a
  `for_each` fan-out, a compaction and a later node reusing the compacted
  thread (with an MCP call on it), and a second thread failing over to the
  Anthropic twin and continuing there; failure (an exhausted chain) and
  cancellation (an interrupt inside a child's tool) apart. Each case checks
  output attribution, answer routing, the final status through `petri
  inspect`, the retained workspace, the absence of Git commits, tags and
  pushes in the repository the run prepared, and every item 9 family on the
  public stream.
- The same execution through the embedding example:
  `embedding_readiness::the_combined_workflow_runs_through_the_embedding_boundary`
  (a host with its own hooks, interviewer and sink; the same scripts spent in
  the same order, the same files, the run rebuilt from public events, replay
  equal to the live stream).
- Local replay and sandbox recovery, preserved and passing:
  `inspect_cli::inspect_reconstructs_a_restarted_run_with_children_after_the_process_exits`,
  `inspect_cli::inspect_reports_torn_logs_as_incomplete_and_corrupt_logs_as_errors`,
  `petri-execution::inspect::a_torn_coordinator_tail_is_reported_and_left_in_place`,
  `embedding::recovery_redelivers_with_stable_identities`,
  `petri::host::a_crashed_run_resumes_from_the_run_dir`,
  `petri-driver::resume` (`a_torn_tail_is_truncated_and_resumed`,
  `a_tampered_record_refuses_to_resume`, `an_undecodable_record_refuses_resume`,
  `resume_reacquires_held_scopes`, `resume_fences_the_crashed_container`,
  `interrupted_stop_resumes_the_same_sandbox_and_workspace`),
  `petri-execution::sandbox_recovery`
  (`allocation_crashes_recover_without_replacing_the_workspace`,
  `recovery_refuses_a_changed_fingerprint_or_a_lost_workspace`),
  `petri-fabro-steps::parallel::resume_keeps_a_finished_branch_and_finishes_the_unfinished_one`,
  `petri::controls::the_breaker_state_is_restored_on_resume`,
  `petri-fabro-steps::subagents::a_resumed_run_restarts_the_stage_and_keeps_an_unfinished_childs_files`.
  Fabro's, by the plan: Git-backed workspace restoration, database
  transaction recovery, platform event migration, publication deduplication,
  old-runner retention (`HANDOFF.md`, "What stays Fabro's").
- Production code has no Fabro, database or UI dependency:
  `fabro_dependencies.rs` (the full resolve graph, every kind, nested
  manifests and build scripts), `standalone.rs` (no `fabro` on `PATH`);
  `cargo tree -p petri-cli -e normal` names no database or UI crate (the
  twins and `axum` are dev-dependencies of the test crates only).

### Required rows: evidence

Every required row of the feature matrix and the `workflow.toml` table
above, with the tests that prove it on this branch. "Accepted difference"
names the decision record under `decisions/`; "gap" names the owner.

| Rows | Evidence |
|---|---|
| start, exit, `rankdir`, `label`; command nodes with `script`, `timeout`, `output_schema="routing"`, `stdin_source` (run id and `parallel.results`) | `petri-fabro-acceptance::routing::every_case_matches_the_fabro_oracle` (24 cases at the pin), `fabro_blackbox::contract_branch_envelopes_carry_index_status_and_context_updates`, `fabro_blackbox::contract_helper_merges_both_findings_into_the_report`, the code-review and security-review cells of `fabro_scenarios_blackbox`, differential `parallel_results_matches_the_pinned_fabro` |
| agent nodes (`prompt`, `@prompts/*.j2`, includes), `output_schema="@schemas/*.json"`, `output_retries` | `fabro_blackbox::edit_and_verify_through_native_openai`, `fabro_blackbox::edit_and_verify_through_native_anthropic`, `fabro_blackbox::malformed_agent_output_fails_the_stage_routably`, cells `code-review/invalid-output-repaired`, `code-review/repair-exhausted`, `security-review/malformed-response`; differential `edit_and_verify_matches_the_pinned_fabro` |
| `on_failure` `route`, `exit`, `succeed`; `max_retries`, `default_max_retries` | oracle cases `succeed_keeps_a_failure_an_explicit_edge_matches`, `succeed_keeps_a_failure_a_preferred_label_matches`, `succeed_promotes_a_failure_no_explicit_edge_matches`; cell `routing/failure-policy`; `fabro_terminal_blackbox::a_retry_is_announced_on_the_firing_that_retries`, `fabro_fallback_blackbox::a_workflow_retry_is_not_a_failover`. `partially_succeed`: accepted difference `partially-succeed-extension` |
| `component` with `for_each`, `max_parallel`, `tripleoctagon` fan-in | `petri-fabro-steps::parallel` (`static_branches_return_envelopes_that_never_merge_into_the_parent`, `mixed_failures_join_partially_and_all_failed_fails_the_fan_in`, `an_empty_for_each_list_joins_with_no_branches_and_no_child`, `a_repeated_fork_publishes_results_per_visit_with_its_own_children`, `a_nested_fork_runs_inside_its_branch_and_reports_its_own_results`, `cancelling_the_run_settles_every_branch_child`, `resume_keeps_a_finished_branch_and_finishes_the_unfinished_one`), `petri-execution::admission` (`one_slot_serializes_the_attempts_of_a_fork_and_two_slots_let_them_overlap`, `a_backoff_releases_the_fork_slot_so_a_queued_branch_runs_first`, `the_limit_counts_finished_children_and_names_the_refused_call`), `fabro_blackbox::for_each_branches_keep_distinct_values_under_one_key_in_item_order`, `fabro_blackbox::an_empty_for_each_list_joins_without_calling_the_model`, `fabro_blackbox::nested_joins_report_the_inner_results_inside_the_outer_envelope`, `fabro_blackbox::parallel_gates_bind_each_answer_to_its_own_branch`, cells `backend/dynamic-parallelism@host` and `@docker`, both readiness suites. Accepted differences `parallel-branches-in-path`, `branch-context-updates-are-a-diff`, `empty-for-each-placeholder`. **Scale**: two successive 1,000-item forks (`petri-fabro-acceptance::e2e::two_successive_thousand_item_forks_stay_under_the_ceiling`, in the routine suite since 2026-09-08: 71 s in a debug build at 337 MB peak RSS). Since 2026-09-07 a fork has at most `max_parallel` live children (`petri-execution::admission::a_fork_of_fifty_children_keeps_four_live_and_starts_them_in_order`, `cancelling_the_parent_finishes_every_queued_child_as_cancelled`, `resume_redispatches_queued_children_under_the_bound`), the coordinator store applies each record in place (`petri-execution::store::tests::a_rejected_event_leaves_the_state_and_the_log_untouched`, `a_failed_write_leaves_the_state_unchanged_and_is_reported`), and since 2026-09-08 the `fabro/fork` step offloads the fork snapshot so every branch child is declared from references (`petri-fabro-steps::parallel::a_fifty_item_fork_declares_its_children_from_references`, `a_resumed_run_reads_the_offloaded_results_after_the_fork`, `two_forks_in_sequence_keep_every_child_small`, `fabro_blackbox::a_fifty_item_fork_declares_small_children_and_prompts_still_see_the_list`, lowering `a_static_fork_keeps_a_nested_for_each_list_inline_in_its_snapshot`). Measured with one probe (`.ai/reviews/fabro-unified/fanout-offload-snapshot.md`): one 1,000-item fork went from 43 s, 8.1 GB peak RSS and a 266 MB `coordinator.jsonl` to 28.5 s, 161 MB and 2.7 MB; bytes per child are flat at about 2.7 KB from 100 to 1,000 items; the two-fork run went from 133 s, 39 GB and 1.66 GB to 58 s, 343 MB and 5.7 MB (release). What remains is the `fsync` per coordinator record (five per child, about 25 ms of the 28 ms per child) and the one O(N) `NodeExpanded` record per fork. The 10,000-invocation ceiling itself is proven by `exactly_ten_thousand_invocations_are_admitted_and_the_next_is_refused` in `test:long` |
| `class` with `model_stylesheet`, `model`, `provider`, `reasoning_effort`, `speed`, `max_tokens` | `petri-frontend-fabro::lowering`, cell `routing/bundle-defaults-and-overrides`, `fabro_blackbox::run_model_defaults_reach_openrouter_through_chat_completions`, `fabro_fallback_blackbox::reasoning_effort_maps_per_target_and_unfit_targets_are_skipped`, `petri-fabro-steps::hooks::speed_and_max_tokens_reach_the_model_requests` |
| `default_fidelity`, `fidelity=*`, `thread_id`, `default_thread`, `project_memory` | `petri-fabro-steps::hooks::full_fidelity_nodes_continue_their_thread_and_others_start_fresh`, `petri-fabro-steps::hooks::edge_fidelity_wins_and_a_lost_thread_degrades_to_summary_high`, `petri-fabro-steps::hooks::project_memory_follows_the_profile_and_the_node_kind`, `fabro_hooks_blackbox::full_fidelity_nodes_share_one_conversation_through_the_binary`, `fabro_compaction_blackbox::a_node_that_lost_its_conversation_starts_at_summary_high`, `fabro_fallback_blackbox::a_retained_thread_continues_on_the_fallback_route`, both readiness suites (the `notes` and `docs` threads, the memory rule in the first request). Across `petri resume` a retained thread restarts from the `summary:high` preamble under Fabro's discarded-session rule (`petri-fabro-steps::subagents::a_resumed_run_restarts_the_stage_and_keeps_an_unfinished_childs_files`); this matches the pinned Fabro, whose `AgentApiBackend` keeps full-fidelity sessions in an in-memory map per worker, so it is parity, not a gap (owner decision of 2026-09-08; persisting threads would be an improvement beyond Fabro) |
| sub-agents | the C4 row above; `fabro_subagents_blackbox` (8), `petri-fabro-steps::subagents` (14), both readiness suites (a hooked child, cancellation inside the child's tool). Accepted differences `subagent-nesting-and-concurrency`, `subagent-mcp-tools`, `subagent-usage-separate` |
| `stall_timeout`, `loop_restart_signature_limit`, `goal_gate`, `retry_target`, `max_visits`, `max_node_visits` | `petri::controls::an_idle_run_is_cancelled_by_the_watchdog`, `petri::controls::a_pending_question_parks_the_watchdog`, `fabro_blackbox::a_stalled_run_is_cancelled_by_the_watchdog`, `petri::controls::a_repeated_deterministic_failure_trips_the_breaker_across_restarts`, `petri::controls::a_restart_edge_admits_only_transient_failures`, `petri::controls::the_breaker_state_is_restored_on_resume`, `petri::controls::node_visit_totals_survive_a_restart_while_context_resets`, cell `routing/goal-gate-restart-and-visit-limit`, the oracle |
| `hexagon` gates, every `question_type`, accelerator labels, `freeform=true`, `human.default_choice`, `review_target`, gate `timeout` | `fabro_blackbox::a_multi_select_answer_routes_on_the_first_key_and_records_all`, `fabro_blackbox::an_invalid_scripted_answer_is_re_asked_and_the_second_entry_routes`, `fabro_blackbox::a_delayed_reply_lands_on_its_gate`, `fabro_blackbox::a_withheld_reply_expires_into_the_default_choice`, `fabro_blackbox::a_withheld_reply_without_a_default_fails_with_the_retry_outcome`, `fabro_blackbox::a_review_target_gate_shows_its_reference_in_the_terminal`, `fabro_terminal_blackbox::interactive_multi_select_takes_comma_separated_keys`, `fabro_terminal_blackbox::interactive_freeform_takes_a_line_of_text`, `fabro_terminal_blackbox::interactive_invalid_input_is_refused_and_asked_again`, `fabro_terminal_blackbox::interactive_without_terminal_input_fails_closed_with_a_reason`, the eleven `interview/*` cells, differential `interview_scripted_choices_match_the_pinned_fabro`. Accepted difference `sensitive-answers`; tracked departure `interview-run-model-migration` (retire when `petri run` gains a launch-level model default; owner Petri CLI) |
| `tab` prompt nodes | `petri-fabro-steps::prompt`, `fabro_blackbox::a_prompt_node_makes_one_tool_free_model_call`, `fabro_fallback_blackbox::a_prompt_node_fails_over_and_repairs_on_its_plan`. Accepted difference `event-kinds` |
| `house` manager loop, `stack.child_workflow`, `manager.max_cycles` | `petri-fabro-steps::manager` (`a_thousand_polls_consume_one_child_then_exhaustion_cancels_it`, `a_redispatched_attempt_reattaches_and_a_new_attempt_starts_a_new_child`, `the_stop_condition_reads_the_parent_context_and_cancels_the_child`, `a_parent_cancel_cancels_the_child`, `max_cycles_normalizes_as_fabro_does`), `petri-fabro-acceptance::workflow`, the seven `implement/*` host cells (inline graphs of the bundle's shape: an accepted migration, task 17). **Gap, owner the implement bundle (Fabro)**: `implement/child-runs-successfully@docker/openrouter` stays `blocked` because the pinned child's verify script needs the Fabro repository's own toolchain; a container adds no manager-loop behavior |
| conditions, `{{ inputs.* }}`, `[run.inputs]`, `import` | `petri-frontend-fabro::conditions`, the oracle, `fabro_blackbox::workflow_toml_inputs_bind_and_unsupported_sections_are_reported`, `petri-frontend-fabro::lowering::imports_expand_at_load_with_fabro_rules`, `fabro_blackbox::an_import_is_expanded_at_load_and_its_nodes_run_under_the_prefix` |
| output values above 100 KiB, `parallel.results` offload, fork snapshot offload | `petri-fabro-steps::steps::large_command_output_is_offloaded_and_reads_back_logically`, `petri-fabro-steps::parallel::a_fifty_item_fork_declares_its_children_from_references`, `a_resumed_run_reads_the_offloaded_results_after_the_fork`, `fabro_blackbox::a_fifty_item_fork_declares_small_children_and_prompts_still_see_the_list`. Accepted difference `output-references` |
| `_version`, `[workflow]`, `[run]`, platform-only and rejected sections | `petri-frontend-fabro::lowering::workflow_toml_sections_warn_or_reject_and_never_pass_silently`, `fabro_blackbox::workflow_toml_inputs_bind_and_unsupported_sections_are_reported`, `fabro_blackbox::the_complete_configuration_is_validated_before_preparation_starts`. Accepted differences `checkpoint-saved-hook`, `run-agent-keys` |
| `[run.model]`, `[run.model.fallbacks]` | the C1 row above; `fabro_fallback_blackbox` (15), `petri::fallback_events` (3), `petri-cli::llm_client` (2), both readiness suites (a failover on the `docs` thread, the chain exhausted in the failure case). Accepted differences `fallback-session-handoff`, `fallback-resolution`; baseline defect `fallback-repeated-tool-effect` |
| `[run.prepare]`, `[run.execution]`, `[run.environment]`, `[environments.*]`, secrets | `fabro_blackbox::run_prepare_steps_run_before_the_nodes_and_a_failure_stops_the_run`, `fabro_blackbox::a_cancel_during_run_prepare_stops_the_run_before_any_node`, `fabro_blackbox::run_execution_settings_are_the_launch_defaults`, `fabro_blackbox::run_environment_env_and_secrets_reach_the_commands_and_stay_masked`, `petri-frontend-fabro::lowering::run_environment_and_prepare_lower_onto_the_scope_and_the_graph`, `fabro_terminal_blackbox::a_docker_run_keeps_its_sandbox_after_success_and_prune_removes_it` (and the failed and cancelled Docker cases), the five Docker cells. Accepted differences `image-dockerfile-warning`, `workflow-secrets-source`. **Docker coverage is thin by design**: four Docker cases plus five Docker cells run live here and on Linux CI; Daytona is mapped but never exercised (its gate is separate, not claimed) |
| `[run.agent]` `fabro_tools`, `skills`, `compaction`; `[run.agent.mcps]` | the C2, C3, C5 rows; `petri-frontend-fabro::lowering::run_agent_skills_is_a_warned_extension`, `petri-frontend-fabro::lowering::mcps_lower_onto_agent_nodes_and_into_nested_workflows`, `fabro_mcp_blackbox::unsupported_mcp_settings_are_refused_before_the_run`, `fabro_mcp_blackbox::a_sandbox_server_in_a_docker_scope_is_reached_through_the_plugins_forward`, both readiness suites. Accepted differences `mcp-catalog-and-transports`, `mcp-server-lifetime`, `run-agent-skills-extension`, `malformed-skills-reported`, `missing-skill-failure-class`, `compaction-always-on`, `compaction-summary-usage` |
| `[[run.hooks]]` | `petri-fabro-steps::hooks` (`command_hooks_fire_at_every_reference_phase_with_fabros_payload`, `a_blocking_run_start_hook_stops_the_run_before_work`, `command_decisions_skip_block_and_ignore_nonblocking_hooks`, `edge_hooks_override_and_block_routes`, `native_tool_hooks_block_pre_and_observe_post`, `acp_tool_hooks_are_best_effort_with_explicit_warnings`, `a_command_hook_timeout_blocks`, `http_hooks_post_the_context_and_fail_open`, `prompt_hooks_evaluate_with_one_model_call_and_fail_open`, `agent_hooks_investigate_the_workspace_then_decide`, `hooks_load_from_every_layer_and_merge_by_id`, `run_failed_then_sandbox_cleanup_run_at_the_run_end_in_fabros_order`, `parallel_start_and_parallel_complete_surround_the_branches`), `fabro_hooks_blackbox::a_configured_hook_blocks_a_real_tool_effect_in_the_native_backend`, `fabro_milestone_blackbox` (run-end hooks), both readiness suites (hooks on a skill-driven call, an MCP call, inside a child; every report on the public stream). Accepted differences `acp-tool-hooks-best-effort`, `stage-retrying-hook`, `run-level-hook-reports`, `checkpoint-saved-hook` |
| `[run.clone]` | the code-review and security-review cells of `fabro_scenarios_blackbox` (the fixture repository checked out on the host and in a container), the `fabro.checkout` event |
| pause, unpause, steer, cancellation, retention | `petri::controls::pause_holds_admission_and_unpause_releases_it`, `petri::controls::a_paused_run_can_still_be_cancelled`, `petri::controls::a_steer_reaches_the_stage_and_does_not_answer_its_question`, `fabro_blackbox::the_control_file_pauses_unpauses_and_steers_without_answering`, the cancellation cases of `fabro_milestone_blackbox` and `fabro_readiness_blackbox`, `fabro_blackbox::retain_never_deletes_the_workspace`. The pause is durable: `petri::controls::a_pause_survives_resume_and_holds_admission_until_unpaused`, `petri::controls::a_paused_resumed_run_can_be_cancelled`, `fabro_resume_blackbox::a_paused_run_stays_paused_across_resume_until_unpaused`, `fabro_resume_blackbox::inspect_reports_a_paused_run_as_paused`. Recovery through the binary: `fabro_resume_blackbox::a_killed_run_resumes_without_repeating_finished_work`, `a_gate_waiting_at_the_crash_asks_again_on_resume`, and the three refusal cases |

### Known limitations carried into the handoff

1. Fan-out scale: the bytes per branch child are bounded by the fan-out
   offload threshold and no longer grow with the item count (above); time
   is linear in N and dominated by the five `fsync` calls per child (one
   per coordinator record plus the engine-log header), about 25 ms of the
   28 ms per child; the one `NodeExpanded` record per fork is O(N) once.
   Live children are bounded to `max_parallel`. Owner Petri core.
2. Retained agent threads restart from the `summary:high` preamble on
   resume, as in Fabro (its `AgentApiBackend` keeps full-fidelity sessions
   in an in-memory map per worker); this is parity, not a gap. Pause is
   durable across resume (`petri resume --run-dir`).
3. The implement family runs inline graphs of the bundle's shape; its Docker
   cell is blocked on the Fabro toolchain. Owner the implement bundle.
4. Docker coverage is thin (four cases, five cells); ACP and Daytona are
   gated separately and not claimed.
5. `fix-ci` is excluded by the owner's decision of 2026-09-07.
6. The two deploy keys (`CODE_REVIEW_DEPLOY_KEY`, `FACTORY_DEPLOY_KEY`) do
   not exist; the owner creates them. Hosted CI has not run; the first run's
   required results are listed under the readiness gate checklist.
7. A `for_each` fan-out emits no typed `fork_started`, `branch_completed`,
   `fork_completed` (the `fabro.parallel.*` kinds and the invocation links
   carry the facts). Owner Petri core (`execution::events`).
8. Pebble does not export its project-memory loader; `fabro_steps::memory`
   mirrors it for prompt nodes. Owner Pebble.
9. The interview bundle needs a launch-level model default
   (`interview-run-model-migration`). Owner Petri CLI.

## Library pin

Pebble is pinned at `408638fe982ace5b570e04ba808be3a32d4001f7`
(`petri/readiness-batch`), the library batch the coordinator ran after wave 3.
Five changes reached Petri with the re-pin:

- The summary call's usage and cost are in the prompt report, so
  `pebble.usage` and `pebble.cost_usd_micros` include compaction and the
  `pebble.compaction_*` metrics break that share out.
- `Agent::continue_prompt` and `CodingAgent::continue_prompt` continue an
  unfinished turn on the next model with no new input, so the failover no
  longer sends a continuation message.
- `SubagentOptions::with_inherited_memory()` and `with_inherited_skills()`
  give a child the parent's project documents and skill directories, as
  Fabro's child has them.
- `SkillsDiscovered.skipped` reports each skipped skill file with a reason,
  so Petri's diagnostics read Pebble's own report instead of auditing the
  files itself.
- The sub-agent tools of one round run in the model's order
  (`ToolScheduling::Sequential`), so a `wait` beside its own `spawn_agent`
  sees the child.

`RetryEventObserver` needed no Pebble change: it was already exported as
`pebble_coding_agent::events::RetryEventObserver`. Petri installs it on the
client (`petri::build_llm_client`), so the client's own retries reach the
agent's event stream as `LlmRetry`.

## Pinned revisions

Every library Petri runs Fabro workflows through is pinned by revision. This
table is the citation the evidence records and `scripts/check-pins.py`
compare against the manifests (`Cargo.toml`, `crates/petri/cli/Cargo.toml`,
`crates/fabro/corpus-pin.txt`, `bundles.lock.json`). `mise run check:pins`
fails when any of them disagree. The row names are the keys of a record's
`pins` block.

| Pin | Revision | Repository | Role |
|---|---|---|---|
| `pebble` | `408638fe982ace5b570e04ba808be3a32d4001f7` | `lithoscomputer/pebble` (public) | the agent loop and coding agent (`pebble-coding-agent`, `pebble-agent`) |
| `lithos_llm` | `4aab27d7d42e7f762a8b6a3871c3db86816b0721` | `lithoscomputer/lithos-llm` (public) | provider transport and request retries |
| `sandbox_driver` | `5b9f9dae506560f030ca4ef43d0facbe7baa99dc` | `lithoscomputer/sandbox-driver` (public) | the sandbox plugin protocol and the host, Docker, and Daytona plugins |
| `twins` | `fedab8e6b9b8e2577bee7d93812a318d6adb4aa4` | `lithoscomputer/twins` (public) | the OpenAI and Anthropic provider twins the harness serves on loopback |
| `fabro_reference` | `b6482910e517d00dfc3c4a2f2d3e417c9348f7f6` | `fabro-sh/fabro` (public, `refs/pull/844/head`) | the reference Fabro the corpus, oracle, bundles, and differential matrix use |
| `runner_image` | `df708f910111` | `lithoscomputer/sandbox-images` (public) | the default runner images (`ghcr.io/lithoscomputer/ubuntu-*`) Docker and Daytona scopes start from (`RUNNER_PIN` in `crates/core/executor-sandbox/src/backend.rs`; PyYAML present since this revision) |

A change to Pebble, lithos-llm, or an MCP client library runs the owning
repository's required checks before Petri moves its pin; then this table, the
manifests, and the affected evidence records move together. The library batch
the readiness work asked for is pinned (Pebble `408638fe`, sandbox-driver
`5b9f9da`); the twins are pinned in both test crates that serve them.

## Readiness gate checklist

The black box plan's "Verification and readiness gate", item by item, with
where the evidence for each comes from. Status as of task 20 (2026-09-07),
on `swarm/integration` `dbddbfb` merged into the task 20 branch. "met" has a
passing check on that tree; "partial" names what is missing and who closes
it.

| Item | Evidence source | Status |
|---|---|---|
| Repeatable focused task on the same required set as CI | `mise run test:fabro:blackbox` runs `scripts/test-fabro-blackbox.sh`: every `petri-cli` `fabro_*blackbox` binary (the readiness suite included) plus `standalone`, `fabro_cli` and the oracle test, same build and features as `mise run test`, evidence and coverage report per run | met |
| Extended variations and repeated process-isolation runs in `check:nightly` | `test:fabro:blackbox:repeat` (three runs under different schedules), `test:long`, `test:fabro:blackbox:strict`, `test:fabro:differential`, `check:msrv`, `test:release` | met |
| Library changes run the owning repository's checks before Petri pins them | `README.md` "Library and repository gates", `DEVELOPING.md`; the "Pinned revisions" table above; `mise run check:pins` (the runner image included) | met; the batch is pinned |
| Protocol retry, Pebble replay, Petri retry, and cross-layer cases distinct; a provider interruption after a non-idempotent tool effect | `llm_client.rs`, `fabro_fallback_blackbox::a_tool_effect_is_not_repeated_across_a_failover`, `client_retries_are_spent_before_the_chain_advances`, `a_workflow_retry_is_not_a_failover`, `fallback_events.rs` | met |
| Required CI fetches and verifies pinned bundles and twins, requires the corpus, fails on an absent asset, binary, scenario, or backend | `.github/workflows/ci.yml`; `PETRI_REQUIRE_*`; `tests/support/fabro/require.rs` | partial: wired and verified locally; the two deploy keys do not exist, so the first hosted run fails at the bundle step until the owner creates them; hosted CI has not run |
| Every required host scenario in routine CI; the Docker subset on Linux | `mise run check` runs the whole suite on both runners; Docker cases skip on macOS and are required on Linux; every planned cell of `matrix.json` has a test | met locally; hosted run pending |
| The pinned Fabro comparison matrix as a required compatibility job; nightly adds repetitions | `fabro compatibility (ubuntu-24.04)`: cached pinned build, `test:fabro:differential` (five live cells, zero unresolved differences) | met locally; hosted run pending |
| A machine-readable record per scenario | `tests/support/fabro/record.rs`, one record per scenario cell, `compatibility.differential` from the engine records | met |
| Full failure bundles and compact success results retained in CI | the two `actions/upload-artifact` steps per job; the job summary carries `coverage.md` | met (pending the first hosted run) |
| Coverage report with required, passed, failed, blocked, excluded; skips and exclusions never count | `scripts/fabro-coverage-report.py`, strict in CI; with bundles fetched and every gate required: required 51 (the three readiness cells included), blocked 1 (`implement/child-runs-successfully@docker/openrouter`), excluded 6 (`fix-ci`) | met |
| Every bundle materializes with verified dependencies and concrete inputs | `bundles.lock.json`, the fetcher (5 of 5 verified locally); concrete inputs per scenario | met; `implement-issue` stays `required-blocked` for its Docker cell only |
| Every required scenario and backend cell passes with no skips, unmatched calls, unexpected interviews, or unused replies | the strict coverage report plus each record's `services[].unmatched_requests` and interview receipt | met, one blocked cell with its reason |
| Final context, files, side effects meet independent expectations | each scenario's assertions, recorded per record; the readiness suites assert files, Git state and the public stream | met |
| No unresolved result, routing, or side-effect difference | the differential decisions and the "Tracked departures" section (one tracked: `interview-run-model-migration`) | met |
| Every used feature and temporary form has a disposition | the feature matrix and the milestone D audit above | met |
| Inspection and replay trustworthy; cancellation and timeout leave no leak | `inspect_cli.rs`, `inspect.rs`, the milestone and readiness cancellation cases, `assert_no_leaked_processes`, the "No containers left behind" CI step, replay equal to the live stream (`embedding_readiness`) | met |
| Additional cutover requirements (ACP, Daytona, crash resume) gated separately | out of the initial scope by decision; not claimed | not claimed |

What the first hosted run must show, in order: the bundle step passes on
every job once the two deploy keys exist (until then it fails naming them);
the compatibility job restores or builds the pinned `fabro` binary and the
"Build the pinned fabro binary" step reports its time; `test:fabro:differential`
passes its five cells; every `check` job writes `coverage.md` to the summary
with zero skipped, missing, and failed required cells and one blocked cell;
the `fabro-evidence-*` artifacts exist; `check (ubuntu-24.04)` stays inside
its 90-minute budget.

## Where things are

| Artefact | Path |
|---|---|
| Pin | `crates/fabro/corpus-pin.txt` |
| Bundle manifest | `crates/fabro/acceptance/bundles.lock.json` |
| Bundle fetcher | `scripts/corpus-fetch-fabro-bundles.sh` |
| Fabro fetcher | `scripts/corpus-fetch-fabro.sh` |
| Parity harness | `crates/fabro/oracle/harness/` (`oracle_harness.py`, `scripted_acp_agent.py`) |
| Fixture regeneration | `scripts/oracle-regenerate.sh` |
| Fake ACP agent | `crates/fabro/acceptance/testdata/fake_acp_agent.py` |
| Dependency direction tests | `crates/petri/lib/tests/fabro_dependencies.rs`, `crates/petri/cli/tests/standalone.rs` |
| Fabro provisioning | `scripts/fabro-provision.sh` (cache `crates/fabro/corpus/fabro-target/`) |
| Differential matrix | `crates/petri/cli/tests/fabro_differential.rs`, `tests/support/fabro/{fabro_adapter,compare,evidence}.rs`, `crates/fabro/acceptance/DIFFERENTIAL.md` |
| Decision records | `crates/fabro/acceptance/decisions/` |
| Scenario references | `crates/fabro/acceptance/scenarios/<name>/fabro-reference/reference.json` |
| Reference-version checks | `crates/fabro/acceptance/tests/reference_version.rs` |
| Integration handoff | `crates/fabro/HANDOFF.md` |
| Combined readiness suites | `crates/petri/cli/tests/fabro_readiness_blackbox.rs`, `crates/petri/lib/tests/embedding_readiness.rs` |
| Event contract and coverage matrix | `crates/core/execution/EVENTS.md` |
