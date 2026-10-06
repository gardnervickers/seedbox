"""Acceptance suite logic with a fake launcher and a batch.sh stub; never invokes KVM, sudo or MSRs."""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SIM = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SIM))

from acceptance import checks  # noqa: E402
from acceptance.engine import execute, overall  # noqa: E402
from acceptance.launcher import BatchLauncher  # noqa: E402
from acceptance.model import FAIL, FULL_STATE, PASS, UNRESOLVED, Job, RunResult  # noqa: E402

PINS = json.loads((SIM / "acceptance/cases.json").read_text())


class FakeLauncher:
    """Deterministic world: the witness depends only on scenario, seed and witness mode."""

    def __init__(self, tweak=None):
        self.tweak = tweak or (lambda job, result, n: None)
        self.jobs = []

    def run(self, jobs):
        results = []
        for job in jobs:
            self.jobs.append(job)
            base = f"{job.scenario}/{job.seed}"
            status, line = PASS, "RESULT pass ok"
            if job.scenario in ("sanity", "kill-3.5.2"):
                status, line = FAIL, "RESULT fail missing"
            result = RunResult(job, status, result_line=line, workload_exit=int(status == FAIL),
                               world_witness=f"w:{base}",
                               machine_witnesses={"n1": f"n1:{base}", "client": f"c:{base}"},
                               state_digests={"n1": f"d:{base}"} if job.witness == FULL_STATE else {},
                               host_seconds=1.0, sim_seconds=10.0,
                               faults=[{"member": "n1", "stop_ns": 1, "start_ns": 2, "stopped": True, "restarted": True}]
                               if "kill" in job.scenario else [])
            if job.scenario == "race":
                lost = job.seed % 3
                result.logs = {"n1": f"RACE mode=racy counter=9 expected=9 lost={lost}\n"
                                     "RACE mode=atomic counter=9 expected=9 lost=0\n"}
                result.result_line, result.workload_exit = None, 0
            self.tweak(job, result, len(self.jobs))
            results.append(result)
        return results


def verdicts(launcher, tier=checks.FULL, **ctx):
    outcomes = execute(tier, checks.Context(PINS, **ctx), launcher)
    return {o.name.split()[0]: o.verdict for o in outcomes}, outcomes


