# The public event contract

`execution::events` is the versioned event stream an embedding host projects a
run from. This file is the contract for `EVENT_CONTRACT_VERSION` 5. The Rust
types in `crates/core/execution/src/events.rs` are authoritative for field
detail; this file states the guarantees.

Version 5 (2026-09-26) changes one shape inside a record: a partial
success's `underlying` (in a `step.finished` record's `outcome.status`) is
now the whole failure it was converted from, `{"failure": {"message": ...,
"class": ...}}` or `"timed_out"`, where version 4 carried a bare failure
info and lost a timeout. The engine log (v12) and the run format (8) that
carry it move with it: a run written before them is refused, never
migrated.

Version 4 (2026-09-18) adds the scope records: `scope.acquired` and
`scope.failed` in an execution's log, where a scope's environment ran (the
provider, the sandbox, its image and working directory, the workspace and
lease, the acquisition time) or why it could not be acquired, and
`scope.released` in the coordinator log, a lease's sandbox released by
retention. Before version 4 the facts were terminal-only progress lines. The
records are additive, but the engine log (v11) and the run format (6) that
carry them are not: a run written before them holds no scope records, is
refused by this build, and is never migrated.

Added within version 4 on 2026-09-18: the ACP backend records its envelope
under `kind = "acp"` with the same `event` identities as the native
backend's (`session_id`, `seq`, `tool_call_id`), one per `session/update`
notification and per permission exchange; before this the ACP step
recorded a bare `{ "acp": update }` payload with no `kind`, which no host
read. The stage's `acp.usage` and `acp.context` metrics are additive.

Added within version 4 on 2026-09-18: `run.started` carries `forked_from`
on a forked run (`FORK.md`). The field is additive to the stream; the run
format that carries it moves to 7, and a version 6 run is refused. A forked
run's stream is the copied prefix of its source's records, with their
original recording times and their identities renumbered only in the
coordinator log, followed by the fork's own records; `run.started` is the
first event either way, so a host knows it is reading a fork before any
copied record.

Added within version 4 on 2026-09-21: a dry run (`petri run --dry-run`, or
a Fabro launch with `mode = "dry_run"`) acquires every scope on the
`simulated` provider, an in-process provider on which nothing runs and
nothing is stored, whatever backend the workflow's environment selects. Its
`scope.acquired` records carry `provider: simulated`, a `working_directory`
of `/simulated/<workspace>` and no image or snapshot; its `scope.released`
records are always `retained: false`. The record shapes are unchanged, and
no plugin is launched for a dry run.

Renamed within version 3 on 2026-09-14, with no host consuming the stream
yet: the step kinds the Attractor frontend lowers to (`fabro/agent`,
`fabro/command`, `fabro/stage`, ... are now `attractor/agent`,
`attractor/command`, `attractor/stage`, ...), the `step.progress.recorded`
kinds those steps emit (`fabro.prompt`, `fabro.thread`, `fabro.hook`,
`fabro.skills`, `fabro.compaction`, `fabro.fallback.plan`,
`fabro.mcp.unavailable`, `fabro.parallel.*`, `fabro.checkout` are now
`attractor.*`), the graph parameters `fabro_hooks` and `fabro_workflow`
(now `attractor.hooks` and `attractor.workflow`), and the diagnostic codes
the language raises (`fabro.*` is now `attractor.*`). What the Fabro
frontend itself writes keeps its name: `fabro.launch`, `fabro.environment`,
the `fabro.settings_toml` compile variable, and the `fabro.workflow_toml`,
`fabro.hooks.*`, `fabro.mcps.*` and `fabro.model_fallbacks` codes. A run
directory written before the rename names the old kinds and cannot be
resumed or inspected by this build; no alias is kept
(`.ai/plans/attractor-split.md`).

## The rule

One vocabulary for records and events: **a public event carries its record,
unchanged, plus what Petri derived beside it.**

1. **Name.** A record's own event is named after the record: the `body.event`
   tag of the stored line, `<subject>.<verb>` in lower case (`step.finished`,
   `routing.resolved`, `execution.declared`). Events derived from the state
   alone (the view events) follow the same spelling and are marked `derived`.
2. **Record.** The event carries the stored line as one value, `record`:
   exactly what the log holds (`seq`, `origin`, `recorded_at`, and `body`,
   the engine's or the coordinator's event with its `event` tag and fields).
   Nothing is renamed, re-nested, dropped or lifted out of it, and the same
   Rust types and serializers produce the log line and the event.
3. **Derived values.** What Petri adds lives in a separate `derived` value,
   never inside `record`: `final` and `exhausted` for `step.finished`, the
   resolved target node per group for `routing.resolved`, the target with
   `transition` and `back` for `route.applied`, each clone's entry node for
   `node.expanded`, `deliverable` and the decoded `answer` for
   `control.requested`, `parsed` for `step.progress.recorded` and
   `run.note.recorded`. A derived value can never collide with a recorded
   key, and a recorded structure is never modified.
4. **Envelope.** Every event carries `id` (the log, the record's `seq`, and
   `index`), `origin`, `recorded_at`, `observed_at`, `context` (`invocation`,
   `execution`, `parent`) and `subject`. On a record's own event, `origin`,
   `seq` and `recorded_at` repeat what `record` holds, so a consumer reads the
   envelope alone or the record alone.
5. **Origin.** Copied from the record: `external` for a record the host fed
   the engine or a coordinator record, `core` for a record the engine produced
   while draining. `derived` marks a view event, which has no record. The kind
   of a record does not decide its origin: `apply` marks whatever it is fed as
   external, so a host may feed a token or an expansion, and such a record is
   external. Replay consumes external records, regenerates core records, and
   recomputes view events.
6. **Index.** A record's own event is `index` 0 and carries `record`. View
   events attached to the same record follow at `index` 1 and up, carry no
   `record`, and are marked `derived`.
7. **Wire form.** A record serializes as an internally tagged object under
   `body` inside `{"seq", "origin", "recorded_at", "body"}`, which is the line
   the log stores. A public event is `{ id, origin, recorded_at, observed_at,
   context, subject, record, derived }`. Export reads `record`; there is no
   field-removal list, no reverse mapping, and no reconstruction from
   presentation data.

A complete `execution.declared` event, a coordinator record with nothing
derived:

```json
{
  "id": { "log": "coordinator", "seq": 3, "index": 0 },
  "origin": "external",
  "recorded_at": 1789323217366,
  "context": { "invocation": 0, "execution": 1 },
  "record": {
    "seq": 3,
    "origin": "external",
    "recorded_at": 1789323217366,
    "body": {
      "event": "execution.declared",
      "execution": 1,
      "invocation": 0,
      "predecessor": null,
      "start": { "entry": "graph_entries", "context": {}, "prior_firings": {}, "execution_index": 1, "max_executions": 4 },
      "middleware_state": {}
    }
  }
}
```

A `route.applied` edge, a core record whose recorded `edge` is an edge id;
the node reference Petri resolved, the edge's transition and whether it is a
back edge sit in `derived`:

```json
{
  "id": { "log": "execution", "execution": 1, "seq": 23, "index": 0 },
  "origin": "core",
  "recorded_at": 1789323217459,
  "context": { "invocation": 0, "execution": 1 },
  "subject": { "node": { "id": 4, "name": "review", "kind": "attractor/agent", "meta": { "kind": "agent" } }, "firing": 3, "visit": 1, "attempt": 1, "generation": 0, "branch": { "role": "none" } },
  "record": {
    "seq": 23,
    "origin": "core",
    "recorded_at": 1789323217459,
    "body": { "event": "route.applied", "kind": "edge", "firing": 3, "group": 0, "edge": 9 }
  },
  "derived": { "target": { "id": 7, "name": "finalize", "kind": "attractor/command", "meta": { "kind": "command" } }, "transition": "Continue", "back": false }
}
```

