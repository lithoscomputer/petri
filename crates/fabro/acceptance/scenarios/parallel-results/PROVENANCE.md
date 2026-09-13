# Provenance: parallel-results scenario

This directory holds the first failing regression for Fabro parallel branch
results (readiness item 3, black box phase 2) and the reference capture from
the pinned Fabro. Everything here is test data. Nothing in it is linked into
Petri.

## Pinned Conveyor helper: `helper/code_review.py`

The helper is copied unchanged from the Conveyor repository.

| Field | Value |
| --- | --- |
| Source repository | `git@github.com:lithoscomputer/conveyor.git` (local checkout `~/p/lithoscomputer/conveyor`) |
| Path in source repository | `.fabro/workflows/code-review/scripts/code_review.py` |
| Checkout revision (`git rev-parse HEAD`) | `5134b2c0607eb556882faf4b3f7bd53ef204bc74` (branch `main`) |
| Last commit touching the file | `8e5187cf2cc735b3b1b118ec040a90070228b251` (2026-08-06) |
| Working tree state of the file | clean (`git status --porcelain` printed nothing for the path) |
| SHA-256 | `58ab01ff37f0bbb7c0f94f9d634b3e5c54f66f36ea43750d81616e80b8201d3f` |

The Conveyor graph checks that hash before calling the helper (node `prepare`
in `.fabro/workflows/code-review/workflow.fabro`). `workflow.fabro` in this
directory repeats that check, so a modified helper fails the scenario.

The function under test is `parallel_values(raw_results, jobs_count, output_key)`
(helper lines 475 to 499). It requires each branch envelope to be an object
whose `index`, when present, equals its position and whose `context_updates`
is an object holding `output_key`.

## Scenario workflow: `workflow.fabro`

The graph reproduces the shape of the Conveyor merge nodes:

- `prepare` is the Conveyor `prepare` node verbatim (hash check, then
  `code_review.py prepare --level ... --target ...`).
- `merge_scope`, `merge_find`, and `merge_verify` call the helper the same way
  the Conveyor graph does, with the same `stdin_source` values
  (`context.output.scope` and `context.parallel.results`).
- Commands replace the agents. `scope` writes `output.scope`; `finder_a` and
  `finder_b` each write one distinct candidate under `output.finder`;
  `verifier_a` and `verifier_b` each write one verdict under `output.verifier`.
  These are static branches, not `for_each`, because the current Petri lowering
  only accepts agent nodes as `for_each` templates.
- `setup` seeds the empty run workspace: it copies the helper to the path the
  helper hard-codes and creates one Git commit for `review_head()`.
- `report` prints `runtime/report.md` so the harness can read it from the
  command output after the workspace is released.

Inputs: `helper` (absolute path to `helper/code_review.py`), `level` (`high`),
`target` (`review-fixture`).

## Fabro reference: `fabro-reference/`

| Field | Value |
| --- | --- |
| Fabro repository | `https://github.com/fabro-sh/fabro` |
| Revision | `05ebd0fd1beec214b558f4b478e36bd08b507dc7` (the compatibility target, `crates/fabro/corpus-pin.txt`) |
| Where it was fetched from | `main` (the merge of fabro-sh/fabro#867, 2026-09-13); `scripts/corpus-fetch-fabro.sh` fetches it at the pin |
| Build | `cargo build --locked -p fabro-cli` from the fetched corpus checkout through `scripts/fabro-provision.sh`; version string `fabro 0.355.0-nightly.0 (05ebd0f 2026-09-13 debug)` |
| PATH `fabro` | not used |
| Capture date | 2026-09-13 (first captured 2026-09-06 at `b6482910`, before Fabro moved onto Pebble; the re-capture at this revision produced the same branch envelopes, stage order, report and final context) |
| Capture script | `fabro-reference/capture.sh` |

Files:

- `raw/events.jsonl`: `fabro events --json` for the scenario run. The
  absolute working directory was replaced by `<WORKDIR>` after capture; it
  appears only in the `sandbox.initialized` payload and the worker log.
- `raw/find-parallel_results.json`, `raw/verify-parallel_results.json`: the
  `parallel_results.json` files `fabro dump` wrote for the two fan-outs.
- `raw/report.md`: the helper's report as printed by the `report` node.
- `raw/run-stderr.txt`, `raw/run.log`: the CLI progress and the worker log.
- `raw/probe-parallel_results.json`, `raw/probe-inspect-stdout.txt`: the same
  capture for a two-branch probe graph (a `component` node fanning out to
  static command branches `a` and `b`, each writing `output.finder.finding`),
  a reconstruction of the original `probe.fabro` from
  `.ai/reviews/fabro-readiness-2026-09-06`.
- `normalized.json`: the branch envelopes, stage order, report, and final
  context with run ids, blob references, and paths replaced by placeholders.

The Fabro server ran with a private `HOME`, dev-token auth, a loopback TCP
port, and a placeholder `OPENAI_API_KEY` in its vault. The placeholder was
required because run creation resolves a model provider even for a graph with
no agent nodes. No request left the machine.
