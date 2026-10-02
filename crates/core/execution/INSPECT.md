# The `petri inspect` document

`petri inspect --run-dir <directory> --json` reconstructs a run from its
store and prints one JSON document. The library form is
`execution::inspect::inspect_run`, over a `store::RunLogs` handle opened
for reading; `inspect_run_dir` opens a run directory and attaches the
interview receipt beside it. This file is the field contract for
`inspect_format_version` 3. Version 2 spelled every enum the document
carries from a record (`Admission`, `RouteDecision`, `EngineExit`,
`EntryPoint`, `Status`, `RunStatus`) with snake-case tags, as the logs do.
Version 3 reads the run through its store: `run_dir` became `locator`,
`run_key` names the run, and a log's `path` and `torn` are gone.

## Sources

The command reads only the run's store:

- the coordinator log: the run declaration (format version, run key, root
  invocation id), graph registrations, invocation and execution
  declarations, exits, results, cancellation requests, pause and unpause
  controls, and the run's finish.
- every registered graph, a blob by digest, byte-exact.
- one engine log per execution.
- `interviews.json` beside a run directory: the interview receipt the
  standalone host writes when the run had an interviewer
  (`execution::InterviewReceipt`). Optional; a file, not a record.

It decodes them under Petri's own rules (the format version on the run
declaration, the registered graph's digest, the coordinator state machine)
and replays each engine log through `engine::replay`. The loaded log must
be a byte-prefix of the log replay regenerates. Run context is derived on
every call and is never stored separately. A torn final line in a run
directory's log is the store's own business: dropped before the run is
read, the file left as found, and not reported.

The command does not start a step, acquire a sandbox, contact a provider,
take the run lease, or write to the store. It works on a run another
process still holds, on a machine with no provider reachable, and after
the source workflow file has changed or been deleted.

## Outcomes

- Exit 0 with a document whose `complete` is `true`: the run recorded its
  finish, every execution's log is stored, and every execution replayed
  exactly.
- Exit 1 with a document whose `complete` is `false`: the run is interrupted
  or its logs are short. `incomplete` lists every reason. The document still
  reports what the logs support.
- Exit 2 with an error on stderr and no document: the files do not support a
  trustworthy reconstruction. Causes: a missing or unreadable `run.json` or
  `coordinator.jsonl`, an unsupported format version, a complete record that
  does not decode, a coordinator log that does not replay, a registered graph
  that is missing or does not match its digest, an engine log with an
  undecodable record or version, an engine log that diverges from replay, or
  an `interviews.json` that is not an interview receipt.

The torn-line rule is the store's: only a final line that ends without a
newline is dropped, and the file is left as found. A newline-terminated line
that does not decode is corruption and is an error.

## Secrets

Values appear as the driver appended them, after masking. A secret reference
stays `{"$secret": "<name>"}` and a masked value stays `***`. The document
never carries a resolved secret. Child secret bindings show as names only. A
sensitive human-gate answer therefore appears twice: as the `$secret`
reference in `deliveries`, and as `***` wherever the step echoed the value.

## Offloaded values

A context value, an output, or a `context_updates` entry may be the string
`blob://sha256/<hex>` (with a `#json` suffix for a structured value) when a
step offloaded it to the run's output store. The document shows the
reference as the log recorded it and never resolves it; the bytes are under
`<run_dir>/blobs/<hex>` in the standalone runner's store. The field shapes
are unchanged: a reference is a string where the value would be.

## Fields

Ids are plain integers: invocation and execution ids, firing ids, node ids,
edge ids, generations, and attempts. Status words are the engine's lowercase
tags. Run statuses are `success`, `failed`, `cancelled`. Node statuses are
`success`, `partial_success`, `failure`, `skipped`, `cancelled`, `timed_out`.

### Top level