`record` equals the stored line as a JSON value; whitespace and object-key
order are not part of that equality. A view event, such as the
`visit.started` that follows a token, has the same envelope with
`origin: derived`, no `record`, and the view event under `derived`, tagged
by `event`.

## Sources and identity

Every `RunEvent` is derived from one durable record: a coordinator record
(the run's coordinator log) or an engine record (one execution's engine log)
with the post-apply engine state beside it. The derivation is the same live
(the `EventProjector` observer) and over a run's store (`replay_run` over a
`store::RunLogs` handle; `replay_run_dir` over a run directory). Every
record carries the time it was appended (`recorded_at`, milliseconds since the
Unix epoch), read at the recording boundary — the driver's append for an
engine record, the coordinator store's for a coordinator record, never inside
the state machine — and persisted beside the record, so a replayed event
carries the same time the live one did.

`EventId { log, seq, index }` is the stable identity: the log the record came
from (`coordinator`, or `execution` with the execution id), the record's `seq`
in that log, and the ordinal of this event among the events one record
produced (`0` is the record's own event). Within one log the order is total.
Across logs, `context` (`invocation`, `execution`, and the `parent` link, the
firing that called a nested invocation) ties an execution's events to the
invocation that declared it.

`origin` is the record's: `external` for a coordinator record or an engine
record the host fed the engine (an execution's start, an admission, a step's
start, progress and result, a routing decision, an elapsed retry, a host's
cancel, kill or control), `core` for a record the engine produced while
draining (a routed token, an applied route, a splice, a cascading cancel),
`derived` for a view event. Replay consumes the external records and
regenerates the core ones.

`subject` names the node (`NodeRef`: id, instance name, step kind, the
frontend's `meta` verbatim) and, when the event is about a firing, the firing
id, the visit ordinal (which firing of the node within the execution, 1-based),
the attempt (1-based within the firing), the generation, and the node's
`BranchRole` (`none`, `fork`, `member` of a fork's branch, or `join`).

Visits and attempts are distinct: a retry advances `attempt` and keeps the
firing and visit; a loop that fires the node again starts a new firing and
advances `visit`.

A fork's branch events (`fork.started`, `branch.completed`, `fork.completed`)
carry a `ForkOccurrence`: the parent execution, the fork node, the fork's
firing, its visit and its generation. It is the one reference for one
occurrence of a fork. A host keys a repeated visit of one fork, a fork inside
a branch (its own occurrence, in the branch's child execution), or two
branches with the same target on it, never on the most recent fork it saw.
The static `BranchRef {fork, index}` beside it names the branch within the
fork's shape.

## Source metadata

`subject.node.meta` is the frontend's node metadata. The Fabro frontend sets
`label`, `shape`, `kind` (`start`, `exit`, `command`, `agent`, `human`,
`parallel`, `parallel.branch`, `parallel.fan_in`, `wait`, `stack.manager_loop`,
`goal_check`, ...), `classes`, `span`, and `synthetic: true` on nodes it
invented. A `parallel.branch` node is the parent-side delegate of one branch;
its `meta.branch = { fork, target, index }` names the branch, and the branch's
stage itself runs in the child invocation the delegate starts (the child's
entry node carries `meta.branch_role = { fork, index }`, which gives it the
member role in its own graph unless it is itself a fork: a nested parallel
node keeps its fork role there, and its branches project as a fork occurrence
of their own). A synthetic `<fork>.fan_in` is a `parallel.fan_in` with
`synthetic: true`. A host
distinguishes logical stages from lowering artifacts with these fields and
with `BranchRole`, never with node names.

Two more facts a stage view needs ride on `meta` so the records carry them
without a graph lookup: a command node's `script`, the text the step runs,
and `edges`, the node's routing arms keyed by edge id, each `{ to, label,
condition }` with the target node name, the edge's label and its
`condition` as written (absent on an unconditional edge). The edge a
`route.applied` record names (`body.edge`) and the `emit` decision of a
`routing.resolved` group are keys into the subject node's `meta.edges`, so
the condition that matched is `subject.node.meta.edges[edge].condition` on
the same event (`crates/attractor/FORMAT.md`, "Nodes").

## Records

Every stored record is one event, `index` 0, with the record under `record`.
The identifier is the `body.event` tag, which is also the tag in the stored
line. Rust field types are as the engine and coordinator declare them.

Coordinator records, one coordinator log per run (`coordinator.jsonl` in a
run directory), origin always external. Log identity
`{ "log": "coordinator" }`.

| `event` | Body type | Fields and derived values |
| --- | --- | --- |
| `run.started` | `CoordinatorEvent::RunStarted` | `format_version`, `key`, `root`, `middleware_chain`, and `forked_from` on a forked run (`source`, the source run's key; `execution` and `firing`, the position its records were kept up to; `rerun_last`). Nothing derived |
| `graph.registered` | `GraphRegistered` | `digest`, before any invocation declares the graph. Nothing derived |
| `invocation.declared` | `InvocationDeclared` | `invocation`, `call` (absent on the root), `graph`, `context`, the name-only `secret_bindings`, the `sandbox` binding, the `admission` gate. Nothing derived; `context.parent` is the call as a `ParentLink` |
| `execution.declared` | `ExecutionDeclared` | `execution`, `invocation`, `predecessor`, the whole engine `start` (entry, context, inherited firing counts, index, restart limit), `middleware_state`. Nothing derived |
| `execution.finished` | `ExecutionFinished` | `execution`, `exit` (`terminal {status}` or `restart {edge, target, source}`). Nothing derived |
| `invocation.finished` | `InvocationFinished` | `invocation`, `result` (the `InvocationResult`). Nothing derived |
| `invocation.cancel.requested` | `InvocationCancelRequested` | `invocation`, `reason` when the requester gave one (`stall_timeout`, `interrupt`, `control`). A `run.stalled` view event follows a stall watchdog's request |
| `run.paused`, `run.unpaused` | `RunPaused`, `RunUnpaused` | the control service held or released admission. Replay carries them, and a resume whose last recorded control is a pause starts with admission held |
| `run.note.recorded` | `RunNoteRecorded` | `execution` (the one whose driver ran the point, when known), `kind`, `payload`: a note from a run-level hook point (`run_finished`, `scope_released`), appended from the execution's report before `run.finished`. Derived: `parsed`, the same reading a firing's note gets |
| `scope.released` | `ScopeReleased` | `invocation` (the lease's owner), `lease`, `scope` (the coordinator's stable `ScopeIdentity`: `{"declared": <scope>}` or `{"spliced": [..]}`), `workspace`, `provider`, `instance` (the provider's id for the sandbox; absent when none was created), `outcome` (`succeeded` or `failed`, the invocation's status as retention reads it), `retained` (whether the sandbox and its workspace still exist on the provider afterwards: stopped and kept, or deleted; a dry run's `simulated` lease is always released deleted, there being nothing to keep), `problems` (what the release could not do; the sandbox is then still there and the next release, `finish` or `petri sandbox prune`, tries again). Appended once per lease the finished invocation owned, after the executor released it, before `run.finished`; a lease a crash left live is released by the resumed run's end and recorded after `run.finished`. An inherited invocation owns no lease and records none. `context.invocation` is the owner. Nothing derived |
| `run.finished` | `RunFinished` | `status`. Nothing derived |

Engine records, one engine log per execution
(`executions/<execution>/events.jsonl` in a run directory). Log identity
`{ "log": "execution", "execution": <id> }`. Each is attributed to a subject
where one exists.

| `event` | Usual origin | Body type | Fields and derived values |
| --- | --- | --- | --- |
| `execution.started` | external | `Event::ExecutionStarted` | the engine start, flattened: `entry`, `context`, `prior_firings`, `execution_index`, `max_executions`. Nothing derived |
| `admission.decided` | external | `AdmissionDecided` | `decision_id` (`execution_start`, or `attempt_start {firing, attempt}`, which gives the event its subject), `decision` (`admit`, `skip {outcome}`, `block {reason}`), `trace` (the middleware keys). Nothing derived |
| `step.started` | external | `StepStarted` | `firing`, `attempt`: the attempt was dispatched to its step. Nothing derived; `wait.state.changed {running}` follows |
| `step.progress.recorded` | external | `StepProgressRecorded` | `firing`, `ev`: a log line (`log {stream, line}`), an artifact (`artifact {name, uri}`) or a step-defined payload (`custom`). Derived: `parsed`, when the payload is a protocol Petri owns (below) |
| `step.finished` | external | `StepFinished` | `firing`, `attempt`, `outcome` (status, output, `metrics`, context updates, splices). Derived: `final` (whether the engine recorded it as the firing's outcome; a non-final attempt is followed by a retry) and `exhausted` (the retry policy allowed no further attempt while the status was retryable). `retry.scheduled` and `wait.state.changed {awaiting_retry}` follow a non-final attempt |
| `routing.resolved` | external | `RoutingResolved` | `decision_id` (`route {firing, attempt}`), `groups`: per group the decision (`emit`, `none`, `block {reason}`), the interventions (`override`, `block`, outermost first) and the weighted `draw` (tier, candidates, roll, total) when one happened. Derived: `groups[].target`, the node each decision leads to |
| `retry.elapsed` | external | `RetryElapsed` | `firing`, `next_attempt`: the driver waited out the backoff. Nothing derived |
| `cancel.requested` | external, or core for a cascade | `CancelRequested` | `target`: `{scope}` (the run's root scope cancels everything) or `{group}` (a declared node group). Nothing derived; `wait.state.changed {cancelling}` follows for each live firing it reaches |
| `kill.requested` | external | `KillRequested` | `scope`: the forced tier. Nothing derived; `wait.state.changed {cancelling}` follows |
| `control.requested` | external | `ControlRequested` | `firing`, `ctl` (`cancel`, `kill`, `deliver` with the value as delivered; a host may send an answer under `$answer`, a steer under `$steer`, an interrupt under `$interrupt` (`steps::Interrupt`: stop the agent stage's current model turn and keep its session; its optional `steer` is the stage's next input)). Derived: `deliverable` (whether the firing could receive it; a late answer is recorded but not deliverable) and `answer`, the decoding of a delivered value that reads as one. `wait.state.changed {running}` follows a deliverable answer |
| `token.emitted` | usually core | `TokenEmitted` | the token, flattened: `edge`, `generation`, `payload`, `from`. Nothing derived; the subject is the emitting firing when it has one |
| `route.applied` | usually core | `RouteApplied` | `kind`: `edge {firing, group, edge}` or `none {firing, group}`. Derived for an edge: the `target` node, the edge's `transition` and `back`; nothing for `none`. `fork.started` follows the first applied route of a fork node |
| `node.expanded` | usually core | `NodeExpanded` | `node`, `splice` (cancel scope, source, region, generation, payload, the clones with their nodes, `max_parallel`, `fail_fast`). Derived: `clones[].entry`, each clone's entry node. `fork.started` follows for the node that fanned out into the template |
| `scope.acquired` | external | `ScopeAcquired` | `scope`, `lease` (when a coordinator allocated one), `workspace`, the sandbox the executor acquired flattened beside them (`ir::SandboxInstance`: `provider` — `host`, `docker`, `daytona`, a plugin's kind, or `simulated` for a dry run, whose scopes are acquired on no provider at all —, `instance`, the provider's id for the sandbox, `image` and `snapshot` when the provider knows them, `working_directory`, the directory the scope's steps run in as the sandbox sees it; a simulated sandbox's is `/simulated/<workspace>`, a name that exists on no machine), `duration_ms`, the acquisition's wall-clock time. Recorded by the driver once per acquisition, before any `step.started` in the scope; a resumed execution and an inherited sandbox's execution each record their own, naming the same lease and sandbox. No subject: a scope is not a node. Nothing derived |
| `scope.failed` | external | `ScopeFailed` | `scope`, `lease`, `workspace`, `provider` (the kind the lease was reserved on, when one was), `error` (the executor's message), `causes` (its source chain, outermost first), `duration_ms`. The run does not abort: every firing in the scope then fails, routably, with the failure class `acquire` and the message `could not acquire the environment: <error>: <causes...>`. No subject. Nothing derived |

A host may feed a `token.emitted` or a `node.expanded` itself; such a record
is external, and replay consumes it.

## Derived events

The view events are the projection's incremental reading of the state. They
have no record, are marked `origin: derived`, follow the record whose apply
produced them at `index` 1 and up, and are never stored: a host that stores
only records recomputes them with `Projection`. Under `derived` they are
tagged by `event`.

| `event` | When |
| --- | --- |
| `visit.started {inputs}` | a firing exists for a node whose join was satisfied; `wait.state.changed {awaiting_admission}` follows |
| `visit.completed {outcome, executed, attempts}` | the firing's final record exists; `executed` is false for a synthesized completion (false precondition, cancelled scope, blocked or skipped admission); `attempts` is the count |
| `retry.scheduled {next_attempt, base_delay}` | a non-final attempt returned and the next waits out the backoff |
| `wait.state.changed {state}` | `awaiting_admission`, `running`, `awaiting_answer`, `awaiting_retry`, `cancelling`; a delivered answer or the question's expiry ends `awaiting_answer` |
| `fork.started {occurrence, branches}`, `branch.completed {occurrence, result}`, `fork.completed {occurrence, fork, results, disposition}` | a fork's branches start; a branch reaches its end; the fork's branches are all accounted for, in branch order, with the `disposition` that closed them (`joined`, `cancelled`, `killed`; see "Fork closure"). All three carry the `ForkOccurrence` of the fork visit; `branch.completed`'s subject is the branch's last firing with its visit, attempt and generation. A static fork's branches are its routing groups; a `for_each` expansion's branches are its clones, in item order, and the fork is the node that fanned out into the template (Fabro's `parallel` node), so both fan-outs carry the same identities |
| `run.stalled {stall_timeout_ms, idle_ms}` | the stall watchdog cancelled the run; attached to the `invocation.cancel.requested` record that carries the reason |

## `parsed`

`step.progress.recorded` carries the progress payload verbatim in its
record. `derived.parsed` is Petri's reading of the step protocols Petri
itself owns, tagged by `kind`:

| `parsed.kind` | From | Fields |
| --- | --- | --- |
| `question` | a `$question` payload | `question` (`steps::Question`: id, text, options, default, whether free text is accepted, sensitivity, the format's `kind`, a `reference` to review, `timeout_ms`, and `context`, the text a person reads beside the question; each option carries `key`, `label` and, when the asking step set them, `description` and `preview`. The optional fields are absent from the wire when unset) |
| `question_expired` | a `$question_expired` payload (`steps::QuestionExpired`) | `expired`: the question id, how long the step waited (`waited_ms`), and the option it took on its own (`default`, by key) when it had one. The attempt's outcome follows as `step.finished`; a host never infers a timeout from that outcome |
| `note` | a `$note` payload (`driver::lifecycle::Note`) | `note {kind, payload}`. Kinds the driver writes: `result_prepared` (original attempt evidence beside an adjusted result), `transition` (overrides, best-effort problems, a block), `budget_paused`, `budget_resumed`. Kinds the hook adapter writes: `hook` (a hook service report: the point, the decision, each hook's state, duration, message and `usage`: its requests, tool calls, timings and the model `usage` those requests used, a lithos-llm `Usage`), `hook.activity`. Two kinds get a reading beside the note: `hook_activity` (`{hook {point, hook}, backend, envelope}`: one event a hook's own agent produced, the hook agent's payload forwarded as recorded, apart from the stage's own agent) and `budget` (`{state: paused | resumed, attempt, remaining_ms, pending_questions}`: an executor-enforced attempt budget stopped counting because the attempt asked a question, or counts again because its last pending question was answered) |

`parsed` is absent when the payload is a log line, an artifact, or a
payload Petri does not own. A backend's own event (a `kind` naming the
backend and an `event` object: for `pebble`, Pebble's `CodingAgentEvent`
with `seq`, `stream_id`, `session_id`, `parent_session_id`, `tool_call_id`,
`timestamp`, `event`) is forwarded as recorded, and the step's own keys
beside it (the native backend records `node`, `firing`, `attempt` and
`scope`) with it. Petri reads nothing into it and adds no variant per backend
event; a host that wants Pebble's typed view asks Pebble's adapter, and an
unfamiliar payload survives without a Petri release. A native agent's
sub-agents are on the same stream: the lifecycle (`SubAgentSpawned`,
`SubAgentTurnStarted`, `SubAgentCompleted`, `SubAgentFailed`,
`SubAgentClosed`) under the parent's session, a child's own events under the
child's session with `parent_session_id` naming its immediate parent, all
attributed to the parent stage (`crates/attractor/FORMAT.md`, "Native Pebble").
A hook's agent is never on the stage's stream: it is a `hook_activity`
reading, so a consumer summing a stage's backend events never counts a
hook's model requests as the stage's.

Usage and timing: `step.finished` and `visit.completed` carry the outcome's
`metrics` (`duration_ms`, `exit_code`, `custom`). The driver fills
`duration_ms` with the observed wall-clock duration of the attempt when the step
kind did not report one. The native agent backend reports `pebble.usage`,
`pebble.inference_ms`, `pebble.tool_ms`, `pebble.compactions`,
`pebble.compaction_usage` and `pebble.subagents` (the node's agent tree:
children spawned, completed, failed and closed, and their summed usage by
session, each session with the provider and model it ran on) under
`custom`; a prompt node reports `prompt.calls` and `prompt.usage`. Every
`usage` on Petri's events and metrics, Pebble's own events included, is
lithos-llm's `Usage`: `{ tokens: { input, output, reasoning, cache_read,
cache_write }, cost: { usd_micros, source } }`, with `cost` absent when the
value is not fully priced (a sum has a cost only when every part that used
tokens was priced; `source` is `catalog`, `provider` or `application`). See
`crates/attractor/FORMAT.md`, "Usage". Before 2026-09-14 the five buckets were
the top level of `usage` and the cost a separate `cost_usd_micros` number
beside it: `pebble.cost_usd_micros`, `pebble.compaction_cost_usd_micros`
and `prompt.cost_usd_micros` under `custom`, and `cost_usd_micros` on
`attractor.compaction`, `attractor.prompt.completed`, `pebble.subagents` (and each
of its `sessions`), a hook's `usage` (whose `tokens` is now `usage`), and
Pebble's `AssistantMessage` (with `cost_source`), `CompactionCompleted`,
`CompactionFailed` and `RouteFailover`. Those fields are gone; a stored run
from before the change reads its Pebble events back with zero usage.

Times: every event carries `recorded_at`, when its record was appended to its
log, the same live and on replay. A host reconstructs start and completion
times from the events that mark them — `run.started`/`run.finished`,
`invocation.declared`/`invocation.finished`, `execution.declared`/
`execution.finished`, `visit.started`/`visit.completed`,
`step.started`/`step.finished` (a command's boundaries), a question's
`step.progress.recorded`/`control.requested` (an interview's) — and
durations from their differences, beside the step-reported `duration_ms`.
`observed_at` is when the projector saw the record live; it is absent on
replay and is never a substitute for `recorded_at`: replay time is not
execution time. A record a crash kept off disk has no event until a resume
stores it; it then carries the resume's recording time, and a host that saw
the original live keeps whichever copy it deduplicated first.

## Fork closure

Every fork that announced `fork.started` closes with exactly one
`fork.completed`, live and on replay, whatever stopped it. The projection
keeps each open fork by its `ForkOccurrence` and ties its branches and its
join to that occurrence through the generation the fork's tokens carry, so
two visits of one fork or a fork inside a branch never share a closure, and
every `branch.completed` and `fork.completed` names the occurrence its
`fork.started` announced.

| `disposition` | When | Each `branch.completed` | `results` |
| --- | --- | --- | --- |
| `joined` | every branch's token reached the join and the join fired (`visit.started` on the join follows) | the token that reached the join, with its `payload` | every branch |
| `cancelled` | the join completed without running (`visit.completed {executed: false}` with a `cancelled` outcome): its scope was cancelled, or every branch reached it cancelled | the branch's last record; no `payload`, since the join never ran | every branch with a record |
| `killed` | the fork's scope was killed, so the join's tokens were dropped and it never fires; the fork closes once no branch of it has a live firing left, on the fork's own firing as subject | the branch's last record; no `payload` | the branches with a record; one that never recorded is absent |

A branch's own terminal facts stay where they are: the member's
`visit.completed` (with `executed`, false for a branch cancelled before it was
admitted) and, for a Fabro branch, the child's `invocation.finished`. A Fabro
branch child cancelled before it was admitted still gets an execution that
records the cancellation and no `step.started`.

### The Fabro branch events

The Fabro frontend's branch delegates and fan-in add custom payloads
(`step.progress.recorded`) with dispositions of their own. This is the mapping from Petri's facts to the
pinned Fabro's `parallel.*` events; Fabro emits no group event on
cancellation, and this contract does not claim it does.

| Petri | Fabro |
| --- | --- |
| `fork.started {occurrence}` | `parallel.started`; Fabro's `parallel_group_id` (`node@visit`) is the occurrence's fork node and visit |
| `attractor.parallel.branch.started {fork, occurrence, branch, index, item_label, invocation}`, emitted once the child's engine has started (it holds a slot under the fork's gate, as Fabro's branch holds a permit). `occurrence` is `{fork, firing}`: the fork step's firing in the event's execution, the same firing the typed `fork.started` names. The child's `invocation.declared` `call.slot` is `branch:<fork>@<firing>:<index>:<target>`, so the child link names the occurrence too | `parallel.branch.started` |
| `attractor.parallel.branch.completed {fork, occurrence, branch, index, item_label, invocation, status, disposition, started, duration_ms}`, emitted on every path a branch ends. `status` is the envelope's Fabro status; `disposition` is `completed` (the child finished; `status` says how), `cancelled` (the child settled cancelled; `status` is `failed`), `killed` (the stop escalated to a kill before the child settled; `failed`) or `failed_to_start` (the child could not be declared; `failed`); `started` is whether the child's engine ever started (false for a queued branch the cancel reached first) | `parallel.branch.completed` with the same `status`, for every event with `started: true`. Fabro emits none for a branch that never held a slot, so a host projecting Fabro's stream drops `started: false`. Fabro reports a cancelled branch as `failed` with duration 0; Petri carries the observed duration |
| `attractor.parallel.completed {node, fork, occurrence, branch_count, success_count, failure_count, status}`, emitted by the fan-in when it runs, with the occurrence its inputs carried | `parallel.completed`. Neither engine emits it for a cancelled or killed fork; the terminal group fact is then Petri's `fork.completed {disposition}`, which Fabro has no event for |

## Ordering and delivery

- Per execution, events are delivered in record order, and within one record
  in `index` order. The order is causal: admission before start, start before
  finish, the final finish before `visit.completed`, `visit.completed` before
  `routing.resolved`, `routing.resolved` before `route.applied`, host notes
  before the record they annotate.
- Across executions the `parent` link and `execution.declared` `predecessor` tie
  the streams together. The coordinator log's records are delivered as they
  are appended; a fresh run's `run.started` is delivered to an observer when
  it attaches.
- `EventProjector` is bounded: the observer callback derives each record's
  events and queues them without waiting; a pump task awaits the host's
  `RunEventSink::deliver` per event, in order. The queue holds
  `ProjectorOptions::capacity` events (1024 by default), which is the most
  the projector keeps in memory: a slow sink delays delivery and the run
  keeps its pace. An event projected while the queue is full is not queued:
  the `ProjectionReceipt` counts it as `overflowed` (and `undelivered`),
  live delivery goes on with the next event that finds room, so the sink
  sees each log in record order with gaps, and the durable log keeps the
  event. A sink error stops the pump; later events are counted as
  undelivered. A `deliver` or `finish` that does not return within
  `ProjectorOptions::stall_timeout` (30 seconds by default) is dropped, the
  sink counts as failed from then on, and the receipt's `failure` names the
  event it stalled on; `EventProjector::shutdown` therefore completes within
  about one stall budget plus the drain of the queue. A sink that overflows,
  fails or stalls never fails the run. Recovery from each is `replay_run`,
  deduplicated by `EventId`.
- Across a resume, the driver delivers the regenerated suffix (the records a
  crash kept off disk) before dispatching pending work, with the same
  identities; delivery is at-least-once, deduplicated by `EventId`. A
  projector attached at resume is built with `EventProjector::primed`, which
  folds the stored prefix into its state without delivering it.
  `replay_since` is the incremental form of `replay_run`: the events past a
  set of held `EventId`s per log, for a consumer that already holds a prefix;
  its fold still runs over the whole run. A consumer that follows a live run
  through its store keeps a `RunReplay` and advances it: each advance reads
  only the records past the ones it consumed (`RunLogs::read_from`), folds
  them through the state it kept (the coordinator state, each execution's
  engine state, the projection), and hands back their events alone, per log
  the same as `replay_run` derives them. The state is a cache of the records:
  a fresh replay rebuilds it, and it holds every engine record of the run, so
  the consumer drops it when the run ends or goes idle.
- A step's progress event is queued or acknowledged. `StepCtx::logs.send`
  resolves once the event is queued: it is ordered behind the attempt's
  earlier sends and ahead of its outcome, because the driver's completion
  fence drains the queue before it records `StepFinished`, so a durable
  outcome implies its earlier events are durable. Queueing alone is not
  durability: a crash between the queue and the append loses the event.
  `send_acked` resolves only after the driver appended the record and every
  observer's durable storage confirmed it (`EventObserver::durable`; the
  run's store writer answers once the record's append returned from the
  store, past the point where a process crash can lose it), and a store's
  write failure is the sender's error. The
  native agent backend records every Pebble event acknowledged, so Pebble's
  own acknowledgement means the event is in Petri's log. What a crash still
  repeats is the attempt: an attempt whose finish never landed is
  re-dispatched on resume and emits its events again, so an acknowledged
  record can appear twice; a consumer deduplicates delivery by `EventId` and
  a backend's events by Pebble's `(stream_id, seq)`.
- Every event is derived from a durable record, output lines included, and
  carries the record's `recorded_at`, identical live and on replay. The one
  live-only field is `observed_at` (milliseconds since the
  epoch when the projector saw the record), absent on replay. A backend's live
  stream chunks that never reached the step's progress channel are not in the
  contract.

## Export

A record's own event carries the stored line, so a host that stores `record`
values stores the logs: the coordinator log whole, and each execution's
engine log whole. The record commit is the only durability boundary; view
events are recomputed from records by `Projection`, live or after the fact.

`execution::events::verify_export` is the proof, run over a run's store
(`verify_export_run_dir` over a run directory): it projects the run, takes
the `record` values out of the stream, and checks that they equal the stored
records as JSON values, that they reload through the record decoders into the
same logs, and that the stored external records replay to the stored logs,
whole for an execution the coordinator recorded as finished and as a prefix
for a crash log. The standalone host runs it at the end of every run beside
`engine::verify_replay` (`RunOptions::verify_replay`, on by default), so
every run in the test suites is an export case. The store is what a host
keeps: a host with a database of its own implements `store::RunStore` and
`store::RunLogs` over it (`crates/core/store`), Petri's coordinator writes
the same records there, and `verify_export` holds over that store too.

Crash recovery: read-only projection publishes what is stored. `replay_run`
regenerates the core records a crash kept out of the store, so its state is
right, but publishes no event attached to them. A resume writes those
records through the normal storage path, with normal recording times, before
its observers see them: the driver hands the regenerated suffix to the store
writer first and to every other observer after it, before it dispatches
pending work. Existing records keep their times. Live and replayed views
agree at the same durable positions.

## Secrets

Records are masked by the driver before they are appended, so every value is
post-mask: a secret reference stays `{"$secret": ...}`, a masked value stays
`***`. Host notes are masked the same way before they are recorded.

## Terminal output

The CLI's terminal rendering (`[node#firing] line`, status lines) is a
presentation over these events and the run's store. It is not part of the
contract; a host consumes `RunEvent`s and never parses terminal text.

## Event-coverage matrix

The final event contract for readiness item 7, completed at milestone D
(readiness item 10): every Fabro execution-related need, the public Petri
source that carries it, the identities a host keys on, whether the fact is a
durable record (in the log, delivered live and on replay) or live-only, and
the projection test that proves it from `RunEvent`s alone. The Fabro names
describe the consumer's need, not Petri event names. Every row's events carry
`recorded_at`, so the timestamps and durations Fabro's projection needs come
from the same events. A `step.progress.recorded` row
names the `kind` of the `StepEvent::Custom` payload in the record; every such payload
also carries `node`, `firing` and `attempt` beside the event's `subject`.

| Fabro need | Public source | Identities | Durability | Projection test |
| --- | --- | --- | --- | --- |
| `run.started/completed/failed`, root vs internal invocation | `run.started {root, middleware_chain}`, `run.finished {status}`, `invocation.declared` (`call` is `None` on the root, a `ParentLink` on a child), `invocation.finished {result}`, `execution_declared/finished` | `EventId`, `context.invocation`, `context.execution`, `context.parent` | durable (coordinator log) | `embedding::the_workflow_runs_without_adapters_and_the_events_reconstruct_it`, `embedding_readiness` |
| run notices, steer, interrupt, cancel reasons | `invocation.cancel.requested {reason}` (`stall_timeout`, `interrupt`, `control`), `cancel.requested`, `kill.requested`, `control.requested` (a steer is a delivered `{"$steer": ...}`; an interrupt of a live model turn is a delivered `{"$interrupt": {steer?}}`, only ever recorded for a stage with a turn in flight, since `ControlService::interrupt` refuses the rest with `NoLiveTurn`). The stage reports the stopped turn as `step.progress.recorded` kind `attractor.turn.interrupted` (`node`, `firing`, `attempt`, `backend`, `session`) on both agent backends; on the native one Pebble's own `RoundInterrupted` precedes it in the backend envelope. The session stays open and the stage's next input is the interrupt's text, else the next steer | invocation, firing, session | durable | `fabro_readiness_blackbox::…_is_cancelled_…` (`reason = interrupt`), `controls::an_idle_run_is_cancelled_by_the_watchdog` (`stall_timeout`), `controls::the_control_file_pauses_unpauses_and_steers_without_answering`, `petri::interrupt` (an interrupt with text during a tool call, a plain interrupt then a steer, the refusal on a gate, the ACP backend) |
| `stage.started/completed/failed/retrying` | `visit.started`, `admission.decided`, `step.started`, `step.finished` (`derived.final`, `derived.exhausted`), `retry.scheduled`, `retry.elapsed`, `visit.completed {executed, attempts}`; `subject.node.meta.kind` and `synthetic` map lowering nodes to the logical stage | node, firing, visit, attempt, generation | durable | `embedding::the_workflow_runs_without_adapters_…` (the retry), `embedding_readiness` (every logical stage's final status) |
| `stage.prompt`, `prompt.completed` | `step.progress.recorded` kinds `attractor.prompt`, `attractor.prompt.completed`; a prompt node's `step.finished` output | node, firing, attempt | durable | `petri-fabro-steps::prompt` (prompt events), `fabro_blackbox::a_prompt_node_makes_one_tool_free_model_call` |
| `edge.selected`, `loop.restart`, the condition that matched | `routing.resolved` (per group the decision, overrides, blocks and weighted draw in the record; the target in `derived.groups`), `route.applied` (`kind` edge or none in the record; the target, `transition` and `back` in `derived`; the edge's `condition` as written is `subject.node.meta.edges[body.edge].condition`); a restart is `execution.finished {exit: restart}` then `execution.declared {predecessor}` | firing, edge, execution | durable | `embedding::transitions_override_block_or_continue`, `controls::node_visit_totals_survive_a_restart_while_context_resets`, `petri-fabro-steps::steps::the_public_stream_carries_the_script_and_the_matched_condition` |
| `parallel.started`, branch start and completion, `parallel.completed` (static fan-out) | `fork.started {occurrence, branches}`, `branch.completed {occurrence, result}`, `fork.completed {occurrence, fork, results, disposition}` in branch order; `BranchRole` on every subject; the `fabro.parallel.*` `step.progress.recorded` events with the same occurrence and the branch dispositions ("Fork closure") | `ForkOccurrence {execution, fork, firing, visit, generation}`, `BranchRef {fork, index}` | durable | `embedding::the_workflow_runs_without_adapters_…` (`forks`, `joins`); `petri-fabro-steps::parallel` (`a_repeated_fork_publishes_results_per_visit_with_its_own_children`, `duplicate_targets_are_separate_branches_with_their_own_index`, `a_nested_fork_runs_inside_its_branch_and_reports_its_own_results`: two visits, duplicate targets and a nested fork each keyed on their own occurrence, live and replayed; `a_clean_cancel_during_work_closes_the_branches_and_the_fork`, `a_cancel_before_admission_records_a_branch_that_never_started`, `a_cancel_before_the_fan_in_keeps_the_finished_branch_result`, `a_kill_after_the_cancel_closes_the_fork_as_killed`: every closure live and through `replay_run`) |
| the same for a `for_each` fan-out (an expansion) | the same three bodies, with the same identities: `fork.started {occurrence, branches}` on the parallel node once the expansion knows its items (one `BranchRef {fork, index}` per item, in item order; none for an empty list), `branch.completed {occurrence, result}` per clone as its token reaches the fan-in, `fork.completed {occurrence, fork, results}` at the fan-in in item order; the clones are `member {fork, index}`, the fan-in `join {fork}`. Beside them: `node.expanded` (`derived.clones`); one `invocation.declared` per branch child with its `parent` link, `invocation.finished` per child; `step.progress.recorded` kinds `attractor.parallel.branch.started`, `attractor.parallel.branch.completed` (the delegates) and `attractor.parallel.completed` (the fan-in, with `parallel.results`). The roles come from one mechanism for both fan-outs: `BranchMap::of` reads the graph shape, `BranchMap::with_expansions` reads the engine's applied splices (the template, its clones by item index, the one node whose forward arm reaches the template as the fork) | `ForkOccurrence`, `BranchRef {fork, index}`, child invocation (its call slot names the occurrence) | durable | `embedding::the_milestone_workflow_runs_through_the_embedding_boundary` (`fork:2`, `join`, `forks`, `joins`; live equals replay), `fabro_blackbox::for_each_branches_keep_distinct_values_under_one_key_in_item_order` (the three bodies through `replay_run`), `embedding_readiness`, `fabro_readiness_blackbox` (one expansion, one fork, one join, two children) |
| `interview.started/completed/timeout/interrupted` | `step.progress.recorded` with `parsed.question` (type, choices, interaction identity; each choice's `description` and `preview` and the question's `context` when the asking step set them: a human gate reads them from its edges' `human.description` and `human.preview` and from the previous stage's response, a native agent's question carries Pebble's), `wait.state.changed {awaiting_answer}`, `control.requested` with `derived.answer` and `derived.deliverable`; a timeout is `parsed.expired {question, waited_ms, default}`, the step's own report with the default it took (the gate's outcome then follows as `step.finished`: success with the default, or failure class `retry_requested` without one); an interruption is `control.requested` with `derived.answer.cancelled` (the interviewer ended the interview) or `cancel.requested` with the attempt's `Cancelled` status; a sensitive answer stays `{"$secret": …}`. The interview receipt records the same disposition: `timed_out {default}` with delivery `expired` | node, firing, attempt, question id | durable | `embedding::the_workflow_runs_without_adapters_…`, `embedding_readiness` (`questions`, `answers`), `petri::interview` (the interviewer contract; expiry with and without a default, a cancelled reply, a reply after the deadline, each live and through `replay_run`), `petri-fabro-steps::steps::a_human_gate_carries_choice_descriptions_previews_and_its_context`, `fabro_blackbox::a_withheld_reply_…` (the receipt), `inspect_cli::inspect_shows_a_sensitive_answer_as_a_secret_reference_only` |
| `command.started/completed` | `step.started` (the script is `subject.node.meta.script`), `step.finished {outcome}` (`metrics.exit_code`, `duration_ms`, `output`; `metrics.custom.output.dropped_bytes`, `output.truncated_lines` and `output.incomplete` when the capture did not keep every byte, so no output byte is lost silently: `crates/attractor/FORMAT.md`, "Steps at run time"), `step.progress.recorded` (log lines and artifacts; a line the executor's cap cut ends with a marker naming the bytes dropped) | node, firing, attempt | durable | `embedding::the_workflow_runs_without_adapters_…`, `petri-fabro-steps::steps::the_public_stream_carries_the_script_and_the_matched_condition`, `petri-fabro-steps::steps::a_long_line_is_stored_whole_or_its_loss_is_recorded` |
| `agent.*` session, tool calls, LLM requests, steering, interrupts | the backend envelope in `step.progress.recorded` (`kind` naming the backend; `event` the envelope with `session_id`, `parent_session_id`, `tool_call_id`, `stream_id`, `seq`): Pebble's own `CodingAgentEvent`, one stream per session, forwarded as recorded; on the ACP backend `kind = "acp"` with `event` = `{ session_id, seq, tool_call_id?, method, update }` for every `session/update` the agent sends (text, thoughts, tool calls and their updates, plan, usage), `{ session_id, seq, method, params }` for any other notification, and `{ ..., method = "session/request_permission", params, outcome, blocked }` for a permission request with Petri's answer (`crates/attractor/FORMAT.md`, "ACP products"); a host's own tool (the `HostTools` capability, `crates/attractor/steps/src/host_tools.rs`) is called and recorded like any other tool, under the stage that opened the session; a host's interrupt of the turn is `control.requested` (`$interrupt`) then `step.progress.recorded` kind `attractor.turn.interrupted` (above) | session id, stream id and sequence, tool call id | durable | `fabro_subagents_blackbox::a_parent_delegates_a_workspace_change_to_a_child`, `embedding_readiness`, `petri::interrupt` |
| `agent.*` threads (retained sessions, fidelity) | `step.progress.recorded` kind `attractor.thread` (thread id, fidelity, resolution) once per native session | node, firing, attempt, session | durable | `fabro_readiness_blackbox`, `fabro_hooks_blackbox::full_fidelity_nodes_share_one_conversation_through_the_binary` |
| `agent.tools.available`: the tools a session could call | `step.progress.recorded` kind `attractor.tools` (`session`, `tools[] {name, description, source, category}`: every tool the model was offered, Pebble's `ToolSource` and Petri's category `builtin`, `mcp`, `subagent`, `host` or `question`), once per native session: the node's own once the agent is built, each child session right after its `SessionStarted` envelope with the tools Pebble's inheritance gives it. Whether a tool was called derives from the envelope's `ToolCallStarted` (`crates/attractor/FORMAT.md`, "Native Pebble") | node, firing, attempt, session | durable | `petri-fabro-steps::host_tools` (`a_host_tool_edits_the_workspace_under_the_stages_identity`: the list equals the tools the model was offered, host tools included; `a_sub_agent_calls_an_inherited_host_tool`: the child's list equals what the child was offered) |
| `agent.route.failover`, `prompt.failover` (C1): plan, per-target requests, decisions, accounting | `step.progress.recorded` kind `attractor.fallback.plan` (routes, notices: Petri's knowledge). Everything else is Pebble's, in the backend envelope in `step.progress.recorded`: `SessionStarted` (each route's provider and model), `RouteFailover` (from, to, attempt, the failed route's `usage` (tokens and cost) and timings, typed error, `continuation`), `RouteFailoverStopped` (route, attempt, `ineligible`/`exhausted`, the error; published only when the plan named a fallback route), `AssistantMessage` (each answer's usage). The move and a server that did not start are also lines on the node's stderr. No `fabro.fallback.route`, `usage`, `failover` or `stop`, and no `metrics.custom.fallback.*` (removed 2026-09-12; see decision `pebble-events-are-the-agent-contract`) | node, firing, attempt, session | durable (a resumed node starts a new plan) | `fallback_events` (3), `fabro_fallback_blackbox` (15), `fabro_readiness_blackbox` (failover, then `RouteFailoverStopped` `exhausted` in the failure case), `embedding_readiness` |
| `agent.mcp.*` (C2): server and tool lifecycle | Pebble's own events in the backend envelope in `step.progress.recorded`: `McpServerReady` (server, tools, `startup_ms`), `McpServerFailed` (server, error, `startup_ms`), `McpServerDisconnected` (server, error; once per closed connection), and `ToolCallStarted`/`ToolCallCompleted` under `mcp__<server>__<tool>` (call id, `is_error`, `error_kind`: `timeout`, `unavailable`, `cancelled`, `denied`, ...). One `step.progress.recorded` kind for the fact Pebble cannot know: `attractor.mcp.unavailable` (server, error), a server Petri never named to Pebble because a secret its entry needs is unavailable. A server that did not start is also a line on the node's stderr. No `fabro.mcp.server` phases and no `fabro.mcp.tool` (removed 2026-09-12; see decision `pebble-events-are-the-agent-contract`) | node, firing, attempt, server name, tool call id | durable | `petri-fabro-steps::mcp` (8), `fabro_readiness_blackbox`, `embedding_readiness` (through `replay_run`); terminal and workspace reads in `fabro_mcp_blackbox` |
| `agent.skills.*` (C3): discovery and loading | `step.progress.recorded` kinds `attractor.skills` (the ordered directories with their sources, once per native session) and `attractor.skills.warning` (`malformed`, `unreadable`, `missing_directory`); Pebble's `SkillsDiscovered`/`SkillActivated` in the backend envelope in `step.progress.recorded` | node, firing, attempt, scope | durable | `fabro_readiness_blackbox`, `embedding_readiness`; raw-log reads in `fabro_skills_blackbox` |
| `agent.subagent.*` (C4): spawn, input, wait, close, child usage | the backend envelope in `step.progress.recorded` under the parent's session: `SubAgentSpawned {agent_id, depth, task}`, `SubAgentCompleted`, `SubAgentFailed`, `SubAgentClosed`; the child's own events under the child's session with `parent_session`; `step.finished` (`outcome.metrics.custom.pebble.subagents`) (counts and the tree's summed `usage`, and per child session its parent, `provider`, `model` and `usage`) | parent session, child session, stream sequence | durable | `fabro_subagents_blackbox::a_parent_delegates_a_workspace_change_to_a_child` (usage rebuilt from events equals the metric), `fabro_readiness_blackbox`, `embedding_readiness` |
| `agent.compaction.*` (C5): lifecycle and summary usage | Pebble's `CompactionStarted/Completed/Failed/Cancelled` in the backend envelope in `step.progress.recorded`, `CompactionCompleted` with the summary call's `usage` (tokens and cost in one object); `step.progress.recorded` kind `attractor.compaction` (the node's attribution of the completion, with the same `usage` and the start's estimate) once per completed compaction of the node's own session, right after its `CompactionCompleted`; `step.finished` (`outcome.metrics.custom.pebble.compaction_*`) | node, firing, attempt, session | durable | `petri-fabro-steps::compaction::public_events_account_for_the_compaction_and_later_activity`, `fabro_readiness_blackbox`, `embedding_readiness` |
| `agent.*` error and warning | `step.finished` failure class; `step.progress.recorded` kinds `attractor.hook.warning` (what an ACP tool hook can see, and a tool call the agent ran without asking), `attractor.skills.warning`, `fabro.mcp.server {failed}` | node, firing, attempt | durable | `petri-frontend-fabro::hooks::acp_tool_hooks_are_best_effort_with_explicit_warnings`, `fabro_mcp_blackbox` |
| `watchdog.timeout` | `run.stalled {stall_timeout_ms, idle_ms}` beside `invocation.cancel.requested {reason: stall_timeout}` | invocation | durable (coordinator log) | `controls::an_idle_run_is_cancelled_by_the_watchdog` |
| `subgraph.started/completed` | `invocation.declared` (`context.parent`), `invocation.finished`, `execution.*` with `parent` | invocation, parent execution, firing, attempt, call slot | durable | `petri-execution::inspect::a_nested_invocation_keeps_its_own_context_and_parent_link`, `petri-fabro-acceptance::workflow` |
| local setup (`[run.prepare]`, `[run.clone]`) | the `run_prepare_N` stages' events; `step.progress.recorded` kind `attractor.checkout` (repository, commit, depth, files) on `start` | node, firing | durable | `embedding::the_milestone_workflow_runs_through_the_embedding_boundary`, `fabro_scenarios_blackbox` (checkout) |
| hook decisions (workflow points) | a `hook` note (`step.progress.recorded`, `parsed.note.kind = hook`) with the `HookReport` (point, decision, each hook's state, duration and usage, fail-open warnings); a report that ran no hook is silent | node, firing, attempt | durable | `embedding::a_hook_service_runs_each_hook_once_at_its_point`, `petri-fabro-steps::hooks` |
| hook-owned model requests, agent and tool activity, usage | `parsed.hook_activity {hook, backend, envelope}` per event of a hook's agent (a prompt hook has none), before the `hook` note whose `hooks[].usage` carries the hook's requests, tool calls, timings and the model `usage` (tokens and cost) they summed to; the same for a point a step asks itself, beside its `attractor.hook` event; the stage's own the backend envelope in `step.progress.recorded` and `pebble.usage` count the stage's agent alone | node, firing, attempt, hook operation (point, hook name), the hook agent's session | durable | `petri-fabro-steps::hooks::an_agent_hooks_activity_is_kept_apart_from_the_stages_own` (live and replay), `agent_hooks_investigate_the_workspace_then_decide`, `prompt_hooks_evaluate_with_one_model_call_and_fail_open`, `prompt_hook_usage_records_a_failed_and_a_timed_out_request`, `an_agent_hook_timeout_stops_its_tool_before_failing_open` |
| hook decisions (points a step asks itself) | `step.progress.recorded` kind `attractor.hook` (`event` = `sandbox_ready`, `run_start` and `stage_start` on the root `start` stage; `parallel_start` on the fork node, `parallel_complete` on the fan-in; `pre_tool_use`, `post_tool_use`, `post_tool_use_failure` at the tool boundary; and the report), one per point that ran a hook, a child's under the parent stage | node, firing, attempt | durable | `fabro_readiness_blackbox`, `embedding_readiness` (counts per stage, two `block` decisions), `petri-fabro-steps::hooks::a_replacement_service_receives_every_step_driven_phase_once` |
| run-level hooks (`run_complete`, `run_failed`, `sandbox_cleanup`) | `run.note.recorded` (`parsed.note.kind = hook`) from the coordinator log, with no subject and the execution named: one per run-level point that ran a hook (`point` is `run.finished` or `scope_released`), in the order the points ran, before `run.finished`; the run's own end is `run.finished` | execution | durable (coordinator log) | `fabro_milestone_blackbox` (`assert_run_level_notes`: the reports through `replay_run` and `petri inspect` on a succeeded, a failed and a cancelled run, beside the hooks' effects) |
| sandbox instance: provider, id, image, snapshot, working directory, workspace, ready duration, failure | `scope.acquired` (the provider, the provider's id for the sandbox, its image and snapshot when known, the working directory, the workspace and lease, `duration_ms`), `scope.failed` (the error, its causes, the reserved provider, `duration_ms`); `invocation.declared` (`sandbox`) is the binding (isolated or inherited). A host reconnects to the sandbox by `provider` and `instance`; the clone a host performed itself is on its own `attractor.checkout` payload | scope, lease, execution | durable (engine log) | `petri-execution::scope_records` (the host and Docker providers, live equals replay through `replay_run_dir`, `verify_export`; the failure) |
| retention outcome: kept or deleted | `scope.released` (`outcome`, `retained`, `problems`, the provider and instance), once per lease the invocation owned; the `scope_released` hook's own report is a `run.note.recorded` beside it | invocation, lease | durable (coordinator log) | `petri-execution::scope_records` (`Retention::Always` keeps and records `retained: true`; `Never` deletes; a failed acquisition's lease is released with no instance) |
| local sandbox output references | `step.progress.recorded` (log lines and artifacts), `blob://sha256/…` references in outputs | scope | durable | `inspect_cli`, `petri-fabro-steps::steps::large_command_output_is_offloaded_and_reads_back_logically` |
| budget pause and resume | `parsed.budget {state, attempt, remaining_ms, pending_questions}` on the driver's `budget_paused` and `budget_resumed` notes | node, firing, attempt | durable | `petri-driver::interview_budget::the_waiting_stage_pays_only_for_active_work` |
| pause and unpause | `run.paused`, `run.unpaused` from the coordinator's `RunPaused` and `RunUnpaused` records | run | durable; a resume starts paused when the last control recorded is a pause | `controls::pause_holds_admission_and_unpause_releases_it`, `controls::a_pause_survives_resume_and_holds_admission_until_unpaused`, `fabro_resume_blackbox::a_paused_run_stays_paused_across_resume_until_unpaused` |
| platform lifecycle, `checkpoint.*`, `git.*`, `pull_request.*`, product projections | not emitted; a host performs them in its `transition` and records a `transition` note | | | `embedding::adapters_run_in_order_and_checkpoint_work_follows_source_metadata` |

Replay equality: `embedding_readiness::the_combined_workflow_runs_through_the_embedding_boundary`
compares the live stream with `replay_run` event for event over a run that
exercises every row above but the live-only ones (624 events). Floats inside
backend payloads survive the round trip exactly because the workspace's
`serde_json` enables `float_roundtrip`.

## Known backend limits

- The native agent backend (`pebble`) records every `CodingAgentEvent` as an
  acknowledged `StepEvent::Custom`: Pebble's `record` returns only once the
  record is in the durable log, so an event Pebble treats as recorded survives
  a crash, and a store that cannot write stops the prompt. Compaction of
  the agent's context appears in that stream as Pebble's `CompactionStarted`,
  `CompactionCompleted`, `CompactionFailed` and `CompactionCancelled`, the
  completion with the summary call's usage and cost, which Pebble bills to
  the prompt that compacted. The Fabro backend folds the session's own
  completions from that stream as they arrive and adds, after each, one
  custom payload with `kind = "attractor.compaction"` attributing it to the node
  and attempt; the payload carries no `summary_truncated`, which Pebble puts
  on no event; see `crates/attractor/FORMAT.md`, "Compaction".
- The ACP backend records what the external agent sends over ACP; tool calls
  the agent does not report are not observable.
- Agent facts are Pebble's; Petri adds run, invocation, node and attempt
  attribution and does not restate them.
- A backend envelope is a custom payload with a string `kind` and an
  `event` **object**. A step's own payload may carry a string `event` (a
  hook report names its hook event); it is not a backend envelope.
- A `for_each` child invocation's own stage carries no `branch_role` in its
  `meta`: the item index is known only at run time. The parent-side clone
  (`<template>#<index>`, a `parallel.branch` delegate) carries the member
  role, and the child's `meta.branch = {fork, target}` names the fork.
- An empty `for_each` list expands to one clone of the IR's placeholder item
  (`{"$placeholder": true}`, `ir::placeholder::PLACEHOLDER_ITEM_KEY`), so the
  fan-in still fires. The clone is no branch: `BranchMap` gives its nodes no
  role, `fork.started` and `fork.completed` carry zero branches, and no
  `branch.completed` is emitted. Its `node.expanded` clone and its own
  `visit_*` events (`<template>#0`, `synthetic: true` from the template) are
  the only trace of it.
