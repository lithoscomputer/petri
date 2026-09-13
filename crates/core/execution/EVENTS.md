# The public event contract

`execution::events` is the versioned event stream an embedding host projects a
run from. This file is the contract for `EVENT_CONTRACT_VERSION` 1. The Rust
types in `crates/core/execution/src/events.rs` are authoritative for field
detail; this file states the guarantees.

## Sources and identity

Every `RunEvent` is derived from one durable record: a coordinator record
(`coordinator.jsonl`) or an engine record (one execution's `events.jsonl`) with
the post-apply engine state beside it. The derivation is the same live (the
`EventProjector` observer) and over a finished run dir (`replay_run`). Every
record carries the time it was appended (`recorded_at`, milliseconds since the
Unix epoch), read at the recording boundary — the driver's append for an
engine record, the coordinator store's for a coordinator record, never inside
the state machine — and persisted beside the record, so a replayed event
carries the same time the live one did.

`EventId { source, seq, index }` is the stable identity: the log the record came
from (`coordinator`, or `execution: <id>`), the record's `seq` in that log, and
the ordinal of this event among the events one record produced. Within one
source the order is total. Every event carries `invocation` and `execution`
when it has them, and `parent` (the calling execution, firing, attempt and call
slot) for a nested invocation.

`subject` names the node (`NodeRef`: id, instance name, step kind, the
frontend's `meta` verbatim) and, when the event is about a firing, the firing
id, the visit ordinal (which firing of the node within the execution, 1-based),
the attempt (1-based within the firing), the generation, and the node's
`BranchRole` (`none`, `fork`, `member` of a fork's branch, or `join`).

Visits and attempts are distinct: a retry advances `attempt` and keeps the
firing and visit; a loop that fires the node again starts a new firing and
advances `visit`.

A fork's branch events (`fork_started`, `branch_completed`, `fork_completed`)
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

## Events

Run and invocation events (coordinator log): `run_started`, `run_finished`,
`invocation_declared` (with the parent link, graph digest, sandbox binding and
initial context), `invocation_finished` (with the `InvocationResult`),
`invocation_cancel_requested` (with the `reason` the requester gave, when it
gave one: `stall_timeout`, `interrupt`, `control`), `stall_timeout` (derived
beside the cancel request the stall watchdog made, with the budget and the
idle time), `execution_declared` (predecessor, index, entry),
`execution_finished` (the engine exit: terminal status or restart).

Run controls (coordinator log): `run_paused`, `run_unpaused`, derived from
the `RunPaused` and `RunUnpaused` records the control service appends through
the coordinator. Replay carries them, and a resume whose last recorded control
is a pause starts with admission held. The records are additive to coordinator
format version 2 (version 3 added `recorded_at` to every coordinator record).

Run-level notes (coordinator log): a `host_note` with no subject, derived from
a `RunNote` record: the report of a hook point that belongs to no firing
(`run_finished`, `scope_released`), with the execution whose driver ran it,
and a `hook_activity` for each event a run-level hook's agent produced. The
coordinator appends them from the execution's report, in the order the
points ran, before it records `RunFinished`. Additive to coordinator format
version 2.

Execution events (engine log), each attributed to a subject where one exists:

| Event | When |
| --- | --- |
| `execution_started`, `execution_admitted` | the execution's own start and admission |
| `visit_started` | a firing exists for a node whose join was satisfied |
| `attempt_admitted` | the host or middleware decided on an attempt (`Admit`, `Skip`, `Block`, with the trace) |
| `attempt_started` | the attempt was dispatched to its step |
| `attempt_finished` | an attempt returned; `final` says whether it is the firing's outcome, `exhausted` whether retries ran out |
| `retry_scheduled`, `retry_elapsed` | the backoff between attempts |
| `visit_completed` | the firing's final record exists; `executed` is false for a synthesized completion (false precondition, cancelled scope, blocked or skipped admission); `attempts` is the count |
| `routes_resolved` | one `RouteChoice` per group with the decision, resolved target, interventions (overrides, jumps, blocks) and whether a weighted draw happened |
| `route_applied` | one applied route: edge (with target, transition, back), jump, or none |
| `fork_started`, `branch_completed`, `fork_completed` | a fork's branches start; a branch reaches its end; the fork's branches are all accounted for, in branch order, with the `disposition` that closed them (`joined`, `cancelled`, `killed`; see "Fork closure"). All three carry the `ForkOccurrence` of the fork visit; `branch_completed`'s subject is the branch's last firing with its visit, attempt and generation. A static fork's branches are its routing groups; a `for_each` expansion's branches are its clones, in item order, and the fork is the node that fanned out into the template (Fabro's `parallel` node), so both fan-outs carry the same identities |
| `node_expanded` | a `for_each` expansion with its clones |
| `question_asked`, `control_delivered` | a question on the firing's progress channel; a host control decoded as an answer when it is one, with whether the firing could receive it |
| `question_expired` | the step reported that its question's answer deadline passed (`steps::QuestionExpired` on the progress channel): the question id, how long it waited (`waited_ms`), and the option it took on its own (`default`, by key) when it had one. The attempt's outcome follows as `attempt_finished`; a host never infers a timeout from that outcome |
| `wait_state_changed` | `awaiting_admission`, `running`, `awaiting_answer`, `awaiting_retry`, `cancelling`; a delivered answer or the question's expiry ends `awaiting_answer` |
| `cancel_requested`, `kill_requested` | the two stop tiers, scope or group |
| `output_line`, `artifact_recorded` | step output and artifacts |
| `agent_activity` | a backend's own event envelope (`kind` names the backend; for `pebble` the envelope is Pebble's `CodingAgentEvent`) with the session, parent session, tool call, stream and stream sequence read out of it. A native agent's sub-agents are on the same stream: the lifecycle (`SubAgentSpawned`, `SubAgentTurnStarted`, `SubAgentCompleted`, `SubAgentFailed`, `SubAgentClosed`) under the parent's session, a child's own events under the child's session with `parent_session` naming its immediate parent, all attributed to the parent stage (`crates/fabro/FORMAT.md`, "Native Pebble"). A stage's own agent only: a hook's agent is `hook_activity` |
| `hook_activity` | one event a hook's own agent produced: `hook {point, hook}` names the hook operation (with the subject's firing and attempt, one execution of one hook) and `activity` is the same `AgentActivity` an `agent_activity` carries, in its own session. Attributed to the firing whose hook ran (or, for a run-level point, to no subject), and never to the stage's own agent, so a consumer summing `agent_activity` never counts a hook's model requests as the stage's. From the `hook.activity` notes the hook service's report carries, recorded before the `hook` note |
| `budget_paused`, `budget_resumed` | an executor-enforced attempt budget stopped counting (the attempt asked a question; `remaining_ms` is the active-work time left, `pending_questions` how many wait) and counted again (its last pending question was answered); from the driver's durable `budget_paused`/`budget_resumed` notes |
| `host_note` | a `driver::lifecycle::Note` the host or the driver recorded: `result_prepared` (original attempt evidence beside an adjusted result), `transition` (overrides, best-effort problems, a block), `hook` (a hook service report: the point, the decision, each hook's state, duration, message and `usage`: the model requests it made, the tool calls its agent started, the backend's token counts in the `pebble.usage` shape, cost and timings). A run-level hook report is the same `hook` note from the coordinator log, with no subject |
| `step_custom` | any other step-defined progress payload |

Usage and timing: `attempt_finished` and `visit_completed` carry the outcome's
`metrics` (`duration_ms`, `exit_code`, `custom`). The driver fills
`duration_ms` with the observed wall-clock duration of the attempt when the step
kind did not report one. The native agent backend reports `pebble.usage`,
`pebble.cost_usd_micros`, `pebble.inference_ms`, `pebble.tool_ms` and
`pebble.subagents` (the node's agent tree: children spawned, completed,
failed and closed, and their summed usage by session) under `custom`.

Times: every event carries `recorded_at`, when its record was appended to its
log, the same live and on replay. A host reconstructs start and completion
times from the events that mark them — `run_started`/`run_finished`,
`invocation_declared`/`invocation_finished`, `execution_declared`/
`execution_finished`, `visit_started`/`visit_completed`,
`attempt_started`/`attempt_finished` (a command's boundaries),
`question_asked`/`control_delivered` (an interview's) — and durations from
their differences, beside the step-reported `duration_ms`. `observed_at` is
when the projector saw the record live; it is absent on replay and is never a
substitute for `recorded_at`: replay time is not execution time. A record
replay regenerates that never reached a log (a crash's lost tail, before any
resume re-recorded it) has no `recorded_at`; once a resume re-records it, it
carries the resume's recording time, and a host that saw the original live
keeps whichever copy it deduplicated first.

## Fork closure

Every fork that announced `fork_started` closes with exactly one
`fork_completed`, live and on replay, whatever stopped it. The projection
keeps each open fork by its `ForkOccurrence` and ties its branches and its
join to that occurrence through the generation the fork's tokens carry, so
two visits of one fork or a fork inside a branch never share a closure, and
every `branch_completed` and `fork_completed` names the occurrence its
`fork_started` announced.

| `disposition` | When | Each `branch_completed` | `results` |
| --- | --- | --- | --- |
| `joined` | every branch's token reached the join and the join fired (`visit_started` on the join follows) | the token that reached the join, with its `payload` | every branch |
| `cancelled` | the join completed without running (`visit_completed {executed: false}` with a `cancelled` outcome): its scope was cancelled, or every branch reached it cancelled | the branch's last record; no `payload`, since the join never ran | every branch with a record |
| `killed` | the fork's scope was killed, so the join's tokens were dropped and it never fires; the fork closes once no branch of it has a live firing left, on the fork's own firing as subject | the branch's last record; no `payload` | the branches with a record; one that never recorded is absent |

A branch's own terminal facts stay where they are: the member's
`visit_completed` (with `executed`, false for a branch cancelled before it was
admitted) and, for a Fabro branch, the child's `invocation_finished`. A Fabro
branch child cancelled before it was admitted still gets an execution that
records the cancellation and no `attempt_started`.

### The Fabro branch events

The Fabro frontend's branch delegates and fan-in add `step_custom` events
with dispositions of their own. This is the mapping from Petri's facts to the
pinned Fabro's `parallel.*` events; Fabro emits no group event on
cancellation, and this contract does not claim it does.

| Petri | Fabro |
| --- | --- |
| `fork_started {occurrence}` | `parallel.started`; Fabro's `parallel_group_id` (`node@visit`) is the occurrence's fork node and visit |
| `fabro.parallel.branch.started {fork, occurrence, branch, index, item_label, invocation}`, emitted once the child's engine has started (it holds a slot under the fork's gate, as Fabro's branch holds a permit). `occurrence` is `{fork, firing}`: the fork step's firing in the event's execution, the same firing the typed `fork_started` names. The child's `invocation_declared.call.slot` is `branch:<fork>@<firing>:<index>:<target>`, so the child link names the occurrence too | `parallel.branch.started` |
| `fabro.parallel.branch.completed {fork, occurrence, branch, index, item_label, invocation, status, disposition, started, duration_ms}`, emitted on every path a branch ends. `status` is the envelope's Fabro status; `disposition` is `completed` (the child finished; `status` says how), `cancelled` (the child settled cancelled; `status` is `failed`), `killed` (the stop escalated to a kill before the child settled; `failed`) or `failed_to_start` (the child could not be declared; `failed`); `started` is whether the child's engine ever started (false for a queued branch the cancel reached first) | `parallel.branch.completed` with the same `status`, for every event with `started: true`. Fabro emits none for a branch that never held a slot, so a host projecting Fabro's stream drops `started: false`. Fabro reports a cancelled branch as `failed` with duration 0; Petri carries the observed duration |
| `fabro.parallel.completed {node, fork, occurrence, branch_count, success_count, failure_count, status}`, emitted by the fan-in when it runs, with the occurrence its inputs carried | `parallel.completed`. Neither engine emits it for a cancelled or killed fork; the terminal group fact is then Petri's `fork_completed {disposition}`, which Fabro has no event for |

## Ordering and delivery

- Per execution, events are delivered in record order, and within one record
  in `index` order. The order is causal: admission before start, start before
  finish, the final finish before `visit_completed`, `visit_completed` before
  `routes_resolved`, `routes_resolved` before `route_applied`, host notes
  before the record they annotate.
- Across executions the `parent` link and `execution_declared.predecessor` tie
  the streams together. The coordinator log's records are delivered as they
  are appended; a fresh run's `run_started` is delivered to an observer when
  it attaches.
- `EventProjector` is bounded: the observer callback derives each record's
  events and queues them without waiting; a pump task awaits the host's
  `RunEventSink::deliver` per event, in order. The queue holds
  `ProjectorOptions::capacity` events (1024 by default), which is the most
  the projector keeps in memory: a slow sink delays delivery and the run
  keeps its pace. An event projected while the queue is full is not queued:
  the `ProjectionReceipt` counts it as `overflowed` (and `undelivered`),
  live delivery goes on with the next event that finds room, so the sink
  sees each source in record order with gaps, and the durable log keeps the
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
  folds the on-disk prefix into its state without delivering it.
- A step's progress event is queued or acknowledged. `StepCtx::logs.send`
  resolves once the event is queued: it is ordered behind the attempt's
  earlier sends and ahead of its outcome, because the driver's completion
  fence drains the queue before it records `StepFinished`, so a durable
  outcome implies its earlier events are durable. Queueing alone is not
  durability: a crash between the queue and the append loses the event.
  `send_acked` resolves only after the driver appended the record and every
  observer's durable storage confirmed it (`EventObserver::durable`; the run
  dir's writer answers once the record is written and flushed to
  `events.jsonl`), and a store's write failure is the sender's error. The
  native agent backend records every Pebble event acknowledged, so Pebble's
  own acknowledgement means the event is in Petri's log. What a crash still
  repeats is the attempt: an attempt whose finish never landed is
  re-dispatched on resume and emits its events again, so an acknowledged
  record can appear twice; a consumer deduplicates delivery by `EventId` and
  agent activity by Pebble's `(stream_id, seq)`.
- Every event is derived from a durable record, output lines included, and
  carries the record's `recorded_at`, identical live and on replay. The one
  live-only field on a derived event is `observed_at` (milliseconds since the
  epoch when the projector saw the record), absent on replay. A backend's live
  stream chunks that never reached the step's progress channel are not in the
  contract.

## Secrets

Records are masked by the driver before they are appended, so every value is
post-mask: a secret reference stays `{"$secret": ...}`, a masked value stays
`***`. Host notes are masked the same way before they are recorded.

## Terminal output

The CLI's terminal rendering (`[node#firing] line`, status lines) is a
presentation over these events and the run dir. It is not part of the
contract; a host consumes `RunEvent`s and never parses terminal text.

## Event-coverage matrix

The final event contract for readiness item 7, completed at milestone D
(readiness item 10): every Fabro execution-related need, the public Petri
source that carries it, the identities a host keys on, whether the fact is a
durable record (in the log, delivered live and on replay) or live-only, and
the projection test that proves it from `RunEvent`s alone. The Fabro names
describe the consumer's need, not Petri event names. Every row's events carry
`recorded_at`, so the timestamps and durations Fabro's projection needs come
from the same events. A `step_custom` row
names the `kind` of the `StepEvent::Custom` payload; every such payload
also carries `node`, `firing` and `attempt` beside the event's `subject`.

| Fabro need | Public source | Identities | Durability | Projection test |
| --- | --- | --- | --- | --- |
| `run.started/completed/failed`, root vs internal invocation | `run_started {root, middleware_chain}`, `run_finished {status}`, `invocation_declared` (`call` is `None` on the root, a `ParentLink` on a child), `invocation_finished {result}`, `execution_declared/finished` | `EventId`, `invocation`, `execution`, `parent` | durable (coordinator log) | `embedding::the_workflow_runs_without_adapters_and_the_events_reconstruct_it`, `embedding_readiness` |
| run notices, steer, interrupt, cancel reasons | `invocation_cancel_requested {reason}` (`stall_timeout`, `interrupt`, `control`), `cancel_requested`, `kill_requested`, `control_delivered {Deliver}` (a steer is `{"$steer": ...}`) | invocation, firing | durable | `fabro_readiness_blackbox::…_is_cancelled_…` (`reason = interrupt`), `controls::an_idle_run_is_cancelled_by_the_watchdog` (`stall_timeout`), `controls::the_control_file_pauses_unpauses_and_steers_without_answering` |
| `stage.started/completed/failed/retrying` | `visit_started`, `attempt_admitted`, `attempt_started`, `attempt_finished {final, exhausted}`, `retry_scheduled`, `retry_elapsed`, `visit_completed {executed, attempts}`; `subject.node.meta.kind` and `synthetic` map lowering nodes to the logical stage | node, firing, visit, attempt, generation | durable | `embedding::the_workflow_runs_without_adapters_…` (the retry), `embedding_readiness` (every logical stage's final status) |
| `stage.prompt`, `prompt.completed` | `step_custom` kinds `fabro.prompt`, `fabro.prompt.completed`; a prompt node's `attempt_finished` output | node, firing, attempt | durable | `petri-fabro-steps::prompt` (prompt events), `fabro_blackbox::a_prompt_node_makes_one_tool_free_model_call` |
| `edge.selected`, `loop.restart` | `routes_resolved {choices}` (decision, target, overrides, jumps, blocks, weighted draw), `route_applied {edge / jump / none, transition, back}`; a restart is `execution_finished {Restart}` then `execution_declared {predecessor}` | firing, edge, execution | durable | `embedding::transitions_override_block_or_continue`, `controls::node_visit_totals_survive_a_restart_while_context_resets` |
| `parallel.started`, branch start and completion, `parallel.completed` (static fan-out) | `fork_started {occurrence, branches}`, `branch_completed {occurrence, result}`, `fork_completed {occurrence, fork, results, disposition}` in branch order; `BranchRole` on every subject; the `fabro.parallel.*` `step_custom` events with the same occurrence and the branch dispositions ("Fork closure") | `ForkOccurrence {execution, fork, firing, visit, generation}`, `BranchRef {fork, index}` | durable | `embedding::the_workflow_runs_without_adapters_…` (`forks`, `joins`); `petri-fabro-steps::parallel` (`a_repeated_fork_publishes_results_per_visit_with_its_own_children`, `duplicate_targets_are_separate_branches_with_their_own_index`, `a_nested_fork_runs_inside_its_branch_and_reports_its_own_results`: two visits, duplicate targets and a nested fork each keyed on their own occurrence, live and replayed; `a_clean_cancel_during_work_closes_the_branches_and_the_fork`, `a_cancel_before_admission_records_a_branch_that_never_started`, `a_cancel_before_the_fan_in_keeps_the_finished_branch_result`, `a_kill_after_the_cancel_closes_the_fork_as_killed`: every closure live and through `replay_run`) |
| the same for a `for_each` fan-out (an expansion) | the same three bodies, with the same identities: `fork_started {occurrence, branches}` on the parallel node once the expansion knows its items (one `BranchRef {fork, index}` per item, in item order; none for an empty list), `branch_completed {occurrence, result}` per clone as its token reaches the fan-in, `fork_completed {occurrence, fork, results}` at the fan-in in item order; the clones are `member {fork, index}`, the fan-in `join {fork}`. Beside them: `node_expanded {clones}`; one `invocation_declared` per branch child with its `parent` link, `invocation_finished` per child; `step_custom` kinds `fabro.parallel.branch.started`, `fabro.parallel.branch.completed` (the delegates) and `fabro.parallel.completed` (the fan-in, with `parallel.results`). The roles come from one mechanism for both fan-outs: `BranchMap::of` reads the graph shape, `BranchMap::with_expansions` reads the engine's applied splices (the template, its clones by item index, the one node whose forward arm reaches the template as the fork) | `ForkOccurrence`, `BranchRef {fork, index}`, child invocation (its call slot names the occurrence) | durable | `embedding::the_milestone_workflow_runs_through_the_embedding_boundary` (`fork:2`, `join`, `forks`, `joins`; live equals replay), `fabro_blackbox::for_each_branches_keep_distinct_values_under_one_key_in_item_order` (the three bodies through `replay_run`), `embedding_readiness`, `fabro_readiness_blackbox` (one expansion, one fork, one join, two children) |
| `interview.started/completed/timeout/interrupted` | `question_asked {question}` (type, choices, interaction identity), `wait_state_changed {awaiting_answer}`, `control_delivered {Answer, deliverable}`; a timeout is `question_expired {question, waited_ms, default}`, the step's own report with the default it took (the gate's outcome then follows as `attempt_finished`: success with the default, or failure class `retry_requested` without one); an interruption is `control_delivered {Answer {cancelled}}` (the interviewer ended the interview) or `cancel_requested` with the attempt's `Cancelled` status; a sensitive answer stays `{"$secret": …}`. The interview receipt records the same disposition: `timed_out {default}` with delivery `expired` | node, firing, attempt, question id | durable | `embedding::the_workflow_runs_without_adapters_…`, `embedding_readiness` (`questions`, `answers`), `petri::interview` (the interviewer contract; expiry with and without a default, a cancelled reply, a reply after the deadline, each live and through `replay_run`), `fabro_blackbox::a_withheld_reply_…` (the receipt), `inspect_cli::inspect_shows_a_sensitive_answer_as_a_secret_reference_only` |
| `command.started/completed` | `attempt_started`, `attempt_finished {outcome}` (`metrics.exit_code`, `duration_ms`, `output`), `output_line`, `artifact_recorded` | node, firing, attempt | durable | `embedding::the_workflow_runs_without_adapters_…` |
| `agent.*` session, tool calls, LLM requests, steering | `agent_activity {backend, session, parent_session, tool_call, stream, stream_seq, envelope}`: Pebble's own `CodingAgentEvent` envelope, one stream per session | session id, stream id and sequence, tool call id | durable | `fabro_subagents_blackbox::a_parent_delegates_a_workspace_change_to_a_child`, `embedding_readiness` |
| `agent.*` threads (retained sessions, fidelity) | `step_custom` kind `fabro.thread` (thread id, fidelity, resolution) once per native session | node, firing, attempt, session | durable | `fabro_readiness_blackbox`, `fabro_hooks_blackbox::full_fidelity_nodes_share_one_conversation_through_the_binary` |
| `agent.route.failover`, `prompt.failover` (C1): plan, per-target requests, decisions, accounting | `step_custom` kind `fabro.fallback.plan` (routes, notices: Petri's knowledge). Everything else is Pebble's, in `agent_activity`: `SessionStarted` (each route's provider and model), `RouteFailover` (from, to, attempt, the failed route's usage, cost and timings, typed error, `continuation`), `RouteFailoverStopped` (route, attempt, `ineligible`/`exhausted`, the error; published only when the plan named a fallback route), `AssistantMessage` (each answer's usage). The move and a server that did not start are also lines on the node's stderr. No `fabro.fallback.route`, `usage`, `failover` or `stop`, and no `metrics.custom.fallback.*` (removed 2026-09-12; see decision `pebble-events-are-the-agent-contract`) | node, firing, attempt, session | durable (a resumed node starts a new plan) | `fallback_events` (3), `fabro_fallback_blackbox` (15), `fabro_readiness_blackbox` (failover, then `RouteFailoverStopped` `exhausted` in the failure case), `embedding_readiness` |
| `agent.mcp.*` (C2): server and tool lifecycle | Pebble's own events in `agent_activity`: `McpServerReady` (server, tools, `startup_ms`), `McpServerFailed` (server, error, `startup_ms`), `McpServerDisconnected` (server, error; once per closed connection), and `ToolCallStarted`/`ToolCallCompleted` under `mcp__<server>__<tool>` (call id, `is_error`, `error_kind`: `timeout`, `unavailable`, `cancelled`, `denied`, ...). One `step_custom` kind for the fact Pebble cannot know: `fabro.mcp.unavailable` (server, error), a server Petri never named to Pebble because a secret its entry needs is unavailable. A server that did not start is also a line on the node's stderr. No `fabro.mcp.server` phases and no `fabro.mcp.tool` (removed 2026-09-12; see decision `pebble-events-are-the-agent-contract`) | node, firing, attempt, server name, tool call id | durable | `petri-fabro-steps::mcp` (8), `fabro_readiness_blackbox`, `embedding_readiness` (through `replay_run`); terminal and workspace reads in `fabro_mcp_blackbox` |
| `agent.skills.*` (C3): discovery and loading | `step_custom` kinds `fabro.skills` (the ordered directories with their sources, once per native session) and `fabro.skills.warning` (`malformed`, `unreadable`, `missing_directory`); Pebble's `SkillsDiscovered`/`SkillActivated` in `agent_activity` | node, firing, attempt, scope | durable | `fabro_readiness_blackbox`, `embedding_readiness`; raw-log reads in `fabro_skills_blackbox` |
| `agent.subagent.*` (C4): spawn, input, wait, close, child usage | `agent_activity` under the parent's session: `SubAgentSpawned {agent_id, depth, task}`, `SubAgentCompleted`, `SubAgentFailed`, `SubAgentClosed`; the child's own events under the child's session with `parent_session`; `attempt_finished.metrics.custom.pebble.subagents` (counts and per-session usage) | parent session, child session, stream sequence | durable | `fabro_subagents_blackbox::a_parent_delegates_a_workspace_change_to_a_child` (usage rebuilt from events equals the metric), `fabro_readiness_blackbox`, `embedding_readiness` |
| `agent.compaction.*` (C5): lifecycle and summary usage | Pebble's `CompactionStarted/Completed/Failed/Cancelled` in `agent_activity`; `step_custom` kind `fabro.compaction` (the summary call's usage and cost) once per compaction; `attempt_finished.metrics.custom.pebble.compaction_*` | node, firing, attempt, session | durable | `petri-fabro-steps::compaction::public_events_account_for_the_compaction_and_later_activity`, `fabro_readiness_blackbox`, `embedding_readiness` |
| `agent.*` error and warning | `attempt_finished` failure class; `step_custom` kinds `fabro.hook.warning` (an unenforceable ACP hook), `fabro.skills.warning`, `fabro.mcp.server {failed}` | node, firing, attempt | durable | `petri-fabro-steps::hooks::acp_tool_hooks_are_best_effort_with_explicit_warnings`, `fabro_mcp_blackbox` |
| `watchdog.timeout` | `stall_timeout {stall_timeout_ms, idle_ms}` beside `invocation_cancel_requested {reason: stall_timeout}` | invocation | durable (coordinator log) | `controls::an_idle_run_is_cancelled_by_the_watchdog` |
| `subgraph.started/completed` | `invocation_declared {call: ParentLink}`, `invocation_finished`, `execution_*` with `parent` | invocation, parent execution, firing, attempt, call slot | durable | `petri-execution::inspect::a_nested_invocation_keeps_its_own_context_and_parent_link`, `petri-fabro-acceptance::workflow` |
| local setup (`[run.prepare]`, `[run.clone]`) | the `run_prepare_N` stages' events; `step_custom` kind `fabro.checkout` (repository, commit, depth, files) on `start` | node, firing | durable | `embedding::the_milestone_workflow_runs_through_the_embedding_boundary`, `fabro_scenarios_blackbox` (checkout) |
| hook decisions (workflow points) | `host_note {kind: "hook"}` with the `HookReport` (point, decision, each hook's state, duration and usage, fail-open warnings); a report that ran no hook is silent | node, firing, attempt | durable | `embedding::a_hook_service_runs_each_hook_once_at_its_point`, `petri-fabro-steps::hooks` |
| hook-owned model requests, agent and tool activity, usage | `hook_activity {hook, activity}` per event of a hook's agent (a prompt hook has none), before the `host_note {kind: "hook"}` whose `hooks[].usage` sums the hook's requests, tool calls, tokens, cost and timings; the same for a point a step asks itself, beside its `fabro.hook` event; the stage's own `agent_activity` and `pebble.usage` count the stage's agent alone | node, firing, attempt, hook operation (point, hook name), the hook agent's session | durable | `petri-fabro-steps::hooks::an_agent_hooks_activity_is_kept_apart_from_the_stages_own` (live and replay), `agent_hooks_investigate_the_workspace_then_decide`, `prompt_hooks_evaluate_with_one_model_call_and_fail_open`, `prompt_hook_usage_records_a_failed_and_a_timed_out_request`, `an_agent_hook_timeout_stops_its_tool_before_failing_open` |
| hook decisions (points a step asks itself) | `step_custom` kind `fabro.hook` (`event` = `sandbox_ready`, `run_start` and `stage_start` on the root `start` stage; `parallel_start` on the fork node, `parallel_complete` on the fan-in; `pre_tool_use`, `post_tool_use`, `post_tool_use_failure` at the tool boundary; and the report), one per point that ran a hook, a child's under the parent stage | node, firing, attempt | durable | `fabro_readiness_blackbox`, `embedding_readiness` (counts per stage, two `block` decisions), `petri-fabro-steps::hooks::a_replacement_service_receives_every_step_driven_phase_once` |
| run-level hooks (`run_complete`, `run_failed`, `sandbox_cleanup`) | `host_note {kind: "hook"}` from the coordinator log's `RunNote` records, with no subject and the execution named: one per run-level point that ran a hook (`point` is `run_finished` or `scope_released`), in the order the points ran, before `run_finished`; the run's own end is `run_finished` | execution | durable (coordinator log) | `fabro_milestone_blackbox` (`assert_run_level_notes`: the reports through `replay_run` and `petri inspect` on a succeeded, a failed and a cancelled run, beside the hooks' effects) |
| local sandbox, retention, output references | `invocation_declared.sandbox` (the binding), the reported workspace in `petri inspect`, `output_line`, `artifact_recorded`, `blob://sha256/…` references in outputs; the acquisition progress lines are terminal-only | invocation, scope | durable (the binding and outputs), live-only (progress lines) | `inspect_cli`, `petri-fabro-steps::steps::large_command_output_is_offloaded_and_reads_back_logically` |
| budget pause and resume | `budget_paused {remaining_ms, pending_questions}`, `budget_resumed {remaining_ms}` from the driver's notes | node, firing, attempt | durable | `petri-driver::interview_budget::the_waiting_stage_pays_only_for_active_work` |
| pause and unpause | `run_paused`, `run_unpaused` from the coordinator's `RunPaused` and `RunUnpaused` records | run | durable; a resume starts paused when the last control recorded is a pause | `controls::pause_holds_admission_and_unpause_releases_it`, `controls::a_pause_survives_resume_and_holds_admission_until_unpaused`, `fabro_resume_blackbox::a_paused_run_stays_paused_across_resume_until_unpaused` |
| platform lifecycle, `checkpoint.*`, `git.*`, `pull_request.*`, product projections | not emitted; a host performs them in its `transition` and records `host_note {kind: "transition"}` | | | `embedding::adapters_run_in_order_and_checkpoint_work_follows_source_metadata` |

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
  `CompactionCompleted`, `CompactionFailed` and `CompactionCancelled`. Those
  events carry no usage, so the Fabro backend adds one `step_custom` per
  compaction with `kind = "fabro.compaction"` carrying the summary call's
  usage, which Pebble bills to the prompt that compacted; see
  `crates/fabro/FORMAT.md`, "Compaction".
- The ACP backend records what the external agent sends over ACP; tool calls
  the agent does not report are not observable.
- Agent facts are Pebble's; Petri adds run, invocation, node and attempt
  attribution and does not restate them.
- A backend envelope is a `step_custom` object with a string `kind` and an
  `event` **object**. A step's own payload may carry a string `event` (a
  hook report names its hook event); it stays a `step_custom`.
- A `for_each` child invocation's own stage carries no `branch_role` in its
  `meta`: the item index is known only at run time. The parent-side clone
  (`<template>#<index>`, a `parallel.branch` delegate) carries the member
  role, and the child's `meta.branch = {fork, target}` names the fork.
- An empty `for_each` list expands to one clone of the IR's placeholder item
  (`{"$placeholder": true}`, `ir::placeholder::PLACEHOLDER_ITEM_KEY`), so the
  fan-in still fires. The clone is no branch: `BranchMap` gives its nodes no
  role, `fork_started` and `fork_completed` carry zero branches, and no
  `branch_completed` is emitted. Its `node_expanded` clone and its own
  `visit_*` events (`<template>#0`, `synthetic: true` from the template) are
  the only trace of it.
