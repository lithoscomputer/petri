# The Fabro integration handoff

This page is what a host that embeds Petri to run Fabro workflows can rely
on, and the order of the work that remains on the Fabro side. Readiness item
10 (`.ai/plans/fabro-execution-readiness.md`) asked for it; the evidence
behind every claim is `crates/fabro/acceptance/CONTRACT.md` (the support
matrix, the readiness audit) and `crates/core/execution/EVENTS.md` (the event
contract and its coverage matrix).

Petri owns workflow semantics, execution, the durable run record (through
the store seam, the run directory by default), the public event stream and
the extension points. Pebble owns the agent loop.
`lithos-llm` owns provider transport. Fabro owns its platform: Git
checkpoints and branches, the database, the UI and API, publication,
notifications, the vault. Inside Petri, the Fabro frontend
(`crates/fabro/frontend`) owns Fabro's settings layers (`workflow.toml`,
`.fabro/project.toml`, the user settings) and the launch precedence; the
language and the step kinds a Fabro workflow runs on are the Attractor
component's (`crates/attractor`), and their identifiers carry the
`attractor` prefix. Nothing in Petri production code depends on,
links, or launches Fabro (`crates/petri/lib/tests/fabro_dependencies.rs`,
`crates/petri/cli/tests/standalone.rs`); every adapter that calls a Fabro
handler lives in Fabro.

Fabro depends on six Petri packages: `petri-runtime`, `petri-execution`,
`petri-store`, `petri-attractor-steps`, `petri-frontend-attractor` and
`petri-frontend-fabro`. It does not depend on the `petri` distribution crate
or on any GitHub Actions crate. `Runtime::standard()` is `petri-runtime`'s
and the Attractor registration is `petri-attractor-steps::register`, so the
six build a Fabro runtime alone; their build closure reaches only core,
`attractor` and `fabro` (`crates/petri/lib/tests/layering.rs`,
`fabro_dependencies_reach_neither_github_nor_the_distribution`). Fabro pins
one Petri revision by tag. `petri::build_llm_client` is the exception: it
lives in the distribution crate, so a host on the six packages builds its
`lithos_llm::Client` itself.

Petri names the libraries the two repositories share by `branch = "main"`.
A Fabro build that names the same sources links one copy of each, and
Fabro's `Cargo.lock` chooses the commit that ships. Petri's `Cargo.lock` only
decides what Petri's own CI builds.
The readiness suite (`mise run check:fabro:readiness`) is the gate after
either repository moves its lock.

## What a host implements

The optional interfaces are Petri-owned and versioned. A host installs any
subset; the unconfigured path stays the standalone runner.

