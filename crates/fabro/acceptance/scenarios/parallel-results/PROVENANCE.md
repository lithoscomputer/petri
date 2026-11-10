# Provenance: parallel-results scenario

This directory holds the regression for Fabro parallel branch results
(readiness item 3, black box phase 2) and the reference capture from the
pinned Fabro. Everything here is test data. Nothing in it is linked into
Petri.

## Helper: `helper/merge_branches.py`

Written for this scenario. It stands in for a review workflow's merge step:
it reads `context.parallel.results` from stdin, checks each branch envelope
against `CONTRACT.md` (function `branch_values`), pools the finder candidates,
pairs them with the verdicts, and writes `runtime/report.md`. A malformed
envelope fails the command.

Until 2026-11-10 this scenario ran a copy of Conveyor's `code_review.py`.
That copy and the capture taken with it were replaced because the helper
carried prompt text this repository cannot redistribute. The contract under
test did not change.

## Scenario workflow: `workflow.fabro`

- `finder_a` and `finder_b` are commands in place of review agents. Each
  writes one distinct candidate under the same context key `output.finder`.
- `find_join` joins them; `merge_find` reads `context.parallel.results` and
  routes on `run_verify`.
- `verifier_a` and `verifier_b` each write one verdict under
  `output.verifier`; `merge_verify` reads their results the same way.
- `report` prints `runtime/report.md` so the harness can read it from the
  command output after the workspace is released.

These are static branches, not `for_each`, because Petri's lowering accepts
only agent nodes as `for_each` templates.

Input: `helper` (absolute path to `helper/merge_branches.py`).

## Fabro reference: `fabro-reference/`

| Field | Value |
| --- | --- |
| Fabro repository | `https://github.com/fabro-sh/fabro` |
| Revision | `05ebd0fd1beec214b558f4b478e36bd08b507dc7` (the compatibility target in `crates/fabro/corpus-pin.txt`) |
| Build | `scripts/fabro-provision.sh` (`cargo build --locked -p fabro-cli` from the fetched corpus checkout); version string `fabro 0.355.0-nightly.0 (05ebd0f 2026-09-13 debug)` |
| PATH `fabro` | not used |
| Capture date | 2026-11-10 |
| Capture script | `fabro-reference/capture.sh`, run as `capture.sh <fabro> <root> <workdir> workflow.fabro -I helper=<workdir>/helper/merge_branches.py` |

Files:

- `reference.json`: the differential projection of a live run of the pinned
  binary, recorded with `PETRI_FABRO_REFERENCE_RECORD=1` by
  `fabro_differential.rs`.
- `raw/events.jsonl`: `fabro events --json` for the capture run.
- `raw/find-parallel_results.json`, `raw/verify-parallel_results.json`: the
  `parallel_results.json` files `fabro dump` wrote for the two fan-outs.
- `raw/report.md`: the helper's report.
- `raw/run-stderr.txt`, `raw/run.log`: the CLI progress and the worker log.
- `raw/probe-parallel_results.json`, `raw/probe-inspect-stdout.txt`: the
  capture for the original `probe.fabro` from
  `.ai/reviews/fabro-readiness-2026-09-06` (branches `a` and `b`), taken on
  2026-09-13.

In the raw files the run id is replaced by `<RUN_ID>` and the absolute
working directory by `<WORKDIR>`.

The Fabro server ran with a private `HOME`, dev-token auth, a loopback TCP
port, and a placeholder `OPENAI_API_KEY` in its vault. The placeholder was
required because run creation resolves a model provider even for a graph with
no agent nodes. No request left the machine.
