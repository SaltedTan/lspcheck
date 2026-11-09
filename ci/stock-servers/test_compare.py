#!/usr/bin/env python3
# Copyright 2026 The lspcheck Authors.
# SPDX-License-Identifier: Apache-2.0

"""Exercise the comparator CLI against the documented verdict-file contract."""

import itertools
import json
from pathlib import Path
import subprocess
import tempfile
import unittest


COMPARE = Path(__file__).absolute().with_name("compare")


class CompareTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.expected = self.root / "expected.verdicts"
        self.expected.write_text(
            "# A server exit race can flip F1 and leave J1 inconclusive.\n"
            "F1 pass|fail\n\nJ1 pass|error\nF2 pass\n"
        )
        self.report = self.root / "report.json"

    def run_compare(self, rows=None, raw=None):
        if raw is None:
            raw = json.dumps({"results": [
                {"id": check, "verdict": verdict} for check, verdict in rows
            ]})
        self.report.write_text(raw)
        return subprocess.run(
            [str(COMPARE), str(self.expected), str(self.report)],
            capture_output=True, text=True, timeout=10,
        )

    def test_allowed_alternatives_are_silent(self):
        for framing, correlation in itertools.product(("pass", "fail"), ("pass", "error")):
            with self.subTest(framing=framing, correlation=correlation):
                result = self.run_compare([("F1", framing), ("J1", correlation), ("F2", "pass")])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, "")

    def test_diff_only_shows_real_changes(self):
        result = self.run_compare([("F1", "fail"), ("J1", "error"), ("F2", "fail")])
        self.assertEqual(result.returncode, 1, result.stderr)
        changes = [line for line in result.stdout.splitlines()
                   if line.startswith(("+", "-")) and not line.startswith(("+++", "---"))]
        self.assertEqual(changes, ["-F2 pass", "+F2 fail"])
        for unsupported in ("error", "skip", "not-exercised", "pas"):
            with self.subTest(unsupported=unsupported):
                result = self.run_compare([("F1", unsupported), ("J1", "pass"), ("F2", "pass")])
                self.assertEqual(result.returncode, 1)
                self.assertIn("+F1 " + unsupported, result.stdout)

    def test_changed_check_catalogue_is_rejected(self):
        matching = [("F1", "pass"), ("J1", "pass"), ("F2", "pass")]
        cases = {
            "missing": matching[:-1],
            "extra": matching + [("X1", "pass")],
            "duplicate": matching + [matching[-1]],
            "reordered": list(reversed(matching)),
            "empty": [],
        }
        for name, rows in cases.items():
            with self.subTest(name=name):
                result = self.run_compare(rows)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("@@", result.stdout)

    def test_unreadable_or_partially_parseable_report_is_rejected(self):
        matching = json.dumps({"results": [
            {"id": "F1", "verdict": "pass"},
            {"id": "J1", "verdict": "pass"},
            {"id": "F2", "verdict": "pass"},
        ]})
        for raw in ("{broken", matching + "\n{broken", matching + '\n{"results": null}'):
            with self.subTest(raw=raw):
                result = self.run_compare(raw=raw)
                self.assertNotEqual(result.returncode, 0, result.stderr)
                self.assertIn("jq:", result.stderr)
        self.report.unlink()
        result = subprocess.run(
            [str(COMPARE), str(self.expected), str(self.report)],
            capture_output=True, text=True, timeout=10,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Could not open file", result.stderr)


if __name__ == "__main__":
    unittest.main()