| Need | Interface | Where |
|---|---|---|
| Build a runtime with the Fabro frontend and the Attractor step kinds a Fabro workflow runs on | `attractor_steps::register(Runtime::standard().frontend(Fabro::new()))` (`petri::attractor::register` through the distribution) and the `PebbleClient` capability (`petri::build_llm_client` with the host's `CredentialProvider` and catalog layers) | `crates/core/runtime/src/runtime.rs`, `crates/attractor/steps/src/lib.rs`, `crates/petri/lib/src/lib.rs` |
| Load and run one workflow | `Runtime::check` (lowering with diagnostics) over a file on disk, or `Runtime::check_source` over a workflow the host holds in memory: the repository-relative path, the text, and a `frontend::FileSource` for the settings files and `@file` references beside it (`frontend::MapFiles` is a map of paths to text; the host binds `petri.repository` in the inputs itself); then `execution::host::HostRun`, `host::run_configured`; `RunOptions::run_key` names the run with the host's own id | `crates/core/runtime/src/runtime.rs`, `crates/core/execution/src/host.rs` |
| Pin every model at run creation | Petri's, at `Runtime::check`, when the `PebbleClient` capability is installed: every agent and prompt node's `model`, `provider` and fallback chain resolve to concrete routes written on the node's config as the frozen `plan` (`{original, remaining, notices}`), and the graph the host persists is the resolved one, so dispatch and resume run the admitted routes. A selector the catalog cannot resolve refuses the graph with `attractor.model.unknown`; a malformed or provider-keyed `[run.model.fallbacks]` table with `attractor.model.fallbacks`. A pinned route the client can no longer address fails its stage with `llm:pinned_route_unavailable`. See `crates/attractor/FORMAT.md`, "Model resolution at admission" | `crates/attractor/steps/src/admission.rs`, `crates/core/runtime/src/runtime.rs` (`AdmissionPass`) |
| The run's durable record | `store::RunStore` and `store::RunLogs` (`crates/core/store`), installed with `Runtime::store`: open a run by key in one access mode (`Create`, `Write` under an `OwnerId`, `Read`), then `append` and `read` records per log (`LogId::Coordinator`, `Resources`, `Execution(id)`) and `put_blob` and `get_blob` by digest. The stored unit is the record, exactly the `record` value of a public event. Petri ships `RunDirStore` (the run directory) and `MemoryRunStore`; a host implements the two traits over its database and runs `testkit::run_store::conformance` against it. See "Two shapes" below | `crates/core/store/src/lib.rs`, `crates/core/testkit/src/run_store.rs` |
| Awaited extension points: admission, result preparation, transition, run end, scope release | `driver::lifecycle::ExecutionHooks`, installed with `Runtime::hooks`; `AdmitAttempt` (`Admit`, `Skip`, `Block`), `PrepareResult` (`Prepared` adjustments with the original evidence kept), `Transition` (`RouteOverride`, best-effort `problems`, a fatal `TransitionError`), `RunFinished`, `ScopeReleased`; notes returned at each point are durable records | `crates/core/driver/src/lifecycle.rs`; proven by `crates/petri/lib/tests/embedding.rs` and `embedding_readiness.rs` |
| The local hook system, or a replacement | `execution::hooks::HookService` behind `HookAdapter` and the `HookServiceHandle` capability; the standalone service is `attractor_steps::hooks::LocalHooks`. Every point, the ones steps ask themselves included (`ScopeReady`, `RunStarted`, the start stage's admission, `ForkStarted`, `ForkCompleted`, the tool boundary of both agent backends), reaches the one service through that handle, so a replacement receives each exactly once (`embedding::a_hook_service_runs_each_hook_once_at_its_point`). A host that installs its own `ExecutionHooks` and still wants `[[run.hooks]]` calls `register` first and wraps `Runtime::installed_hooks()`, forwarding every point, `run_finished` and `scope_released` included (the `EmbeddingHost` in `embedding_readiness.rs` is the pattern) | `crates/core/execution/HOOKS.md` |
| Questions and answers | `execution::interview::{Interviewer, InterviewDispatcher}`; `InterviewRequest` carries the interaction identity (node, firing, occurrence, invocation path), the question type and choices (each choice's optional `description` and `preview`, the question's optional `context`: a human gate's edge attributes `human.description` and `human.preview` and the previous stage's response; a native agent's question carries Pebble's option descriptions and previews); `InterviewReply::Answered(Answer)`, expiry, cancellation | `crates/core/execution/src/interview.rs` |
| Pause, unpause, steer, interrupt, cancel | `execution::controls` (the control service; `petri run --control <FILE>` is the terminal transport), `RunHandle` for cancel and kill. `ControlService::interrupt` and `interrupt_and_steer` stop a live agent stage's current model turn and keep its session (the text, else the next steer, is the stage's next input); they read the service's `LiveTurns`, which the host installs with `Runtime::capability(controls.turns())` beside `Runtime::hooks(controls.hooks(..))`, and refuse a stage with no turn in flight with `ControlError::NoLiveTurn`. The record is `control.requested` with `$interrupt`; the stage reports the stopped turn as `attractor.turn.interrupted` | `crates/core/execution/src/controls.rs` |
| The public event stream | `execution::events::{EventProjector, RunEventSink, replay_run, replay_since}`; `EVENT_CONTRACT_VERSION`. A public event carries its record unchanged under `record`, plus what Petri derived under `derived`; `execution::events::verify_export` proves the records equal the stored logs and replay, at the end of every run | `crates/core/execution/EVENTS.md` ("Export") |
| Durable inspection of a stored run | `execution::inspect::inspect_run` over a read handle (`inspect_run_dir` and `petri inspect --run-dir --json` over a run directory), `INSPECT_FORMAT_VERSION` | `crates/core/execution/INSPECT.md` |
| Fork a stored run at a position (rewind, fork, retry) | `execution::host::fork_from(rt, source, ForkPosition {execution, firing}, ForkOptions {rerun_last})` seeds a new run in the runtime's store from the source's records up to the position: the same graphs, the position execution's log cut after the firing's routing (before its first record with `rerun_last`), the finished children called before the position, and a `run.started` whose `forked_from` names the source and position. No sandbox lease is carried over; the host restores the checkpoint's commit in `scope_acquired`, then continues the run with `host::resume_configured` under the source's middleware. A position inside a child invocation is refused (`ForkError::PositionInChild`) | `crates/core/execution/FORK.md` |
| Output references and large values | the `OutputStore` capability (`BlobStore`); the default is a local store under `<run_dir>/blobs` writing `blob://sha256/<hex>` | `crates/attractor/steps/src/blobs.rs` |
| Secrets | the `SecretProvider` capability; the standalone runner reads `PETRI_SECRET_<NAME>`; records are masked before they are appended | `crates/core/executor/src/secrets.rs` |
| Sandboxes | every provider (host, Docker, Daytona) through the sandbox-driver JSON-RPC plugin protocol; `Retention` (`Always` is the Fabro default), `petri sandbox prune` | `crates/core/executor-sandbox/`, `README.md` |
| Skills home, memory | the `FabroHome` capability (else `FABRO_HOME`, else `$HOME/.fabro`); project memory is read from the Git root to the working directory per Fabro's profile rules | `crates/attractor/steps/src/skills.rs`, `memory.rs` |
| Compaction policy, MCP tool registration, sub-agent limits | `CompactionPolicyHandle`; MCP servers from `[run.agent.mcps]` are Petri-owned processes and connections; Pebble's sub-agent tools are on every native agent | `crates/attractor/FORMAT.md` ("Native Pebble" and after) |
| The host's in-run tools (Fabro's run tools) | the `HostTools` capability (`attractor_steps::host_tools`): builders of Pebble `RegisteredTool`s, called once per native session with a `HostToolContext` (run key, invocation, execution, node, firing, attempt). The tools register beside Pebble's own, so they run under the run's tool hooks, are recorded on the public stream under the stage, and reach a sub-agent through Pebble's inheritance when marked `allow_in_subagents`. The list a session ended up with (Pebble's, MCP, sub-agent and host tools, each with its description, source and category) is on the stream once per session as the `attractor.tools` progress payload, the source of a host's "tools available" view. `register_fabro_run_tools` is the builder; Fabro's adapter maps the context to `FabroRunToolServices`. The context needs the coordinator's `ExecutionIdentity`, which every run through `execution::host` has | `crates/attractor/steps/src/host_tools.rs` |