class CheckTests(unittest.TestCase):
    def test_deterministic_build_passes_and_shares_default_runs(self):
        launcher = FakeLauncher()
        result, outcomes = verdicts(launcher)
        self.assertEqual({k: v.status for k, v in result.items()},
                         {"1": PASS, "2": PASS, "3": PASS, "4": PASS, "8": PASS, "6": PASS, "7": checks.SKIPPED})
        self.assertEqual(overall(outcomes), "PASS")
        short = [j for j in launcher.jobs if j.scenario == "short-kill-3.5.2"]
        # Items 1 (2 + 2 digest runs), 2 (3 variants) and 8 share one default run.
        self.assertEqual(len(short), 7)
        self.assertIn("RAM/register digests", result["1"].summary)
        self.assertEqual(len([j for j in launcher.jobs if j.scenario == "kill-3.5.3"]), 11)

    def test_replay_digest_mismatch(self):
        def tweak(job, result, n):
            if job.witness == FULL_STATE and n % 2:
                result.state_digests["n1"] = "other"
        result, _ = verdicts(FakeLauncher(tweak), checks.SMOKE)
        self.assertEqual(result["1"].status, FAIL)
        self.assertIn("n1:digest", result["1"].summary)

    def test_stop_placement_difference_is_a_determinism_bug(self):
        def tweak(job, result, n):
            if ("undo", "0") in job.performance:
                result.machine_witnesses["client"] = "skewed"
        result, outcomes = verdicts(FakeLauncher(tweak), checks.SMOKE)
        self.assertEqual(result["2"].status, FAIL)
        self.assertIn("undo=0 differs in client", result["2"].summary)
        self.assertEqual(overall(outcomes), "REGRESSION")

    def test_race_needs_varied_outcomes_clean_atomic_and_a_replaying_failure(self):
        def same(job, result, n):
            if job.scenario == "race":
                result.logs["n1"] = result.logs["n1"].replace(f"racy counter=9 expected=9 lost={job.seed % 3}",
                                                              "racy counter=5 expected=9 lost=4")
        self.assertIn("same outcome", verdicts(FakeLauncher(same), checks.SMOKE)[0]["3"].summary)

        def dirty(job, result, n):
            if job.scenario == "race" and job.seed == 2:
                result.logs["n1"] = result.logs["n1"].replace("atomic counter=9 expected=9 lost=0", "atomic counter=8 expected=9 lost=1")
        self.assertIn("atomic control lost 1", verdicts(FakeLauncher(dirty), checks.SMOKE)[0]["3"].summary)

        launcher = FakeLauncher()
        def diverge(job, result, n):
            if job.scenario == "race" and len([j for j in launcher.jobs if j.scenario == "race"]) > len(PINS["race"]["seeds"]):
                result.world_witness = "diverged"
        launcher.tweak = diverge
        result = verdicts(launcher, checks.SMOKE)[0]["3"]
        self.assertEqual(result.status, FAIL)
        self.assertIn("did not replay", result.summary)

    def test_pinned_margin_completed_only_by_retry_was_not_exercised(self):
        def retried(job, result, n):
            if ("margin", "32768") in job.performance:
                result.retries, result.attempts = 1, [{"evidence": "x", "detail": "exit status 1: overshoot on n1"}]
        result = verdicts(FakeLauncher(retried), checks.SMOKE)[0]["2"]
        self.assertEqual(result.status, UNRESOLVED)
        self.assertIn("margin=32768 not exercised", result.summary)

        def also_diverges(job, result, n):
            retried(job, result, n)
            if ("undo", "0") in job.performance:
                result.world_witness = "skewed"
        # A real determinism failure is never hidden behind an unexercised margin.
        result = verdicts(FakeLauncher(also_diverges), checks.SMOKE)[0]["2"]
        self.assertEqual(result.status, FAIL)
        self.assertIn("undo=0 differs", result.summary)

    def test_fault_effects_need_applied_faults_and_recovery(self):
        def none(job, result, n):
            if job.scenario == "short-kill-3.5.2":
                result.faults = []
        self.assertIn("no guest acknowledged", verdicts(FakeLauncher(none), checks.SMOKE)[0]["8"].summary)

        def scheduled_only(job, result, n):
            for fault in result.faults:
                fault["restarted"] = False
        result = verdicts(FakeLauncher(scheduled_only), checks.SMOKE)[0]["8"]
        self.assertEqual(result.status, FAIL)
        self.assertIn("1 scheduled, 1 stopped", result.summary)

        def broken(job, result, n):
            if job.scenario == "short-kill-3.5.2":
                result.status, result.detail = FAIL, "RESULT fail missing"
        self.assertIn("did not recover", verdicts(FakeLauncher(broken), checks.SMOKE)[0]["8"].summary)

        def unfaulted(job, result, n):
            if job.scenario == "kill-3.5.3" and job.seed == 4002:
                result.faults = []
        self.assertIn("seeds [4002]", verdicts(FakeLauncher(unfaulted))[0]["6"].summary)

    def test_launcher_error_is_unresolved_not_a_crash(self):
        class Broken:
            def run(self, jobs):
                raise OSError("batch.sh: permission denied")
        result, outcomes = verdicts(Broken(), checks.SMOKE)
        self.assertEqual({v.status for v in result.values()}, {UNRESOLVED})
        self.assertIn("permission denied", result["1"].summary)
        self.assertEqual(overall(outcomes), "UNRESOLVED")

    def test_checker_control_must_fail_with_exit_one(self):
        def passes(job, result, n):
            if job.scenario == "sanity":
                result.status, result.result_line, result.workload_exit = PASS, "RESULT pass", 0
        self.assertEqual(verdicts(FakeLauncher(passes), checks.SMOKE)[0]["4"].status, FAIL)

    def test_fixed_control_failures_and_unresolved(self):
        def stuck(job, result, n):
            if job.scenario == "kill-3.5.3" and job.seed == 4003:
                result.status, result.detail = UNRESOLVED, "overshoot again"
        result, outcomes = verdicts(FakeLauncher(stuck))
        self.assertEqual(result["6"].status, UNRESOLVED)
        self.assertEqual(overall(outcomes), "UNRESOLVED")

        def fails(job, result, n):
            if job.scenario == "kill-3.5.3" and job.seed == 4007:
                result.status = FAIL
        self.assertIn("seeds [4007]", verdicts(FakeLauncher(fails))[0]["6"].summary)

    def test_cross_build_compares_declared_performance_only_change(self):
        launcher = FakeLauncher()
        result, _ = verdicts(launcher, performance_only=True)
        self.assertEqual(result["7"].status, PASS)
        baseline = [j for j in launcher.jobs if j.engine == checks.BASELINE]
        self.assertEqual({(j.scenario, j.seed) for j in baseline}, {("short-kill-3.5.2", 4000)})

        def drift(job, result, n):
            if job.engine == checks.BASELINE and job.scenario == "short-kill-3.5.2":
                result.machine_witnesses["n1"] = "old"
        result, _ = verdicts(FakeLauncher(drift), performance_only=True)
        self.assertEqual(result["7"].status, FAIL)
        self.assertIn("short-kill-3.5.2 seed 4000 differs in n1", result["7"].summary)


