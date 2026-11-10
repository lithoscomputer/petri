"""Consume Fabro parallel branch results the way a real review workflow does.

usage:
  merge_branches.py merge find     read the finder fan-out from stdin
  merge_branches.py merge verify   read the verifier fan-out from stdin
  merge_branches.py report         print runtime/report.md

Each merge reads `context.parallel.results` from stdin and checks every
branch envelope against the contract in ../CONTRACT.md: an object whose
`index`, when present, equals its position, and whose `context_updates` is an
object holding the fan-out's output key. A malformed envelope fails the
command, so a shape regression cannot pass as an empty result.

State lives in runtime/state.json under the working directory. The last
stdout line of a merge is a routing directive with the merge's counts.
"""

import json
import sys
from pathlib import Path

RUNTIME = Path("runtime")
STATE = RUNTIME / "state.json"
REPORT = RUNTIME / "report.md"
OUTPUT_KEYS = {"find": "output.finder", "verify": "output.verifier"}


def branch_values(results, output_key):
    if not isinstance(results, list):
        sys.exit(f"parallel.results is not a list: {results!r}")
    values = []
    for position, branch in enumerate(results):
        if not isinstance(branch, dict):
            sys.exit(f"branch {position} is not an object: {branch!r}")
        if "index" in branch and branch["index"] != position:
            sys.exit(f"branch {position} has index {branch['index']!r}")
        updates = branch.get("context_updates")
        if not isinstance(updates, dict) or output_key not in updates:
            sys.exit(f"branch {position} has no {output_key} in context_updates")
        values.append(updates[output_key])
    return values


def load_state():
    return json.loads(STATE.read_text()) if STATE.is_file() else {}


def save_state(state):
    RUNTIME.mkdir(exist_ok=True)
    STATE.write_text(json.dumps(state, indent=2, sort_keys=True) + "\n")


def emit(**updates):
    print(json.dumps({"context_updates": updates}, separators=(",", ":")))


def merge_find(values):
    candidates = [c for value in values for c in value["candidates"]]
    locations = sorted({(c["file"], c["line"]) for c in candidates})
    save_state({"candidates": candidates})
    print(f"Pooled {len(candidates)} candidates into {len(locations)} locations")
    emit(candidate_count=len(candidates), run_verify=bool(candidates))


def merge_verify(values):
    state = load_state()
    verdicts = [v["verdicts"][0] for v in values]
    findings = []
    for candidate, verdict in zip(state["candidates"], verdicts):
        if verdict["verdict"] != "REFUTED":
            findings.append({**candidate, "verdict": verdict["verdict"]})
    state["findings"] = findings
    save_state(state)
    lines = [f"# Findings ({len(findings)})", ""]
    for number, f in enumerate(findings, 1):
        lines += [
            f"{number}. {f['file']}:{f['line']} ({f['verdict']}): {f['summary']}",
            f"   Failure scenario: {f['failure_scenario']}",
        ]
    REPORT.write_text("\n".join(lines) + "\n")
    print(f"Kept {len(findings)} of {len(verdicts)} verdicts")
    emit(verified_count=len(verdicts), reported=len(findings))


def main(argv):
    if argv[:1] == ["report"]:
        sys.stdout.write(REPORT.read_text())
        return
    if len(argv) != 2 or argv[0] != "merge" or argv[1] not in OUTPUT_KEYS:
        sys.exit(__doc__)
    values = branch_values(json.load(sys.stdin), OUTPUT_KEYS[argv[1]])
    (merge_find if argv[1] == "find" else merge_verify)(values)


if __name__ == "__main__":
    main(sys.argv[1:])
