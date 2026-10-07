#!/usr/bin/env python3
"""Report backend identity must be checked before witness equality."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class Compare(unittest.TestCase):
    def test_backend_comparability(self):
        report = dict(backend="hvf", arch="arm64", timebase="pv", format=1,
                      seed=1, kernel_sha256="k", initrd_sha256="i", witness="same")
        with tempfile.TemporaryDirectory() as directory:
            left, right = [Path(directory) / name for name in ("a.json", "b.json")]
            left.write_text(json.dumps(report))
            for backend, expected in [("hvf", "MATCH: same"),
                                      ("kvm", "NOT_COMPARABLE: backend: hvf != kvm"),
                                      (None, "NOT_COMPARABLE: backend missing")]:
                other = dict(report)
                if backend is None:
                    del other["backend"]
                else:
                    other["backend"] = backend
                right.write_text(json.dumps(other))
                result = subprocess.run([sys.executable, str(Path(__file__).with_name("compare.py")),
                                         str(left), str(right)], capture_output=True, text=True)
                self.assertEqual(result.returncode, 0 if backend == "hvf" else 1)
                self.assertIn(expected, result.stdout + result.stderr)
                if backend != "hvf":
                    self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
