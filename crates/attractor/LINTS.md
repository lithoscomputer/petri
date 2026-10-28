# Fabro's lint rules in Petri

Fabro checks a workflow with the rules in
`lib/components/fabro-validate/src/rules/` before it creates a run. Plan item
P1.4 (`.ai/plans/fabro-integration.md`) audits those rules so that Fabro can
hand the check to Petri: a rule about the language is a diagnostic of the
Attractor lowering, raised by `petri check` and by every host that embeds
Petri; a rule about Fabro's settings would stay in Fabro's create handler.
This page records the result for each of the 38 rule names in that
directory, read against the rule's source and tests.

Classifications:

- **Covered**: the lowering already raised an equivalent diagnostic. The Petri
  code is named.
- **Language**: the rule was ported into the lowering in this item
  (`crates/attractor/frontend/src/lower/lints.rs`, and `threads.rs` for one
  clause), with a stable code, a span and a hint.
- **Platform**: the rule checks Fabro's settings, not the language. None of
  the 38 does.
- **Obsolete**: the construct does not exist in Petri's dialect.
- **Covered at admission**: the three catalog rules. They need the model
  catalog, which the frontend does not have and the runtime does: with the
  `PebbleClient` capability installed, `Runtime::check` resolves every
  agent and prompt node's model against it
  (`attractor_steps::admission`, FORMAT.md "Model resolution at
  admission") and a selector or chain entry it cannot resolve is
  `attractor.model.unknown` (a malformed or provider-keyed table is
  `attractor.model.fallbacks`). Nothing stays in Fabro for them (decision 4
  of the plan's review).

Counts: 24 covered, 9 language (7 whole rules and one clause each of
`backend_valid` and `for_each_contract`), 3 covered at admission, 1
obsolete, 1 helper file that is not a rule, 0 platform.

A diagnostic's severity is Fabro's unless the notes say otherwise. Where Petri
is stricter the notes say so, and `crates/fabro/acceptance/CONTRACT.md` lists
the differences that are tested. `crates/attractor/FORMAT.md` documents every
code; `crates/attractor/frontend/tests/lowering.rs` exercises each row below
(the `condition_syntax` case is in `tests/conditions.rs`).

| Fabro rule | Classification | Petri code, or home and reason | Notes |
|---|---|---|---|
| `all_conditional_edges` | Language | `attractor.all_conditional_edges` (error) | On the node. Hint: add one unconditional edge as the fallback. |
| `auto_status_deprecated` | Covered | `deprecated.auto_status` (warning) | Fabro's three messages: ignored beside `on_failure`, the `succeed` spelling, no effect unless true. |
| `backend_valid` | Covered, one clause Language | `attractor.bad_backend` (unknown value, `cli` included), `attractor.prompt_backend`, `unsupported.acp_command`, `attractor.acp_both`, `attractor.bad_acp_command`, `attractor.bad_acp_config`; ported: `attractor.acp_requires_command`, `attractor.acp_api_only_attributes` (errors) | Petri resolves the backend and the ACP agent from the node or the graph; Fabro's rule reads node attributes only. A `model` a stylesheet wrote is not the node's and is not refused. The lowering checks that `acp.command` splits into a non-empty command line and that `acp.config` is a JSON object with a non-empty `command`; the rest of the stdio JSON shape is the step's to check at run time. |
| `command_requires_script` | Covered | `attractor.command_requires_script` (error) | |
| `condition_syntax` | Covered | `attractor.condition.syntax` (error) | Petri parses conditions in more detail at load: `attractor.condition.regex`, `.non_numeric`, `.outcome_op`, `unsupported.outcome_value`. |
| `direction_valid` | Language | `attractor.bad_rankdir` (warning) | `rankdir` stays a layout attribute; only a value outside `TB`, `LR`, `BT`, `RL` is diagnosed. |
| `edge_target_exists` | Covered | `attractor.undeclared_node` (error) | |
| `exit_no_outgoing` | Covered | `attractor.exit_has_outgoing` (error) | One per edge, on the edge. |
| `fidelity_valid` | Covered | `attractor.bad_fidelity` | Error in Petri, warning in Fabro: a mode the step cannot resolve does not run. Node, edge and the graph's `default_fidelity`. |
| `for_each_contract` | Covered, one clause Language | `attractor.for_each.source`, `.template_edges`, `.target`, `.nested`; ported: `attractor.for_each.not_parallel` (error) | |
| `freeform_edge_count` | Covered | `attractor.freeform_edge_count` (error) | |
| `goal_gate_has_retry` | Covered | `attractor.goal_gate_without_target` (warning) | Petri warns when no named target exists; Fabro when none is named. A superset, with `attractor.retry_target_not_found` naming the bad target. |
| `import_error` | Covered | `attractor.import` (error) | Fabro's message, on the placeholder. Fabro's second clause (an import with no base directory) does not arise: Petri resolves every import beside its file. |
| `inert_attribute` | Language | `attractor.inert_attribute` (warning) | Fabro's table of handler-specific attributes. Recorded difference: a prompted `tripleoctagon` is a prompt node in Petri and reads `output_schema` and `output_retries`, so they are not inert on it. |
| `script_prompt_conflict` (raised by `inert_attribute.rs`) | Language | `attractor.script_prompt_conflict` (error) | |
| `join_policy_removed` | Obsolete | `attractor.unknown_attribute` (error) | `join_policy` is not in the dialect's attribute tables. The hint says to remove it: a parallel node always waits for every branch. |
| `model_support` | Covered at admission | `attractor.model.unknown` | The helper the two catalog rules share (unknown model, unknown provider). Raised by `Runtime::check` with a model client, not by the lowering; `crates/attractor/steps/tests/admission.rs`. |
| `node_model_known` | Covered at admission | `attractor.model.unknown` | A node's `model` and `provider`, and a provider named alone whose catalog row has no default model. |
| `on_failure_valid` | Covered | `attractor.bad_on_failure` (error); on an edge, `attractor.unknown_attribute` (error) | Fabro warns for `on_failure` on an edge; Petri refuses every attribute the edge table does not list. Petri also accepts `partially_succeed`, with the `attractor.petri_extension` warning (see FORMAT.md). |
| `orphan_custom_outcome` | Covered | `attractor.all_conditional_edges` (error) | A node with an `outcome=` condition and no unconditional edge has only conditional edges, so the ported error covers it. Fabro's rule is a warning; the error it always accompanies wins. |
| `parallel_branch` | Helper, not a rule | | The branch analysis the two rules below share. Petri's is `threads::is_branch_first` and the fork-edge check in `threads::edge_payload`. |
| `parallel_branch_inert_attribute` | Covered | `attractor.parallel_branch_inert_attribute` (warning) | The `thread_id` clause on a branch's first node was covered; the `fidelity="full"` clause (a branch runs at most at `summary:high`) and the checks on a fork edge (`threads::check_fork_edge`, which the routing pass skipped) were added under the same code in this item. Petri warns on a node with any fork edge into it; Fabro only when every edge into it is a fork edge. |
| `prompt_on_llm_nodes` | Covered | `attractor.prompt_missing` (warning) | Petri warns whenever an agent or prompt node has no `prompt`, even when a label stands in; Fabro is quiet when a label exists. Warning only. |
| `random_selection_no_conditions` | Covered | `attractor.random_with_conditions` (error) | |
| `reachability` | Covered | `attractor.unreachable_node` (error), `attractor.exit_unreachable` (error) | Error in Petri, warning in Fabro: a node that cannot run is a structural mistake (FORMAT.md, "Syntax both runners reject"). |
| `reserved_keyword_node_id` | Language | `attractor.reserved_keyword_node_id` (warning) | Fabro's list, case-insensitive. `strict` and `if` parse as ids; `graph`, `node`, `edge`, `subgraph` are statements in both parsers and reach the check only when quoted. |
| `retry_target_exists` | Language | `attractor.retry_target_not_found` (warning) | `retry_target` and `fallback_retry_target`, on a node or the graph. |
| `script_absolute_cd` | Language | `attractor.script_absolute_cd` (warning) | `cd` at a word boundary, then whitespace, then `/`. |
| `selection_valid` | Covered | `attractor.bad_selection` | Error in Petri, warning in Fabro. Node and graph. |
| `start_no_incoming` | Covered | `attractor.start_has_incoming` (error) | |
| `start_node` | Covered | `attractor.no_start`, `attractor.multiple_starts` (errors) | Petri also reads `type=start`. |
| `stdin_source_valid` | Covered | `attractor.bad_stdin_source` (error) | An empty key. Fabro also refuses a non-string value; Petri reads any scalar as the text it was written as. |
| `stylesheet_model_known` | Covered at admission | `attractor.model.unknown` | A stylesheet writes `model` on the node before the lowering reads it, so the pass sees the styled value. |
| `stylesheet_syntax` | Covered | `attractor.stylesheet.syntax` (error) | Petri also warns on a property the stylesheet cannot set, `attractor.stylesheet.unknown_property`. |
| `terminal_node` | Covered | `attractor.no_exit`, `attractor.multiple_exits` (errors) | Petri also reads `type=exit`. |
| `thread_id_requires_fidelity_full` | Covered | `attractor.thread_id_requires_fidelity_full` (warning) | Node, edge and the graph's `default_thread`. A fork edge or a branch's first node gets `attractor.parallel_branch_inert_attribute` instead, as in Fabro. |
| `type_known` | Covered | `attractor.unknown_type` | Error in Petri, warning in Fabro: Fabro runs an unknown `type` as an agent; Petri refuses it. An unknown shape is the `attractor.unknown_shape` warning and runs as an agent, as in Fabro. |
| `unresolved_file_ref` | Covered | `attractor.file_not_found` (error) | `@file` in a prompt or the goal is read at load; a missing file is the error. Fabro's rule fires on an `@` left after its resolution. |

## Recorded differences

- A prompted fan-in reads `output_schema` and `output_retries` in Petri, so
  `attractor.inert_attribute` does not fire for them there (Fabro's table
  would).
- `attractor.acp_api_only_attributes` skips an attribute the model stylesheet
  wrote: Petri refuses only what the author wrote on the node. Whether
  Fabro's rule sees a stylesheet's value depends on when Fabro applies the
  stylesheet, which this audit did not pin down.
- The Attractor fixtures `test/attractor/*.dot` in the compatibility corpus
  have nodes with only conditional edges. Fabro's validator refuses them
  and now so does Petri; `crates/fabro/acceptance` counts the ported errors
  as specific rejections, and `crates/fabro/corpus/REPORT.md` shows the new
  classes.