STUB = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
world, prefix, seeds, *extra = sys.argv[1:]
root = pathlib.Path(os.environ["SEEDBOX_ROOT"])
with (root / "calls.jsonl").open("a") as calls:
    calls.write(json.dumps({"world": world, "prefix": prefix, "seeds": seeds, "extra": extra,
        "cpu0": os.environ["CPU0"], "undo": os.environ.get("SEEDBOX_UNDO"),
        "margin": os.environ.get("SEEDBOX_MARGIN"), "vmm": os.environ["VMM"]}) + "\n")
mode = os.environ.get("STUB_MODE", "")
if mode == "lease":
    sys.exit("batch: cpu 3 is already owned")
workload = json.loads(pathlib.Path(world).read_text())["end_on_exit"]
retry = os.environ.get("SEEDBOX_MARGIN") == "98304"
for i, seed in enumerate(seeds.split()):
    out = root / "runs" / f"{prefix}-s{seed}-r{i}"
    out.mkdir(parents=True)
    report = {"seed": int(seed), "outcome": "end_machine_exited", "host_seconds": 2.0, "sim_seconds": 10.0,
              "world_witness": f"w{seed}", "machines": [{"name": workload, "state": "Exited(0)", "witness": f"m{seed}",
              "exits": 5, "state_digest": "d" if "--digest" in extra else None}],
              "kills": [{"member": "n1", "stop_ns": 1, "start_ns": 2}, {"member": "n1", "stop_ns": 3, "start_ns": 4}]}
    console, status, err = "RESULT pass ok\n" if workload == "client" else "RACE mode=racy counter=1 expected=2 lost=1\n", 0, ""
    if mode == "clock" or mode == "overshoot_again" or (mode == "overshoot" and not retry):
        report, status = None, 1
        err = "clock bound exceeded: time 1 + 2 exceeds hard stop 2" if mode == "clock" else "soft overshoot on n1: 9 branches past"
    if mode == "contradiction":
        console = "RESULT fail planted\n"
    if mode == "dial_error":
        console = "RESULT error dial 10.0.0.1:2379\n"
        report["machines"][0]["state"] = "Exited(3)"
    pathlib.Path(f"{out}.json").write_text(json.dumps(report) if report else "")
    pathlib.Path(f"{out}.err").write_text(err)
    pathlib.Path(f"{out}.status").write_text(f"{status}\n")
    (out / f"{workload}.console").write_text(console)
    # Only the first scheduled kill lands before the run ends.
    (out / "n1.mgmt").write_text("n1 stopped\nn1 started\n" + "n1 stopped\n")
if mode == "restore_failed":
    sys.exit("batch: restoration failed; retained /tmp/seedbox-msr-x/saved.json")
