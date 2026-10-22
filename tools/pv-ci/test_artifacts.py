#!/usr/bin/env python3
"""Exercise retention under the historical near-full file budget."""
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import artifacts


class Collection(unittest.TestCase):
    def test_failures_and_pv_samples_precede_routine_files_and_omissions_are_explicit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            reports = root / "target/check-reports/run"
            reports.mkdir(parents=True)
            for index in range(128):
                (reports / f"a-success-{index:03}.log").write_text("PASS\n")
            samples = []
            for index in range(5):
                sample = reports / f"b-etcd-{index}/pv-baseline-sample"
                sample.mkdir(parents=True)
                for name in ("baseline-manifest.json", "baseline-driver-state.json",
                             "baseline-oracle-pages.out", "baseline-vmm.stderr"):
                    path = sample / name
                    data = f"round {index} {name}".encode() + b"\0\xff\n"
                    path.write_bytes(data)
                    samples.append((path.relative_to(reports), data))
            failure = reports / "z-etcd/failure"
            failure.mkdir(parents=True)
            (failure / "replay-oracle-log.stdout").write_text("assertion failed\n")
            (failure / "summary.txt").write_text("replay exit 1\n")
            artifacts.collect(root)
            output = root / "target/pv-artifacts"
            retained = output / "check-reports/run/z-etcd/failure"
            self.assertEqual((retained / "replay-oracle-log.stdout").read_text(), "assertion failed\n")
            self.assertEqual((retained / "summary.txt").read_text(), "replay exit 1\n")
            for path, data in samples:
                self.assertEqual((output / "check-reports/run" / path).read_bytes(), data)
            limits = (output / "LIMITS.txt").read_text()
            self.assertIn("Retained files: 128; omitted eligible files: 22", limits)
            self.assertIn("omitted: run/a-success", limits)

    def test_byte_limits_and_priority_overflow_are_reported(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            failure = root / "target/check-reports/run/failure"
            failure.mkdir(parents=True)
            (failure / "oracle-log.stdout").write_bytes(b"x" * 32)
            (failure / "world.stdout").write_bytes(b"y" * 32)
            (failure / "z-state.json").write_bytes(b"z" * 32)
            sample = root / "target/check-reports/run/a-etcd/pv-baseline-sample"
            sample.mkdir(parents=True)
            (sample / "baseline-vmm.stderr").write_bytes(b"sample")
            with patch.multiple(artifacts, LIMIT=4096 + 20, PER_FILE=12, FILES=2):
                artifacts.collect(root)
            output = root / "target/pv-artifacts"
            self.assertEqual((output / "check-reports/run/failure/oracle-log.stdout.truncated").stat().st_size, 12)
            self.assertEqual((output / "check-reports/run/failure/world.stdout.truncated").stat().st_size, 8)
            limits = (output / "LIMITS.txt").read_text()
            self.assertIn("omitted eligible files: 2; truncated files: 2", limits)
            self.assertIn("omitted: run/failure/z-state.json", limits)
            self.assertIn("omitted: run/a-etcd/pv-baseline-sample/baseline-vmm.stderr", limits)
            self.assertLessEqual(sum(p.stat().st_size for p in output.rglob("*") if p.is_file()), 4096 + 20)


if __name__ == "__main__":
    unittest.main()
