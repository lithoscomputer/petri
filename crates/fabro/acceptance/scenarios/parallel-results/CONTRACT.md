# Contract: workflow-visible parallel branch results

Status: captured 2026-09-06 for task 3. Task 6 implements it. Task 18 completes
the comparison matrix.

This document defines what a Fabro workflow sees after a parallel node
(`shape=component`) completes and its fan-in (`shape=tripleoctagon`) runs. It
is the target for `context.parallel.results`, for `stdin_source` reads of that
key, and for the fan-in node's output under Petri.

Sources, in order of authority:

1. Fabro source at revision `05ebd0fd1beec214b558f4b478e36bd08b507dc7`. Paths
   are relative to the Fabro repository root. Line numbers are from that
   revision.
2. The Fabro capture in `fabro-reference/` (see `PROVENANCE.md`).
3. The consumer: `helper/merge_branches.py`, function `branch_values`.

Where Petri's engine spec and this contract meet, Petri keeps its core
invariants. Branch results ride the fan-in's output and tokens. They do not
change the global context merge rules.

## 1. The result list

`parallel.results` is a JSON array with one entry per branch, in branch order.
Branch order is dispatch order, never completion order:

- Static branches: the parallel node's outgoing edges in graph order
  (`handler/parallel.rs`, `build_branch_plan`, lines 162 to 183:
  `edges.iter().enumerate()`).
- `for_each` branches: the source array order (lines 186 to 297:
  `items.iter().enumerate()`).
- Results are awaited in dispatch order so the list stays aligned with the
  edge or item order regardless of which branch finishes first (lines 632 to
  634, comment "Awaiting in dispatch order keeps `results` aligned").

Evidence: in `fabro-reference/raw/events.jsonl`, `verifier_b` started before
`verifier_a` (`parallel.branch.started` events, index 1 then index 0), yet
`parallel.completed` lists `verifier_a` (index 0) before `verifier_b`
(index 1).

## 2. One branch envelope

Type: `lib/foundation/fabro-types/src/parallel.rs`, `ParallelBranchResult`,
lines 9 to 21. Serialized with serde, so field names are exact.

| Field | Type | Required | Meaning |
| --- | --- | --- | --- |
| `id` | string | yes | The branch target node id. For `for_each`, every entry carries the template node id; the items are told apart by `index` and `item_label`. Source: `parallel.rs` line 585, `id: target_id.clone()`. |
| `index` | integer, zero-based | yes for new results | The branch position: outgoing-edge position for static branches, item position for `for_each`. The type marks it `Option` only to read records written before indexes existed (`parallel.rs` types, lines 11 to 14). New results always set it (line 586, `index: Some(branch_index)`). |
| `item_label` | string | only for `for_each` | Absent (not `null`) for static branches: serde `skip_serializing_if = "Option::is_none"` (types line 16). See section 3. |
| `status` | string | yes | See section 4. |
| `context_updates` | object | yes, may be empty | See section 5. |

The Fabro capture for a static two-branch fan-out
(`fabro-reference/raw/find-parallel_results.json`):

```json
[
  {
    "id": "finder_a",
    "index": 0,
    "status": "succeeded",
    "context_updates": {
      "command.output": "{\"context_updates\":{\"output.finder\":{...}}}",
      "output.finder": { "candidates": [ { "file": "src/pager.py", ... } ] }
    }
  },
  {
    "id": "finder_b",
    "index": 1,
    "status": "succeeded",
    "context_updates": { "command.output": "...", "output.finder": { ... } }
  }
]
```

Key order inside an envelope is not part of the contract. Fabro's event
payload and its dump file order the keys differently; both are the same value.

## 3. Item labels

`item_label` exists only for `for_each` results.

- Take the item's `name` string, else its `label` string. Sanitize it:
  strip ANSI escapes and control or bidi characters, trim, cap at 80
  characters with a trailing ellipsis (`lib/foundation/fabro-util/src/text.rs`,
  `sanitize_display_label`, lines 23 to 38; `MAX_DISPLAY_LABEL = 80`).
- When neither key yields a non-empty label (scalar item, blank name, nothing
  printable), the label is the index as decimal text (`parallel.rs`,
  `item_label`, lines 133 to 145: `unwrap_or_else(|| index.to_string())`).
- The label is always derived from the full item, before any size demotion
  (`parallel.rs` line 56 comment).

Fabro tests that pin this: `for_each_item_label_uses_name_then_label_then_index`
(line 1633) and `for_each_item_label_falls_back_when_nothing_printable_survives`
(line 1647).

## 4. Status vocabulary

`status` serializes Fabro's `StageOutcome`
(`lib/foundation/fabro-types/src/outcome.rs`, lines 26 to 31 and 53 to 59):