'''


class BatchLauncherTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        base = Path(tmp.name)
        self.sim, self.root = base / "sim", base / "root"
        (self.sim / "worlds").mkdir(parents=True)
        (self.root / "runs").mkdir(parents=True)
        (base / "artifacts").mkdir()
        for name in ("bzImage", "cluster.cpio", "race.cpio"):
            (base / "artifacts" / name).write_text(name)
        world = {"kernel": "../../artifacts/bzImage", "end_on_exit": "client",
                 "machines": [{"name": n, "initrd": "../../artifacts/cluster.cpio"} for n in ("n1", "client")]}
        (self.sim / "worlds/kill.json").write_text(json.dumps(world))
        race = {"kernel": "../../artifacts/bzImage", "end_on_exit": "n1",
                "machines": [{"name": "n1", "initrd": "../../artifacts/race.cpio"}]}
        (self.sim / "worlds/race.json").write_text(json.dumps(race))
        (self.sim / "batch.sh").write_text(STUB)
        (self.sim / "batch.sh").chmod(0o755)
        self.vmm = base / "vmm"
        self.vmm.write_text("vmm")
        self.base = base

    def launcher(self, cpus=(2, 3, 4, 5), siblings=lambda cpu: {cpu}, **kw):
        return BatchLauncher(self.sim, self.root, {"kill": "worlds/kill.json", "race": "worlds/race.json"},
                             list(cpus), self.root / "runs/acc", {None: self.vmm, "baseline": self.vmm},
                             siblings=siblings, log=lambda message: None, **kw)

    def calls(self):
        return [json.loads(line) for line in (self.root / "calls.jsonl").read_text().splitlines()]

    def run_jobs(self, jobs, mode="", **kw):
        with patch.dict(os.environ, {"STUB_MODE": mode, "SEEDBOX_UNDO": "999"}):
            return self.launcher(**kw).run(jobs)

    def test_groups_settings_into_batches_without_leaking_ambient_settings(self):
        jobs = [Job("kill", 7), Job("kill", 7), Job("kill", 7, FULL_STATE),
                Job("kill", 7, performance=(("undo", "0"),)), Job("race", 1)]
        results = self.run_jobs(jobs)
        self.assertEqual([r.status for r in results], [PASS] * 5)
        calls = {(c["seeds"], tuple(c["extra"]), c["undo"]) for c in self.calls()}
        self.assertEqual(calls, {("7 7", (), None), ("7", ("--digest",), None), ("7", (), "0"), ("1", (), None)})
        self.assertEqual(results[2].state_digests, {"client": "d"})
        self.assertEqual(results[4].logs["n1"].split()[0], "RACE")
        self.assertEqual(len({r.evidence for r in results}), 5)

    def test_overshoot_is_retried_with_larger_margin(self):
        (result,) = self.run_jobs([Job("kill", 7)], "overshoot")
        self.assertEqual((result.status, result.retries), (PASS, 1))
        (failed,) = result.attempts
        self.assertIn("overshoot", failed["detail"])
        self.assertTrue(Path(f"{failed['evidence']}.err").exists())
        self.assertEqual(result.summary()["failed_attempts"], [failed])
        self.assertEqual([c["margin"] for c in self.calls()], [None, "98304"])

    def test_retry_failure_is_unresolved_and_clock_bound_is_not_retried(self):
        (result,) = self.run_jobs([Job("kill", 7)], "overshoot_again")
        self.assertEqual(result.status, UNRESOLVED)
        self.assertIn("after overshoot retry", result.detail)
        (self.root / "calls.jsonl").unlink()
        (result,) = self.run_jobs([Job("kill", 8)], "clock")
        self.assertEqual((result.status, result.retries, len(self.calls())), (UNRESOLVED, 0, 1))
        self.assertIn("clock bound exceeded", result.detail)

    def test_contradicting_result_and_busy_lease_are_unresolved(self):
        (result,) = self.run_jobs([Job("kill", 7)], "contradiction")
        self.assertEqual(result.status, UNRESOLVED)
        self.assertIn("contradicts", result.detail)
        (result,) = self.run_jobs([Job("kill", 8)], "lease")
        self.assertEqual(result.status, UNRESOLVED)
        self.assertIn("already owned", result.detail)

    def test_faults_count_only_guest_acknowledgements(self):
        (result,) = self.run_jobs([Job("kill", 7)])
        self.assertEqual([(f["stopped"], f["restarted"]) for f in result.faults], [(True, True), (True, False)])
        self.assertEqual(result.summary()["faults"], {"scheduled": 2, "stopped": 2, "restarted": 1})

    def test_checker_workload_without_a_result_is_unresolved(self):
        (result,) = self.run_jobs([Job("kill", 7)], "dial_error")
        self.assertEqual(result.status, UNRESOLVED)
        self.assertIn("RESULT error dial", result.detail)

    def test_failed_batch_is_never_accepted(self):
        (result,) = self.run_jobs([Job("kill", 7)], "restore_failed")
        self.assertEqual(result.status, UNRESOLVED)
        self.assertIn("restoration failed", result.detail)

    def test_batches_take_disjoint_core_ranges_and_smt_siblings(self):
        pairs = {2: {2, 3}, 3: {2, 3}, 4: {4, 5}, 5: {4, 5}}
        launcher = self.launcher(siblings=pairs.get)
        self.assertEqual(launcher._allocate(1, [2]), [4])
        self.assertEqual(launcher._allocate(2, []), [2, 3])
        self.assertIsNone(launcher._allocate(2, [3, 4]))
        jobs = [Job("kill", s) for s in range(6)] + [Job("race", 1)]
        results = self.run_jobs(jobs, siblings=pairs.get)
        self.assertEqual([r.status for r in results], [PASS] * 7)
        self.assertEqual(sorted(len(c["seeds"].split()) for c in self.calls()), [1, 2, 4])

    def test_inputs_are_hashed_and_resolved_from_an_artifacts_directory(self):
        other = self.base / "artifacts-pv"
        other.mkdir()
        for name in ("bzImage", "race.cpio"):
            (other / name).write_text("pv " + name)
        os.chdir(self.base)
        self.addCleanup(os.chdir, SIM)
        launcher = self.launcher(artifacts="artifacts-pv")
        world = json.loads(launcher.world("race").read_text())
        self.assertEqual(world["kernel"], str(other.resolve() / "bzImage"))
        self.assertEqual(sorted(launcher.identity("race")), ["initrd:race.cpio", "kernel", "world"])
        self.assertEqual(launcher.missing_inputs(["kill"]), [str(other.resolve() / "cluster.cpio")])
        # A symlinked artifact is matched by the world's file name, not its target's.
        (self.base / "artifacts/race.cpio").unlink()
        (self.base / "artifacts/race.cpio").symlink_to(self.base / "artifacts/cluster.cpio")
        world = json.loads(self.launcher(artifacts=other).world("race").read_text())
        self.assertEqual(world["machines"][0]["initrd"], str(other.resolve() / "race.cpio"))


class YamlSetupTests(unittest.TestCase):
    def test_fault_overlay_preserves_yaml_world_and_rejects_invalid_targets(self):
        from acceptance.prepare import apply_faults
        world = {"machines": [{"name": "n1", "initrd": "/prepared/image"}],
                 "duration_ns": 100, "end_on_exit": "n1", "network": {"latency_ns": 7}}
        schedule = {"members": ["n1"], "start_ns": 10, "end_ns": 80,
                    "gap_ns": [1, 2], "down_ns": [1, 2]}
        combined = apply_faults(world, schedule)
        self.assertEqual({k: v for k, v in combined.items() if k != "kill_restart"}, world)
        self.assertNotIn("kill_restart", world)
        for invalid in [dict(schedule, members=["missing"]), dict(schedule, end_ns=101),
                        dict(schedule, gap_ns=[0, 2])]:
            with self.assertRaises(ValueError):
                apply_faults(world, invalid)

    def test_oci_fault_acknowledgements_must_be_ordered_and_consumed_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp)
            (output / "n1.mgmt").write_text(
                "SEEDBOX_EFFECT start guest_ns=1\n"
                "SEEDBOX_EFFECT stop guest_ns=10\nSEEDBOX_EFFECT start guest_ns=20\n"
                "SEEDBOX_EFFECT stop guest_ns=30\n")
            faults = BatchLauncher.acknowledged(output, [
                {"member": "n1", "stop_ns": 9, "start_ns": 19},
                {"member": "n1", "stop_ns": 29, "start_ns": 39},
            ])
            self.assertEqual([(f["stopped"], f["restarted"]) for f in faults], [(True, True), (True, False)])
            self.assertEqual(faults[0]["stopped_guest_ns"], 10)
            self.assertEqual(faults[0]["restarted_guest_ns"], 20)

    def test_replay_accepts_a_changed_fresh_outcome_without_historical_pins(self):
        def changed(job, result, n):
            if job.scenario == "short-kill-3.5.2":
                result.status, result.workload_exit = FAIL, 1
                result.world_witness = "new-witness"
        result, _ = verdicts(FakeLauncher(changed), [("1 replay", checks.replay)])
        self.assertEqual(result["1"].status, PASS)


if __name__ == "__main__":
    unittest.main()