## Identities a host can rely on

- **Run and invocations.** The run declaration (`run.started`, the
  coordinator log's first record) names the run's key, its format version
  and the root invocation; the key is `RunOptions::run_key` when the host
  gave one, and the run id every sandbox of the run is labelled with. Every
  `RunEvent` carries `invocation` and `execution` when it has them, and
  `parent` (the calling execution, firing, attempt and call slot) on a nested
  invocation. A Fabro parallel branch is a child invocation; its entry node
  carries `meta.branch_role = {fork, index}`, the parent-side delegate
  `meta.branch = {fork, target, index}` and `synthetic: true`.
- **Stages.** `subject.node` is the node (`id`, instance `name`, step `kind`,
  the frontend's `meta` verbatim: `label`, `shape`, `kind` such as `command`,
  `agent`, `human`, `parallel`, `parallel.branch`, `parallel.fan_in`,
  `stack.manager_loop`, `classes`, `span`, `synthetic`; a command node's
  `script`; `edges`, the routing arms by edge id with each target, label and
  `condition` as written, which `route.applied` keys into). A host maps
  synthetic lowering nodes to the logical stage with `meta`, never with node
  names, and shows a stage's script and a decision's condition from `meta`.
- **Firings, visits, attempts.** `firing` is the durable identity of one visit
  of a node in one execution; `visit` is its 1-based ordinal among the node's
  firings, `attempt` the 1-based retry within the firing, `generation` the
  loop generation. A retry keeps the firing and advances `attempt`; a loop
  starts a new firing and advances `visit`.
- **Branches.** `BranchRole` (`none`, `fork {branches}`, `member {fork,
  index}`, `join {fork}`) on every subject of a fan-out, static or
  `for_each`: the parallel node is the fork, each branch node or clone a
  member of its index, the fan-in the join. `fork.started`,
  `branch.completed` and `fork.completed` carry the same `BranchRef {fork,
  index}` for both and one `ForkOccurrence {execution, fork, firing, visit,
  generation}` per fork visit; the `fabro.parallel.*` payloads carry the
  index and the same occurrence (`{fork, firing}`), and a branch child's call
  slot is `branch:<fork>@<firing>:<index>:<target>`. A host keys every
  branch fact on the occurrence, never on the fork it saw last.
- **Interactions.** A question's identity is the node, the firing, its
  occurrence within the run, and the invocation path; `InterviewRequest` and
  the `step.progress.recorded` whose `parsed.question` is the question
  carry it, `control.requested` with a `derived.answer` closes it with an
  answer, a `parsed.expired` closes it when the gate's own deadline passes
  (with the default the gate took, when it had one), and the interview
  receipt (`<run_dir>/interviews.json`) keeps every question and its
  disposition (`answered`, `cancelled`, `failed`, `timed_out`).
- **Agent sessions.** Pebble's session id, parent session id, stream id and
  sequence, and tool call id are in every backend envelope a
  `step.progress.recorded` forwards as recorded, never rewritten. A retained thread keeps one session across the nodes
  that share it; `attractor.thread` names the thread and fidelity per node.
- **Model routes.** `fabro.fallback.route` carries the position in the plan,
  the provider and model, whether the session was reused, and the session id.
- **Sandboxes and workspaces.** `invocation.declared`'s `sandbox` is the binding;
  `scope.acquired` names the sandbox a scope runs in (the provider, the
  provider's id, the image and snapshot when known, the working directory,
  the workspace and lease, the acquisition time; a dry run's scopes name the
  `simulated` provider, on which nothing exists) and `scope.failed` why it
  could not be acquired; `scope.released` records the retention outcome per
  lease (`retained`, `outcome`, `problems`) once the owning invocation
  finished. `petri inspect` reports every scope's workspace and the
  retrieval command for a container; the workspace survives success,
  failure and cancellation under `--retain always` (the Fabro default).

## Event positions

`EventId {log, seq, index}` is the position: the log the record came from
(`coordinator`, or `execution` with the id), the record's sequence in that
log, and the ordinal among the events one record produced (`0` is the
record's own event, which carries the stored line under `record`). Within one
log the order is total and causal (admission before start, start before
finish, the final finish before `visit.completed`, `visit.completed` before
`routing.resolved`, `routing.resolved` before `route.applied`, notes before
the record they annotate). Across executions `context.parent` and
`execution.declared`'s `predecessor` tie the streams together. A host records
the last `EventId` it has applied per log; on resume the driver
redelivers the regenerated suffix with the same identities, at least once,
and the host deduplicates by `EventId`. `replay_run` over the run directory
yields the same stream, event for event, floats included; `recorded_at` —
when each record was appended, read at the recording boundary and persisted
with it — is the same live and on replay, so run, stage, attempt and
interview times come from the logs, never from the time of a replay.
`observed_at` is the one live-only field. `run.paused` and `run.unpaused`
are coordinator records like any other, so replay carries them.

## Forks: rewind, fork and retry

Fabro's rewind, fork and retry are one Petri operation over its own
checkpoint record. Fabro's checkpoint ties a position `(execution, firing)`
to a Git commit. `execution::host::fork_from` seeds a new run from the
source's records up to that position (`crates/core/execution/FORK.md`): the
new run holds the same graphs, the position execution's log cut after the
firing's routing, the finished children the kept firings called, and a
`run.started` whose `forked_from` names the source key, the position and
whether the firing runs again. The source's later firings, later executions
and unfinished children are dropped; nothing of its sandbox leases is
carried over, and the fork's resource log starts empty.

The host then continues the new run exactly as it resumes a crashed one:
`host::resume_configured` under the source's middleware list, with
`RunOptions::run_key` naming the fork. The position execution's scopes are
acquired fresh, so `scope_acquired` runs before the first attempt after the
position, which is where Fabro restores the checkpoint's commit into the new
workspace. A fork is a run like any other afterwards: `inspect_run` reports
`forked_from`, the public stream starts with the fork's `run.started`, the
copied records keep their recording times, and `verify_export` holds before
and after the run.

Retry is a fork at the last position: without `rerun_last` it reruns nothing
and finishes as the source did; with it, the last stage runs again from its
first attempt. Rewind is a fork Fabro records as superseding the source. A
position inside a branch's child invocation is refused
(`ForkError::PositionInChild`); fork at the parent's firing instead.

## Two shapes for the run record

**A. Mirror.** The run directory stays Petri's source of truth and Fabro
projects the public events into its own tables through `RunEventSink`.
Resume reads the run directory; the UI reads the database; a lossy sink is
completed with `replay_run` or `replay_since`, deduplicated by `EventId`.

**C. Records are the store.** Fabro implements `store::RunStore` and
`store::RunLogs` over its database and installs it with `Runtime::store`.
Petri's coordinator, every execution's engine log and the sandbox resource
log then live in Fabro's tables; resume, inspect and replay read them back
through the same handle; there is one source of truth. What Fabro's backend
must do:

- `open` takes the run's exclusive writer lease for the coordinator's
  `OwnerId`, idempotently for the same owner (a retry after a lost reply
  gets the same lease), and refuses another live owner with `Leased`. The
  lease ends when the handle is dropped, when Fabro's own liveness signal
  says the owner is gone (the server observes its worker's exit), or when an
  operator releases it; never by timeout. A handle whose lease moved gets
  `StaleOwner` on its next append.
- `append` returns once the records are durable, and `(log, seq)` is
  unique: the same record again is accepted without a second append, a
  different record at a taken seq is `Conflict`. A backend that keeps a
  derived view (`run_events`, `runs`) runs `execution::events::Projection`
  at ingest on its side of the seam and commits the record and its rows
  together, or the record first with the view's consumed positions beside
  its rows.
- `read` hands back every record of one log in seq order, unchanged as a
  JSON value; blobs come back byte-exact by digest.
- `testkit::run_store::conformance` is the contract; it runs against
  `RunDirStore` and `MemoryRunStore` in Petri and against Fabro's backend in
  Fabro.

What stays on the filesystem under either shape: sandbox workspaces, step
output under `logs/`, and artifacts. Large values already go through
`OutputStore`.

## Lifecycle acknowledgements

Every extension point is awaited at Petri's durability boundary, so a host's
acknowledgement gates the next step of the run:

- `before_attempt` runs before an attempt is dispatched; its decision and
  notes are recorded (`admission.decided`, a note in `step.progress.recorded`) before the attempt
  starts. Holding it pauses admission (the control service's pause is built
  on it).
- `prepare_result` runs after an attempt returned and before its record is
  appended, once per attempt. It is handed the effective outcome: the
  stage's failure policy, exhaustion included, has already run, so the
  final attempt (`will_retry == false`) is the completion to prepare, and
  routing follows the record it produces. An adjustment keeps the original
  evidence beside the effective record (a `result_prepared` note).
- `after_record` runs after the final outcome is recorded and before routing
  is resolved; its notes precede the routing record.
- `transition` runs after routes are selected and before they are recorded
  and applied; an override replaces the edge, a best-effort problem is
  recorded and the run continues, a `TransitionError` blocks every route
  (Fabro's fatal Git commit failure and best-effort metadata writes model
  onto these two).
- `run_finished` runs at the run's terminal exit before any environment is
  released; `scope_released` runs before each scope's own environment is
  released. Fabro's `run_complete`/`run_failed` (by final status, neither on a
  cancelled run) and `sandbox_cleanup` map onto them.
- `scope_acquired` runs once per acquisition of a scope's environment, after
  the executor acquired it and before the first attempt in it, with the
  workspace id and the `ExecEnv` the steps receive: where a host restores a
  workspace onto its snapshot in a Docker or Daytona sandbox before work
  resumes in it, and where it keeps the environment it later runs `git` in
  for its checkpoints. An error fails the scope's firings routably. With
  `SandboxOptions::lost_sandbox = LostSandbox::Replace`, a lease whose
  sandbox is gone gets a fresh, empty one for the host to restore.
- `RunEventSink::deliver` is awaited per event behind a bounded queue
  (`ProjectorOptions::capacity`, 1024 by default); a slow sink delays
  delivery and never slows the run, an event that finds the queue full is
  counted as `overflowed` and left to the log, a failing sink stops the pump,
  and a `deliver` that outlasts `ProjectorOptions::stall_timeout` (30 seconds
  by default) is dropped and named in the receipt's `failure`, so shutdown is
  bounded. The `ProjectionReceipt` counts every undelivered event; recovery
  is `replay_run`, deduplicated by `EventId`.
- A step's progress is queued or acknowledged. `StepCtx::logs.send` orders
  the event ahead of the attempt's outcome (the driver's completion fence
  drains the queue before it records the outcome) without making it durable
  on its own; `send_acked` returns only once the record is appended and every
  observer's durable storage confirmed it (`EventObserver::durable`). The
  native agent backend records every Pebble event acknowledged, so Pebble's
  acknowledgement means the event is in Petri's log and a store that cannot
  write stops the prompt. The at-least-once limit is the attempt: one whose
  finish never landed is re-dispatched on resume and emits its events again.

## Compatibility versions

| Version | Where | Rule |
|---|---|---|
| `EVENT_CONTRACT_VERSION` (5) | `execution::events` | additive within a version; a host checks it before projecting. Version 5 carries a partial success's whole underlying failure (`{"failure": {...}}` or `"timed_out"`). Version 3 names every event after its record, carries the stored line under `record` and the derived values under `derived`; the version 2 presentation names are gone. Version 4 adds the scope records (`scope.acquired`, `scope.failed`, `scope.released`) and, additively, `forked_from` on `run.started` |
| `INSPECT_FORMAT_VERSION` (3) | `execution::inspect` | the `petri inspect` document's field contract; version 3 reads the run through its store (`locator`, `run_key`; no log `path` or `torn`) and, additively, reports `forked_from` |
| the run format (8) on the run declaration, and the coordinator record version (`{seq, origin, recorded_at, body}` lines, `body` tagged by `event` with `<subject>.<verb>` names; the declaration carries the run `key`; version 6 adds `scope.released` and pins engine log v11; version 7 lets the declaration carry `forked_from`; version 8 pins engine log v12) | `execution::store` | a run written by a newer or older format is refused, never migrated; the check reads the first stored record before any other is decoded |
| the engine log version (v12: `{seq, origin, recorded_at, body}` records, `body` tagged by `event` with `<subject>.<verb>` names; v11 adds `scope.acquired` and `scope.failed`; v12 keeps a partial success's whole underlying failure), pinned by the run format | `engine::log` | a log whose version the runner does not speak is refused; replay must reproduce the log byte for byte or inspection reports corruption |
| `inspect_format_version`, `event_contract_version` | in the documents themselves | |
| Library pins (Pebble, lithos-llm, sandbox-driver, twins, the Fabro reference, the runner image) | `Cargo.lock`, `crates/fabro/corpus-pin.txt`, `RUNNER_PIN`; `CONTRACT.md` "Pinned revisions" names each home | evidence records cite the locked commits |

**Strict rejection of incompatible logs.** Petri never migrates a stored
run: a run whose format version, coordinator record version, or engine log
version the runner does not speak is refused at resume and by `petri
inspect` (exit 2), and a log that does not replay to itself is reported as
corrupt. A run directory written before the run key existed (format 4 and
earlier) has no key in `run.json` and is refused as not a run. Fabro chooses which runner version serves which run and decides any
migration or old-runner retention policy; this work implements neither
automatic log migration nor a fleet of versioned runners.

## What stays Fabro's

Git-backed workspace restoration and checkpoints, run and meta branches,
pull requests and publication (including its deduplication), the database
and its transaction recovery, platform event migration, the UI and API,
notifications and Slack interviews, the vault, the environment and MCP
server catalogs (the host hands Petri what a bundle may name: the
environments as `[environments.<id>]` tables of the settings layer,
`Fabro::with_settings_toml`, the environment its run selected as the
`petri.launch_environment` compile variable, the goal its run resolved as
the `petri.launch_goal` compile variable, and the MCP catalog as
`Fabro::with_mcp_catalog_toml`; `crates/fabro/FORMAT.md`, "The files"),
minted GitHub tokens, image builds from `image.dockerfile`, and the choice of
which runner version serves a run. Petri keeps local replay
(`petri inspect`, `replay_run`, resume from the run's store) and local
sandbox recovery (the plugin's lease, fence and reacquire on resume)
tested; those tests are listed in `CONTRACT.md` under "Local replay and
sandbox recovery".

## The integration checklist

In order. Each step has a Petri-side test a Fabro adapter can be checked
against.

1. **Implement the Petri interfaces in Fabro.** `ExecutionHooks` (the
   checkpoint, metadata and transition adapters), `HookService` if Fabro
   serves hooks itself (else keep `LocalHooks`), `Interviewer` over Fabro's
   questions API, `RunEventSink`, `SecretProvider` over the vault,
   `OutputStore` over platform storage, `CredentialProvider` for the model
   client, `FabroHome`, and, for shape C, `RunStore` and `RunLogs` over the
   database (in process first, or the worker's HTTP client), checked with
   `testkit::run_store::conformance`. The shape to copy is
   `crates/petri/lib/tests/embedding_readiness.rs`.
2. **Map stage and branch identities and the public events.** Project
   `RunEvent`s onto Fabro's event families with the matrix in `EVENTS.md`;
   key stages on `subject.node.meta`, branches on `BranchRole` and
   `meta.branch_role`, agent facts on the Pebble envelope's identities.
   Validate the adapter against the projection tests named in the matrix;
   Fabro's platform event schema stays in Fabro.
3. **Connect platform hooks and storage.** Fabro's `run_start`,
   `sandbox_ready`, `stage_*`, `edge_selected`, `parallel_*`, `run_*` and
   `sandbox_cleanup` hooks already run through `LocalHooks`; platform-side
   effects (checkpoint commits, database writes) go into `transition` and
   `prepare_result`, with a fatal failure as `TransitionError` and a
   best-effort one as a recorded problem. Large values and artifacts go
   through `OutputStore`. Under shape C, Petri's records and Fabro's derived
   rows commit in one transaction, or the record first.
4. **Restore workspaces and interactions before dispatching resumed work.**
   Petri resumes from the run's store (`Runtime::store` with the run's key
   in `RunOptions::run_key`, or the run directory) and reacquires held
   sandboxes, reconciling every lease with the provider by label before any
   create; Fabro restores what it owns (a Git-backed workspace, pending
   questions in its UI) from the identities above, then resumes. Known limits: a retained
   thread is not durable across resume (the node starts a fresh session with
   Fabro's discarded-session rule), a pause does not survive resume (a
   resumed run starts unpaused), a model request in flight at the crash may
   be sent again, and an external effect is at least once.
5. **Choose compatible runners.** Pin the Petri runner per run; check
   `EVENT_CONTRACT_VERSION`, `INSPECT_FORMAT_VERSION` and the store versions
   before resuming; keep an old runner for old runs as long as Fabro's
   retention policy requires.
6. **Roll out new runs.** Start new runs on the Petri runner with the
   readiness suites as the acceptance gate (`mise run test:fabro:blackbox`,
   `mise run test:fabro:differential`), then widen. The ACP backend is
   covered by the `acp` scenario family through a scripted agent on the host
   and in a container, and by the protocol suites against Petri's scripted
   agent (`crates/attractor/steps/tests/acp.rs`, the hook mapping in
   `crates/fabro/frontend/tests/hooks.rs`); the real products (Claude Code
   through `claude-code-acp`, Gemini CLI through `gemini --acp`) have their
   own live gate, `crates/petri/lib/tests/acp_products.rs`, which runs with
   `--ignored`, the product on `PATH` and its credential set
   (`crates/attractor/FORMAT.md`, "ACP products"). Daytona and crash-resume
   across runner versions have their own gates and are not part of the
   initial readiness claim.
