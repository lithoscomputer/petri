#!/usr/bin/env python3
"""Check that every citation of a locked library revision agrees.

    scripts/check-pins.py [--evidence DIR] [--update-contract]

Every internal git dependency (a `lithoscomputer/*` repository) must name
exactly `branch = "main"` in its manifest: never `rev`, and never an omitted
ref, which Cargo treats as a different source. `Cargo.lock` chooses the
commit. Sources compared:

  Cargo.toml, crates/petri/cli/Cargo.toml, crates/petri/lib/Cargo.toml
                                      every internal git dependency tracks `main`
  Cargo.lock                          the locked commit of pebble-coding-agent and
                                      pebble-agent, lithos-llm, the sandbox-driver
                                      packages, and the twins (one commit, and one
                                      copy, per repository)
  crates/core/executor-sandbox/src/backend.rs  RUNNER_PIN, the sandbox-images revision of
                                      the default runner images (cited by the contract
                                      table, not by evidence records)
  crates/fabro/corpus-pin.txt         the Fabro reference commit
  crates/fabro/acceptance/bundles.lock.json   fabro_reference.commit
  crates/fabro/acceptance/CONTRACT.md the "Pinned revisions" table
  DIR/records/*.json                  the `pins` block of every evidence record
                                      (default: target/fabro-evidence/latest when present)

Exit 1 with every disagreement listed; exit 0 when all agree.
With --update-contract, refresh only the internal library rows from Cargo.lock,
and write CONTRACT.md only if all remaining checks (including evidence) pass.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INTERNAL = "github.com/lithoscomputer/"
MANIFESTS = ("Cargo.toml", "crates/petri/cli/Cargo.toml", "crates/petri/lib/Cargo.toml")
# The locked packages behind each row of the contract table.
LOCKED = {
    "pebble": ("pebble-coding-agent", "pebble-agent"),
    "lithos_llm": ("lithos-llm",),
    "sandbox_driver": ("sandbox-driver", "sandbox-driver-protocol", "sandbox-driver-docker-config", "sandbox-driver-daytona-config"),
    "twins": ("twin-openai", "twin-anthropic"),
}


def dependency_tables(manifest: dict) -> list[tuple[str, dict]]:
    tables = [manifest.get("workspace", {}).get("dependencies", {})]
    tables += [manifest.get(kind, {}) for kind in ("dependencies", "dev-dependencies", "build-dependencies")]
    for target in manifest.get("target", {}).values():
        tables += [target.get(kind, {}) for kind in ("dependencies", "dev-dependencies", "build-dependencies")]
    return [(name, spec) for table in tables for name, spec in table.items() if isinstance(spec, dict)]


def check_manifest_refs(problems: list[str]) -> None:
    """Every internal git dependency names exactly `branch = "main"`."""
    for relative in MANIFESTS:
        manifest = tomllib.loads((ROOT / relative).read_text(encoding="utf-8"))
        for name, spec in dependency_tables(manifest):
            if INTERNAL not in spec.get("git", ""):
                continue
            refs = {key: spec[key] for key in ("branch", "rev", "tag") if key in spec}
            if refs != {"branch": "main"}:
                problems.append(f'{relative}: {name} must name exactly branch = "main", found {refs or "no ref"}')


def locked_revisions(problems: list[str]) -> dict[str, str]:
    """The commit `Cargo.lock` chooses for each internal repository."""
    lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    sources: dict[str, set[str]] = {}
    for package in lock.get("package", []):
        sources.setdefault(package["name"], set()).add(package.get("source", "path"))
    expected: dict[str, str] = {}
    for row, names in LOCKED.items():
        commits: dict[str, str] = {}
        for name in names:
            found = sources.get(name, set())
            if not found:
                problems.append(f"Cargo.lock: {name} is not locked")
            elif len(found) > 1:
                problems.append(f"Cargo.lock: {name} is locked more than once: " + ", ".join(sorted(found)))
            else:
                source = next(iter(found))
                if "?branch=main#" not in source:
                    problems.append(f"Cargo.lock: {name} is not locked from branch main: {source}")
                commits[name] = source.rsplit("#", 1)[-1]
        commit = same(problems, row, commits) if commits else None
        if commit:
            expected[row] = commit
    return expected


def same(problems: list[str], label: str, values: dict[str, str]) -> str | None:
    distinct = sorted(set(values.values()))
    if len(distinct) == 1:
        return distinct[0]
    problems.append(f"{label}: revisions disagree: " + ", ".join(f"{k}={v}" for k, v in sorted(values.items())))
    return None


def contract_table(text: str) -> dict[str, str]:
    start = text.find("## Pinned revisions")
    if start < 0:
        return {}
    section = text[start:]
    end = section.find("\n## ", 1)
    section = section if end < 0 else section[:end]
    table: dict[str, str] = {}
    for line in section.splitlines():
        m = re.match(r"^\|\s*`?([a-z0-9_-]+)`?\s*\|\s*`([0-9a-f]{7,40})`", line)
        if m:
            table[m.group(1)] = m.group(2)
    return table


def update_contract(text: str, expected: dict[str, str], problems: list[str]) -> str:
    """Refresh library citations, without changing reference or runner provenance."""
    start = text.find("## Pinned revisions\n")
    if start < 0:
        problems.append("CONTRACT.md: no `## Pinned revisions` table")
        return text
    end = text.find("\n## ", start + 1)
    end = len(text) if end < 0 else end
    section = text[start:end]
    for name in LOCKED:
        if name not in expected:
            continue
        row = re.compile(rf"^(\|\s*`?{name}`?\s*\|\s*`)[0-9a-f]{{7,40}}(`[^\n]*)$", re.M)
        section, count = row.subn(lambda m: m[1] + expected[name] + m[2], section)
        if count != 1:
            problems.append(f"CONTRACT.md: expected one row for {name}, found {count}")
    return text[:start] + section + text[end:]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--evidence", type=Path)
    parser.add_argument("--update-contract", action="store_true", help="refresh internal library citations from Cargo.lock after validation")
    args = parser.parse_args()
    problems: list[str] = []

    check_manifest_refs(problems)
    expected: dict[str, str] = locked_revisions(problems)

    pin_file = ROOT / "crates/fabro/corpus-pin.txt"
    fabro = next((line.split()[0] for line in pin_file.read_text().splitlines() if line.strip() and not line.startswith("#")), None)
    if fabro:
        expected["fabro_reference"] = fabro
    else:
        problems.append(f"{pin_file}: no pin")
    lock = json.loads((ROOT / "crates/fabro/acceptance/bundles.lock.json").read_text(encoding="utf-8"))
    if lock.get("fabro_reference", {}).get("commit") != fabro:
        problems.append(f"bundles.lock.json fabro_reference.commit {lock.get('fabro_reference', {}).get('commit')} != pin {fabro}")

    contract_path = ROOT / "crates/fabro/acceptance/CONTRACT.md"
    original_contract = contract_path.read_text(encoding="utf-8")
    updated_contract = update_contract(original_contract, expected, problems) if args.update_contract else original_contract
    contract = contract_table(updated_contract)
    if not contract:
        problems.append("CONTRACT.md: no `## Pinned revisions` table")
    backend = (ROOT / "crates/core/executor-sandbox/src/backend.rs").read_text(encoding="utf-8")
    runner = re.search(r'const RUNNER_PIN: &str = "([0-9a-f]+)"', backend)
    if runner is None:
        problems.append("backend.rs: RUNNER_PIN not found")
    elif contract.get("runner_image") is None:
        problems.append("CONTRACT.md: no row for runner_image")
    elif not runner.group(1).startswith(contract["runner_image"]) and not contract["runner_image"].startswith(runner.group(1)):
        problems.append(f"CONTRACT.md: runner_image is {contract['runner_image']}, backend.rs pins {runner.group(1)}")
    for name, rev in expected.items():
        cited = contract.get(name)
        if cited is None:
            problems.append(f"CONTRACT.md: no row for {name}")
        elif not rev.startswith(cited):
            problems.append(f"CONTRACT.md: {name} is {cited}, Cargo.lock locks {rev}")

    evidence = args.evidence or (ROOT / "target/fabro-evidence/latest")
    records = sorted(evidence.glob("records/*.json")) if evidence.is_dir() else []
    for path in records:
        try:
            pins = json.loads(path.read_text(encoding="utf-8")).get("pins") or {}
        except ValueError as error:
            problems.append(f"{path}: not JSON: {error}")
            continue
        for name, rev in expected.items():
            cited = pins.get(name)
            if isinstance(cited, dict):
                cited = cited.get("commit")
            if cited is None:
                problems.append(f"{path.name}: no {name} pin")
            elif cited != rev:
                problems.append(f"{path.name}: {name} cites {cited}, Cargo.lock locks {rev}")

    for name, rev in sorted(expected.items()):
        print(f"{name:16} {rev}")
    if runner is not None:
        print(f"{'runner_image':16} {runner.group(1)}")
    print(f"evidence records checked: {len(records)}" + (f" ({evidence})" if records else ""))
    if problems:
        print("pin check failed:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    if updated_contract != original_contract:
        contract_path.write_text(updated_contract, encoding="utf-8")
        print("updated CONTRACT.md internal library citations")
    print("pins agree")
    return 0


if __name__ == "__main__":
    sys.exit(main())
