#!/usr/bin/env python3
"""Exercise retention under the historical near-full file budget."""
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import artifacts


class Collection(unittest.TestCase):
    def test_failure_logs_precede_success_files_and_omissions_are_explicit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            reports = root / "target/check-reports/run"
            reports.mkdir(parents=True)
            for index in range(128):
                (reports / f"a-success-{index:03}.log").write_text("PASS\n")
            failure = reports / "z-etcd/failure"
            failure.mkdir(parents=True)
            (failure / "replay-oracle-log.stdout").write_text("assertion failed\n")
            (failure / "summary.txt").write_text("replay exit 1\n")
            artifacts.collect(root)
            output = root / "target/pv-artifacts"
            retained = output / "check-reports/run/z-etcd/failure"
            self.assertEqual((retained / "replay-oracle-log.stdout").read_text(), "assertion failed\n")
            self.assertEqual((retained / "summary.txt").read_text(), "replay exit 1\n")
            limits = (output / "LIMITS.txt").read_text()
            self.assertIn("Retained files: 128; omitted eligible files: 2", limits)
            self.assertIn("omitted: run/a-success", limits)

    def test_byte_limits_and_priority_overflow_are_reported(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            failure = root / "target/check-reports/run/failure"
            failure.mkdir(parents=True)
            (failure / "oracle-log.stdout").write_bytes(b"x" * 32)
            (failure / "world.stdout").write_bytes(b"y" * 32)
            (failure / "z-state.json").write_bytes(b"z" * 32)
            with patch.multiple(artifacts, LIMIT=4096 + 20, PER_FILE=12, FILES=2):
                artifacts.collect(root)
            output = root / "target/pv-artifacts"
            self.assertEqual((output / "check-reports/run/failure/oracle-log.stdout.truncated").stat().st_size, 12)
            self.assertEqual((output / "check-reports/run/failure/world.stdout.truncated").stat().st_size, 8)
            limits = (output / "LIMITS.txt").read_text()
            self.assertIn("omitted eligible files: 1; truncated files: 2", limits)
            self.assertIn("omitted: run/failure/z-state.json", limits)
            self.assertLessEqual(sum(p.stat().st_size for p in output.rglob("*") if p.is_file()), 4096 + 20)


if __name__ == "__main__":
    unittest.main()
