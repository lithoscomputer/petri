# Embedding Petri

## The integration handoff

`crates/fabro/HANDOFF.md` is what a host that embeds Petri to run Fabro
workflows relies on: the optional interfaces (`ExecutionHooks`,
`HookService`, `Interviewer`, `RunEventSink`, `SecretProvider`,
`OutputStore`, the `PebbleClient`, `HostTools` and `FabroHome`
capabilities), the
identities and event positions, the lifecycle acknowledgements, the
compatibility versions and the strict rejection of incompatible logs, what
stays Fabro's, and the integration checklist. The event contract's coverage
matrix is `crates/core/execution/EVENTS.md`; the compatibility audit of every
required row is in `crates/fabro/acceptance/CONTRACT.md`.

## The embedding path

A host that embeds Petri (a platform running Fabro workflows, say) uses the
same `Runtime` the CLI does. It checks a workflow with `Runtime::check` from
a file on disk, or with `Runtime::check_source` from text it holds in memory
beside a `frontend::FileSource` for the settings files and `@file`
references (`frontend::MapFiles` is a map of paths to text), and adds the following
interfaces, all Petri-owned types with no platform vocabulary in them:

- **Events.** `execution::events` is the versioned public event contract
  (`crates/core/execution/EVENTS.md`). An `EventProjector` is an
  `ExecutionObserver` that derives `RunEvent`s from every record and hands
  them to the host's `RunEventSink`, awaited per event through a bounded
  queue (`ProjectorOptions`: 1024 events and a 30 second stall budget by
  default), so a slow store delays delivery but never slows the run or
  grows memory; what finds the queue full, or follows a failed or stalled
  sink, is counted in the `ProjectionReceipt` and stays in the durable log.
  `replay_run` rebuilds the same events, with the same identities, from a
  run's store after the fact (`replay_run_dir` over a run directory),
  `replay_since` hands back the events past held positions, and
  `EventProjector::primed` attaches at resume. Every event names its run, invocation, execution,
  node (with the frontend's `meta`), firing, visit, attempt and branch role.
  One vocabulary for records and events: every event is named after its
  record and carries the stored log line unchanged under `record`, with what
  Petri derived beside it under `derived`; `verify_export` proves the
  exported records are the stored logs at the end of every run beside
  `verify_replay`.
- **Admission passes.** `Runtime::admission` registers an `AdmissionPass`
  that `Runtime::check` runs over every graph the step registry accepted,
  the root and the pre-lowered children, with the runtime's static
  capabilities: how a component resolves what the frontend cannot and
  writes it into the graph a run persists. A problem is an error diagnostic
  on the node's `meta.span`; a changed child's digest is followed through
  the parent's reference. The Attractor component's pass pins every LLM
  node's model against the host's catalog (`crates/attractor/FORMAT.md`,
  "Model resolution at admission").
- **Awaited extension points.** `Runtime::hooks` installs
  `driver::lifecycle::ExecutionHooks`: `before_attempt` (pause, skip or block
  an attempt), `prepare_result` (adjust the effective result; the original is
  recorded beside it), `after_record`, and `transition` (override the selected
  route, report best-effort problems, or fail advancement). Callbacks see an
  immutable `FiringView` and change the run only through their return values.
  Without hooks the driver's fast path is unchanged.
- **Hook service.** `execution::hooks::HookService` is the one interface a
  hook executor implements (`crates/core/execution/HOOKS.md`); the
  `HookAdapter` is its one caller from workflow points, so the local hook
  executor and a platform's hook service both run each configured hook once.
  `attractor_steps::hooks::LocalHooks` is the local one: `attractor_steps::register`
  installs it unless the host installed a service first, and it serves the
  per-firing points, the native agent's tool boundary, the ACP client's
  permission requests and the run-level events from one place
  (`crates/attractor/FORMAT.md`, "Hooks").
- **Host tools.** `attractor_steps::host_tools::HostTools` is a capability
  holding builders of Pebble `RegisteredTool`s. Every native agent session
  calls them once with a `HostToolContext` (run key, invocation, execution,
  node, firing, attempt) and registers the tools beside Pebble's own, so a
  host tool runs under the same tool hooks, is recorded on the same event
  stream under the stage's identity, and reaches a sub-agent through
  Pebble's own rule when marked `allow_in_subagents`. The standalone runner
  registers none.
- **Questions.** `execution::Interviewer` and the `InterviewDispatcher`, as
  on the terminal path.
- **Forks.** `execution::host::fork_from` seeds a new run from a stored
  run's records up to a position (an execution and a firing of the root
  invocation): the same graphs, the position execution's log cut after the
  firing's routing, the finished children the kept firings called, and a
  run declaration whose `forked_from` names the source. No sandbox lease
  is carried over; the host continues the new run with
  `host::resume_configured`, restoring whatever it wants in the fresh
  workspace from `scope_acquired`. `ForkOptions::rerun_last` runs the
  position's firing again instead of keeping its finish. The design is
  `crates/core/execution/FORK.md`.

`crates/petri/lib/tests/embedding.rs` is the worked example: a Fabro workflow
run without adapters, then with fake adapters that exercise every point, with
the timeline, attempts, branches, question, outcomes and accounting
reconstructed from public events alone.

## Local run layout

The standalone host stores one root run as `Run → Invocation → Execution → Firing`
through the store seam (`crates/core/store`): every durable thing is a log of
records or a content-addressed blob, and a backend stores what it is given. The
run directory is the `RunDirStore` layout of that store. `coordinator.jsonl`
records graph registrations, invocation calls, execution successors, and final
results; `resources.jsonl` records every sandbox lease transition; each execution
keeps an independent engine log. `petri inspect` reads this layout back.

```text
<run-dir>/
  run.json                 the run's key, and the file the writer lease locks
  coordinator.jsonl
  resources.jsonl
  graphs/<sha256>.json
  executions/<execution-id>/events.jsonl
```

A line is the record's JSON value, `{seq, origin, recorded_at, body}`, the same
value a public event carries under `record`; there is no header line, and the
engine log version is pinned by the run format version on the run declaration.
A restart creates a successor execution in the same invocation. It starts at the
selected target with empty context and carried firing budgets. Nested workflow calls
create invocations, not separately managed runs. The coordinator holds the run's
exclusive writer lease (a lock on `run.json`) while it creates or resumes the run;
a reader takes no lease. A host with a store of its own installs it with
`Runtime::store` and keeps the run directory for what is a file by nature
(workspaces, step output). `petri resume --run-dir` continues the run from this
layout alone; `RunPaused` and `RunUnpaused` records in `coordinator.jsonl` make a
pause durable across it.
