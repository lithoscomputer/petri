"""Exercise dependency upgrades through the pin checker's public command."""

import json
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CONTRACT = Path("crates/fabro/acceptance/CONTRACT.md")
NEW_REV = "1234567890abcdef1234567890abcdef12345678"


class PinChecks(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        for relative in (
            "scripts/check-pins.py", "Cargo.toml", "Cargo.lock",
            "crates/petri/cli/Cargo.toml", "crates/petri/lib/Cargo.toml",
            "crates/fabro/corpus-pin.txt", "crates/fabro/acceptance/bundles.lock.json",
            "crates/core/executor-sandbox/src/backend.rs", CONTRACT,
        ):
            target = self.root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, target)
        self.contract = (self.root / CONTRACT).read_text()

    def check(self, *args):
        return subprocess.run(
            [sys.executable, str(self.root / "scripts/check-pins.py"), *args],
            capture_output=True, text=True, check=False,
        )

    def move_twins(self):
        lock = self.root / "Cargo.lock"
        text = lock.read_text()
        updated, count = re.subn(
            r'(git\+https://github.com/lithoscomputer/twins\?branch=main#)[0-9a-f]+',
            lambda m: m[1] + NEW_REV, text,
        )
        self.assertGreaterEqual(count, 2)
        lock.write_text(updated)

    def assert_rejected_without_edit(self, message):
        before = (self.root / CONTRACT).read_bytes()
        result = self.check("--update-contract")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(message, result.stderr)
        self.assertEqual((self.root / CONTRACT).read_bytes(), before)

    def test_upgrade_requires_a_coordinated_contract_update(self):
        self.assertEqual(self.check().returncode, 0)
        self.move_twins()
        result = self.check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("CONTRACT.md: twins", result.stderr)
        self.assertEqual((self.root / CONTRACT).read_text(), self.contract)

        result = self.check("--update-contract")
        self.assertEqual(result.returncode, 0, result.stderr)
        expected = re.sub(
            r'(\| `twins` \| `)[0-9a-f]+', lambda m: m[1] + NEW_REV, self.contract,
        )
        self.assertEqual((self.root / CONTRACT).read_text(), expected)
        self.assertEqual(self.check().returncode, 0)
        self.assertEqual(self.check("--update-contract").returncode, 0)
        self.assertEqual((self.root / CONTRACT).read_text(), expected)

    def test_upgrade_keeps_evidence_validation_strict(self):
        # Default evidence location, as used by check:pins. A candidate update
        # must not rewrite or accept records from the committed baseline.
        pins = dict(re.findall(r'^\| `([a-z_]+)` \| `([0-9a-f]+)`', self.contract, re.M))
        evidence = self.root / "target/fabro-evidence/latest/records/host.json"
        evidence.parent.mkdir(parents=True)
        evidence.write_text(json.dumps({"pins": pins}))
        self.assertEqual(self.check().returncode, 0)
        self.move_twins()
        original = evidence.read_bytes()
        self.assert_rejected_without_edit("host.json: twins cites")
        self.assertEqual(evidence.read_bytes(), original)
        pins["twins"] = NEW_REV
        evidence.write_text(json.dumps({"pins": pins}))
        result = self.check("--update-contract")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.check().returncode, 0)

    def test_invalid_manifest_ref_prevents_an_update(self):
        self.move_twins()
        manifest = self.root / "crates/petri/cli/Cargo.toml"
        manifest.write_text(manifest.read_text().replace('branch = "main"', 'branch = "other"'))
        self.assert_rejected_without_edit('must name exactly branch = "main"')

    def test_duplicate_locked_source_prevents_an_update(self):
        self.move_twins()
        with (self.root / "Cargo.lock").open("a") as lock:
            lock.write('\n[[package]]\nname = "twin-openai"\nversion = "0.0.0"\n'
                       'source = "git+https://github.com/lithoscomputer/twins?branch=main#'
                       + "f" * 40 + '"\n')
        self.assert_rejected_without_edit("twin-openai is locked more than once")

    def test_missing_locked_package_prevents_an_update(self):
        self.move_twins()
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace('name = "twin-anthropic"', 'name = "missing-twin"'))
        self.assert_rejected_without_edit("twin-anthropic is not locked")

    def test_split_locked_revisions_prevent_an_update(self):
        self.move_twins()
        lock = self.root / "Cargo.lock"
        lock.write_text(lock.read_text().replace(NEW_REV, "f" * 40, 1))
        self.assert_rejected_without_edit("twins: revisions disagree")

    def test_reference_and_runner_citations_are_not_refreshed(self):
        self.move_twins()
        for name in ("fabro_reference", "runner_image"):
            with self.subTest(name=name):
                changed = re.sub(
                    rf'(\| `{name}` \| `)[0-9a-f]+', lambda m: m[1] + "f" * 40, self.contract,
                )
                (self.root / CONTRACT).write_text(changed)
                self.assert_rejected_without_edit(f"CONTRACT.md: {name}")

    def test_missing_or_duplicate_contract_rows_prevent_an_update(self):
        self.move_twins()
        row = next(line for line in self.contract.splitlines(keepends=True) if line.startswith("| `twins` |"))
        for replacement in ("", row * 2):
            with self.subTest(replacement=replacement):
                (self.root / CONTRACT).write_text(self.contract.replace(row, replacement))
                self.assert_rejected_without_edit("expected one row for twins")


if __name__ == "__main__":
    unittest.main()