| Value | Meaning |
| --- | --- |
| `"succeeded"` | The branch target succeeded, or it failed under `on_failure="succeed"` and was promoted before collection (`parallel.rs` line 577, `outcome.apply_on_failure`). |
| `"partially_succeeded"` | The target itself reported partial success. A nested parallel target can produce this. |
| `"failed"` | The branch failed after retries, was cancelled, hit a missing target, panicked, or its task did not join. `retry_requested` is not serialized. |
| `"skipped"` | Reserved by the enum. Fabro's parallel handler does not produce it for a branch. |

Petri's own outcome tags (`success`, `failure`, `partial`) are not this
vocabulary. Petri's current output uses `"status": "success"`; the contract
requires `"succeeded"`.

## 5. `context_updates`

The branch's own context changes, as a flat object keyed by Fabro context key.
Built by `branch_context_updates` (`parallel.rs`, lines 793 to 804):

1. Start with the branch outcome's `context_updates` (what the node reported,
   such as `output.<node>`, `command.output`, or a routing directive's keys).
2. Extend with the public context diff of the branch context against the fork
   snapshot: every key whose value differs from the snapshot, minus engine
   internal keys (`context.rs`, `context_diff_public`, lines 231 to 239;
   `is_engine_internal_key`, lines 109 to 114: prefixes `internal.`, `graph.`,
   `thread.`, `current.`).
3. On duplicate keys the diff wins.

Consequences the consumer relies on:

- Two branches that write the same key (`output.finder` here) each keep their
  own value. Nothing merges them, and no branch's value reaches the parent or
  a sibling context. The fork snapshot is taken once, before dispatch
  (`parallel.rs` line 417, `context.snapshot()`), so late-starting branches
  see the same snapshot.
- A command branch with `output_schema="routing"` contributes the directive's
  `context_updates` (here `output.finder`) plus `command.output`.
- A failed branch has an empty `context_updates` object (`failed_branch_result`,
  lines 886 to 903). It still has `id`, `index`, `item_label`, and `status`.

The parent does not apply branch updates to its own context. After the
fan-out in the capture, the exit-time context has no `output.finder` and no
`output.verifier` key (`reference.json`, `projection.context`).

Value offloading: Fabro replaces large leaves with `blob://sha256/...`
references in event payloads (`parallel.rs` lines 721 to 733). In the capture
even the short `command.output` strings appear as blob references in
`events.jsonl`, while the `dump` and the `stdin` the merge command received
hold the inline strings. The workflow-visible value is the inline string.
Readiness item 4 covers Petri's offload threshold; this contract only requires
that the consumer sees the logical value.

## 6. `parallel.branch_count`

An integer, the number of branches dispatched, equal to the result list length
(`parallel.rs` lines 736 to 741). Published together with `parallel.results`
as the parallel node's own context updates. Success and failure counts appear
only in the `parallel.completed` event (`success_count`, `failure_count`),
never in context.

## 7. Aggregate status and empty lists

`aggregate_status` (`parallel.rs`, lines 905 to 927):

| Branch statuses | Parallel node status |
| --- | --- |
| all `succeeded` | `succeeded` |
| all `failed` | `failed` (the node fails with "All parallel branches failed"; `jump_to_node` is cleared) |
| any other mix | `partially_succeeded` |
| empty `for_each` list | `succeeded`; the node still jumps to the join (`find_join_node` is called with the template target, lines 703 to 713) |
| empty static list | `partially_succeeded` (Petri's frontend rejects this shape at lowering with `attractor.parallel.no_branches`, so it is unreachable under Petri) |

`parallel.results` is `[]` and `parallel.branch_count` is `0` for an empty
list. A fan-in with no `parallel.results` in context fails with
`No parallel results to join` (`handler/fan_in.rs`, `validated_branch_count`,
lines 95 to 102). A `parallel.results` that does not deserialize as
`Vec<ParallelBranchResult>` fails with `Invalid parallel results`.

## 8. What the fan-in adds

A prompt-less fan-in is a barrier: it contributes no context updates and
succeeds with the note `Joined N parallel branches` (`fan_in.rs`, lines 103 to
116). It does not reshape the list. Under Petri the fan-in's output is the list
above, and `stdin_source="context.parallel.results"` on a later command reads
that output.

## 9. The regression this contract closed

When this contract was captured, Petri `9cea20d` emitted, per branch,
`{ "id", "status", "output" }` with Petri's own status tag and the raw step
output. It had no `index`, no `context_updates`, no Fabro status vocabulary,
and no `parallel.branch_count`, so a consumer of the branch results found no
findings. `helper/merge_branches.py` now fails on any envelope that breaks
sections 1 and 2, and the `contract_*` tests in `fabro_blackbox.rs` compare
Petri's envelopes with the Fabro capture.