| Field | Meaning |
|---|---|
| `inspect_format_version` | This document's version. `3`. |
| `coordinator_format_version` | The run's own format, from its run declaration. |
| `locator` | Where the run lives, as its store names it: a run directory's path, or a database and an id. |
| `run_key` | The run's identity in its store and on its sandbox providers: the run id every sandbox of the run is labelled with. |
| `forked_from` | Where the run was forked from, or `null` on a run that started fresh: `source` (the source run's key), `execution` and `firing` (the position the source's records were kept up to), `rerun_last` (whether the position's firing runs again). See `FORK.md`. Additive in format version 3. |
| `complete` | `true` only when `incomplete` is empty. |
| `status` | The recorded run status, or `null` until the run finished. |
| `incomplete` | Every reason `complete` is `false`, in the order found. |
| `paused` | Whether the last recorded run control was a pause (`RunPaused` with no later `RunUnpaused`). A `petri resume` of such a run holds admission until an unpause. Additive in format version 1. |
| `notes` | Every run-level note, in record order, from the coordinator log's `run.note.recorded` records: `execution` (whose driver ran the point), `kind`, `payload`. A `hook` note is a run-level hook report (`payload.point` is `run_finished` or `scope_released`), the same shape as a firing's `hook` note. The summary prints their count. Additive in format version 1. |
| `root` | `invocation`, `final_execution` (the execution the root's result names, or `null`), `latest_execution` (the root's newest execution). |
| `middleware_chain` | The configured decision middleware, by key. |
| `graphs` | Every registered graph digest, sorted. |
| `invocations` | Every invocation, in id order. The root is first. |
| `executions` | Every execution, in id order, which is declaration order across the run. |
| `interviews` | The interview receipt (`version`, `questions`, `errors`, `script`) as the host wrote it, or `null` when the run had no interviewer. `questions` are in the receipt order the dispatcher defines: by invocation path, then invocation, execution, firing, occurrence, and ask (the root invocation's questions first, then each nested invocation's in path order; within an invocation, the order the run asked them), whatever order the answers arrived in. `inspect` passes the receipt through and never re-sorts it. Sensitive answers appear only as `$secret` references. |

### Invocation

| Field | Meaning |
|---|---|
| `invocation` | The id. |
| `status` | `declared`, `running`, or `finished`. |
| `parent` | `null` on the root. Otherwise the call that declared it: `invocation`, `execution`, `firing`, `attempt`, `slot`. |
| `children` | Invocations whose call site lies in one of this invocation's executions. |
| `graph` | The digest of the graph it runs. |
| `context` | The context it was declared with. |
| `secrets` | `mode` (`none`, `inherit`, `explicit`) and `names` (the explicit binding names). |
| `sandbox` | `isolated` or `inherited`. |
| `cancel_requested` | Whether the coordinator recorded a cancel request. |
| `cancel_reason` | Why, when the requester said: `{"kind": "interrupt"}` (Ctrl-C), `{"kind": "control"}` (a run control), or `{"kind": "stall_timeout", "stall_timeout_ms", "idle_ms"}` (the watchdog). Absent for a plain cancel. Additive in format version 1. |
| `executions` | Every execution, in order. Each after the first followed a restart. |
| `final_execution` | The execution the result names, or `null`. |
| `result` | `null` until finished. Otherwise `status`, `failure`, `final_execution`, `output`, `context` (the final execution's `kv`, as recorded). |

### Execution

| Field | Meaning |
|---|---|
| `execution`, `invocation` | Ids. |
| `execution_index` | Position within the invocation, from 0. |
| `predecessor`, `successor` | Neighbouring executions in the invocation, or `null`. |
| `status` | `finished` (terminal exit recorded), `restarted` (restart exit recorded), or `incomplete`. |
| `entry_node` | The node a restart successor starts at, or `null` for the graph's own entries. |
| `start_context` | The context the execution started with. |
| `exit` | The exit the coordinator recorded, or `null`. `{"kind": "terminal", "status"}` or `{"kind": "restart", "edge", "target", "target_name", "source_firing"}`. |
| `log` | `records`, `replay` (`verified`, `prefix`, or `missing`). |
| `engine` | The replayed state, or `null` when there is no log. |
| `children` | Invocations whose call site is a firing of this execution. |

`log.replay` is `verified` when replay regenerated exactly the log's records,
`prefix` when the log is a byte-prefix of the regenerated log (a crash landed
between an external append and the flush of its derived records), and
`missing` when nothing is stored for the execution. `prefix` and `missing`
make the run incomplete.

### Engine

| Field | Meaning |
|---|---|
| `started`, `finished`, `cancelled` | Replayed state flags. `cancelled` means the root cancel scope was cancelled. |
| `exit` | The exit replay derived. Same shape as the execution's `exit`. A disagreement between the two is listed in `incomplete`. |
| `folded_status` | The run status folded from node outcomes under the graph's completion policy. |
| `errors` | Engine errors, rendered. |
| `context` | `kv` and `nodes`. |
| `history` | Every final firing record in completion order. |
| `attempts` | Every `StepFinished` record, final or not, in log order. |
| `routes` | Every applied route, in log order. |
| `deliveries` | Every control the host delivered into a firing, in log order. |
| `live` | Firings still live when the log ends. Empty for a finished execution. |

`context.nodes` is keyed by node instance name (`build`, `build#2`). Each
record has `status`, `success_like`, `failure`, `output`, `generation`, and
`attempts`. This is the engine's node-instance record: it reflects only the
final attempt of the latest generation to complete.

`history` entries have `firing`, `node`, `node_id`, `generation`, `attempt`,
`status`, `failure`, `output`, and `context_updates`. One entry per firing.

`attempts` entries have `seq` (the record's position in the log), `firing`,
`node`, `generation`, `attempt`, `status`, `failure`, and `final`. Retries
show up here and nowhere else: a non-final attempt has `final: false`, and
its `context_updates` never reached `kv`.

`routes` entries have `seq`, `firing`, `node`, `kind` (`edge` or `none`),
`group`, `edge`, and `target` (the node the route led to). Together with
`history` they show routing across repeated visits to one node.

`deliveries` entries have `seq`, `firing`, `node`, `kind` (`deliver`,
`cancel`, `kill`, `other`), and `payload` (the delivered value, for `deliver`).
A sensitive answer was delivered as `{"$secret": "answer:<question>"}` and is
shown as that reference. The step's own echo of the value, in its output or
`context_updates`, was masked to `***` before it was appended.

`live` entries have `firing`, `node`, `generation`, `attempt`, `started`,
`awaiting_retry`, and `cancelling`.

## Versioning

`inspect_format_version` is bumped when a field changes shape or meaning.
Adding a field that leaves every existing field intact does not bump it.
Readers must tolerate added fields.
