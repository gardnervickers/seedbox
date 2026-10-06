"""Runner control tests using a batch stub; never invokes KVM, sudo or MSRs."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SIM = Path(__file__).resolve().parents[1]
STUB = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
world, prefix, seeds = sys.argv[1:4]
root = pathlib.Path.cwd()
with (root / "calls.jsonl").open("a") as calls:
    calls.write(json.dumps({"prefix": prefix, "seeds": seeds}) + "\n")
mode = os.environ["STUB_MODE"]
retry = prefix.endswith("-retry")
if mode == "setup_failure":
    sys.exit(1)
for i, seed in enumerate(seeds.split()):
    out = root / f"runs/{prefix}-s{seed}-r{i}"
    out.mkdir(parents=True)
    report = {"seed": int(seed), "outcome": "end_machine_exited", "host_seconds": 1.5}
    console = "RESULT pass completed\n"
    status, error = 0, ""
    if mode in ("failed", "overshoot", "retry_failed") and not retry:
        report, console, status = None, "", 1
        error = "overshoot on client" if mode != "failed" else "clock bound exceeded"
    if mode == "retry_failed" and retry:
        report, console, status, error = None, "", 1, "overshoot again"
    if mode == "incomplete_json":
        report = {"seed": int(seed)}
    if mode == "failed_status":
        status = 124
    if mode == "wrong_seed":
        report["seed"] += 1
    if mode == "missing_status":
        status = None
    if mode == "consistency_fail":
        console = "RESULT fail contradiction\n"
    pathlib.Path(str(out) + ".json").write_text(json.dumps(report) if report else "")
    pathlib.Path(str(out) + ".err").write_text(error)
    if status is not None:
        pathlib.Path(str(out) + ".status").write_text(str(status) + "\n")
    (out / "client.console").write_text(console)
'''


class CampaignTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.runner = self.root / "sim"
        self.runner.mkdir()
        for name in ("campaign.sh", "campaign-result.py"):
            shutil.copy2(SIM / name, self.runner / name)
        (self.runner / "batch.sh").write_text(STUB)
        (self.runner / "batch.sh").chmod(0o755)
        (self.root / "runs").mkdir()

    def run_campaign(self, mode):
        env = dict(os.environ, SEEDBOX_ROOT=str(self.root), BATCH="1", STUB_MODE=mode)
        result = subprocess.run(
            ["bash", str(self.runner / "campaign.sh"), "world.json", "hunt", "7", "1", "1"],
            env=env, text=True, capture_output=True, timeout=10,
        )
        log = (self.root / "runs/hunt.campaign.log").read_text()
        calls = [json.loads(line) for line in (self.root / "calls.jsonl").read_text().splitlines()]
        return result, log, calls

    def test_success_uses_a_fresh_attempt(self):
        result, log, calls = self.run_campaign("success")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("host_s=2 RESULT pass", log)
        self.assertIn("DONE unresolved=0", log)
        self.assertTrue(calls[0]["prefix"].startswith("hunt-batch."))

    def test_reused_failed_outputs_cannot_accept_old_console(self):
        old = self.root / "runs/hunt-s7-r0"
        old.mkdir()
        (old / "client.console").write_text("RESULT pass old evidence\n")
        old_json = self.root / "runs/hunt-s7-r0.json"
        old_json.write_text('{"seed":7,"outcome":"end_machine_exited","host_seconds":1}')
        result, log, calls = self.run_campaign("failed")
        self.assertEqual(result.returncode, 1)
        self.assertIn("UNRESOLVED clock bound exceeded", log)
        self.assertNotIn("RESULT pass", log)
        self.assertEqual(len(calls), 1)  # Clock costs are not PMU-margin retries.
        self.assertEqual((old / "client.console").read_text(), "RESULT pass old evidence\n")
        self.assertEqual(json.loads(old_json.read_text())["seed"], 7)

    def test_overshoot_retries_with_separate_fresh_output(self):
        result, log, calls = self.run_campaign("overshoot")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("(retry) RESULT pass", log)
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[1]["prefix"], calls[0]["prefix"] + "-retry")

    def test_failed_retry_is_unresolved(self):
        result, log, calls = self.run_campaign("retry_failed")
        self.assertEqual(result.returncode, 1)
        self.assertIn("UNRESOLVED overshoot again", log)
        self.assertEqual(len(calls), 2)

    def test_incomplete_or_failed_attempt_cannot_accept_its_result(self):
        for mode in ("incomplete_json", "failed_status", "wrong_seed", "missing_status"):
            with self.subTest(mode=mode):
                result, log, _ = self.run_campaign(mode)
                self.assertEqual(result.returncode, 1)
                self.assertIn("UNRESOLVED", log)
                self.assertNotIn("RESULT pass", log)
                (self.root / "runs/hunt.campaign.log").unlink()

    def test_consistency_failure_remains_a_completed_campaign_result(self):
        result, log, _ = self.run_campaign("consistency_fail")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("RESULT fail", log)
        self.assertIn("FOUND", log)

    def test_setup_failure_aborts(self):
        result, log, _ = self.run_campaign("setup_failure")
        self.assertEqual(result.returncode, 1)
        self.assertIn("ABORTED batch setup failed", log)


if __name__ == "__main__":
    unittest.main()
