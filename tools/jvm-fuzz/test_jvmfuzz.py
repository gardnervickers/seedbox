"""Verdict logic of jvmfuzz.py on fixture result directories; no guest, engine or JVM."""
import argparse
import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import jvmfuzz  # noqa: E402


def framed(case, lines, code, hserr=()):
    text = f"@@CASE {case}\n" + "".join(f"{line}\n" for line in lines)
    if hserr:
        text += f"@@HSERR {case}\n" + "".join(f"@@| {line}\n" for line in hserr)
    return text + f"@@EXIT {case} {code}\n"


def case(cid, main, args, exit_code=0, det=True, probe=None):
    c = {"id": cid, "main": main, "args": args, "profile": "native" if probe else "default",
         "flags": ["-"], "det": det, "timeout": 10, "exit": exit_code}
    if probe:
        c["probe"] = probe
    return c


class Corpus:
    """A corpus directory with one batch per case: native runs, a simulation and a replay."""

    def __init__(self, root):
        self.root = Path(root)
        self.cases, self.batches = [], {}

    def add(self, c, native, sim=None, replay=None, replay_status="MATCH"):
        """`native`, `sim`, `replay`: (lines, exit[, hserr]) for this case, or None."""
        b = f"b{len(self.batches):03d}"
        self.cases.append(c)
        self.batches[b] = [c["id"]]
        for run in ("r1", "r2"):
            path = self.root / "native" / run / f"{b}.out"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(framed(c["id"], *native) + f"@@DONE {b}\n")
        if sim is not None:
            d = self.root / "sim" / b
            d.mkdir(parents=True)
            (d / "run.out").write_text(framed(c["id"], *sim) + f"@@DONE {b}\n")
            info = {"status": "PASS", "id": f"s{b}"}
            if replay_status is not None:
                info.update({"replay_status": replay_status, "replay_id": f"r{b}"})
                (d / "replay.out").write_text((framed(c["id"], *replay) if replay else "") + f"@@DONE {b}\n")
            (d / "result.json").write_text(json.dumps(info))

    def report(self):
        (self.root / "corpus.json").write_text(json.dumps(
            {"seed": 1, "tag": "t", "image": "jvmfuzz:t", "cases": self.cases, "batches": self.batches}))
        with contextlib.redirect_stdout(io.StringIO()):
            code = jvmfuzz.cmd_report(argparse.Namespace(corpus=str(self.root)))
        rows = json.loads((self.root / "report.json").read_text())["rows"]
        return code, {r["id"]: r["verdict"] for r in rows}


OK = (["value 1", "OK Arith 1f"], 0)


class ReportVerdicts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.corpus = Corpus(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def verdict(self, *args, **kwargs):
        self.corpus.add(*args, **kwargs)
        code, verdicts = self.corpus.report()
        (verdict,) = verdicts.values()
        return code, verdict

    def test_matching_output_and_replay_is_ok(self):
        self.assertEqual(self.verdict(case("Arith.1.default", "Arith", "1 1"), OK, OK, OK), (0, "OK"))

    def test_replay_mismatch_fails_every_case_of_the_batch_despite_equal_output(self):
        for status in ("MISMATCH", "NOT_COMPARABLE", "ERROR"):
            with self.subTest(status=status):
                corpus = Corpus(Path(self.tmp.name) / status)
                corpus.add(case("Arith.1.default", "Arith", "1 1"), OK, OK, OK, replay_status=status)
                corpus.add(case("Arith.2.default", "Arith", "2 1"), OK, OK, OK, replay_status="MATCH")
                code, verdicts = corpus.report()
                self.assertEqual(code, 1)
                self.assertEqual(verdicts, {"Arith.1.default": f"REPLAY_{status}", "Arith.2.default": "OK"})

    def test_replay_output_divergence_or_a_case_missing_from_the_replay(self):
        diverged = (["value 2", "OK Arith 1f"], 0)
        self.assertEqual(self.verdict(case("Arith.1.default", "Arith", "1 1"), OK, OK, diverged),
                         (1, "REPLAY_DIVERGE"))
        corpus = Corpus(Path(self.tmp.name) / "missing")
        corpus.add(case("Arith.1.default", "Arith", "1 1"), OK, OK, None)
        self.assertEqual(corpus.report(), (1, {"Arith.1.default": "REPLAY_DIVERGE"}))

    def test_exit_zero_without_ok_marker_is_a_simulation_failure(self):
        # Nondeterministic output is not compared with native, so only the marker can catch it.
        c = case("C.counters.1.native", "/fuzz/bin/cfz", "counters 1 1", det=False, probe="C.counters")
        cut = (["cycles 5"], 0)
        self.assertEqual(self.verdict(c, (["cycles 9", "OK counters"], 0), cut, cut), (1, "SIM_FAIL"))

    def test_native_runs_without_ok_marker_fail_the_probe(self):
        cut = (["value 1"], 0)
        self.assertEqual(self.verdict(case("Arith.1.default", "Arith", "1 1"), cut, cut, cut), (1, "NATIVE_FAIL+SIM_FAIL"))

    def test_unsupported_feature_ends_without_ok_and_passes(self):
        c = case("C.casp.1.native", "/fuzz/bin/cfz", "casp 1 1", probe="C.casp")
        unsupported = (["UNSUPPORTED lse"], 0)
        self.assertEqual(self.verdict(c, unsupported, unsupported, unsupported), (0, "OK"))

    def test_exit_status_must_match_exit_codes_even_when_output_matches(self):
        c = case("ExitMatrix.1_exit7.default", "ExitMatrix", "1 1 exit7", exit_code=7)
        out = ["mode exit7", "hook exit7"]
        self.assertEqual(self.verdict(c, (out, 7), (out, 0), (out, 0)), (1, "SIM_FAIL"))
        crash = case("Crash.1_segv-c2.default", "Crash", "1 1 segv-c2", exit_code=134, det=False)
        corpus = Corpus(Path(self.tmp.name) / "crash")
        corpus.add(crash, (["armed"], 134), (["armed"], 0), (["armed"], 0))
        self.assertEqual(corpus.report(), (1, {crash["id"]: "SIM_FAIL"}))

    def test_intentional_exits_pass_with_their_expected_status_and_no_marker(self):
        corpus = Corpus(self.tmp.name)
        exit7 = case("ExitMatrix.1_exit7.default", "ExitMatrix", "1 1 exit7", exit_code=7)
        out7 = (["mode exit7", "hook exit7"], 7)
        corpus.add(exit7, out7, out7, out7)
        daemons = case("ExitMatrix.2_daemons.default", "ExitMatrix", "2 1 daemons")
        out0 = (["mode daemons", "hook daemons"], 0)
        corpus.add(daemons, out0, out0, out0)
        crash = case("Crash.3_segv-c2.default", "Crash", "3 1 segv-c2", exit_code=134, det=False)
        corpus.add(crash, (["armed"], 134, ["# A fatal error"]), (["armed"], 134, ["# A fatal error"]),
                   (["armed"], 134, ["# A fatal error"]))
        halt = case("Crash.4_halthook.default", "Crash", "4 1 halthook", exit_code=3, det=False)
        corpus.add(halt, (["hook"], 3), (["hook"], 3), (["hook"], 3))
        self.assertEqual(corpus.report(), (0, {exit7["id"]: "OK", daemons["id"]: "OK",
                                               crash["id"]: "OK_NONDET", halt["id"]: "OK_NONDET"}))


def jcstress_run(text):
    with tempfile.NamedTemporaryFile("w", suffix=".out", delete=False) as f:
        f.write(text)
    try:
        return jvmfuzz.jcstress_summary(Path(f.name))
    finally:
        Path(f.name).unlink()


PASSED = "(Results: 12 planned; 12 passed, 0 failed, 0 soft errs, 0 hard errs)\n@@EXIT jcstress 0\n"


class JcstressVerdicts(unittest.TestCase):
    def test_complete_passing_run(self):
        self.assertEqual(jvmfuzz.jcstress_state(jcstress_run(PASSED)), "PASS")

    def test_zero_selected_tests_is_not_a_pass(self):
        run = jcstress_run("(Results: 0 planned; 0 passed, 0 failed, 0 soft errs, 0 hard errs)\n@@EXIT jcstress 0\n")
        self.assertEqual(jvmfuzz.jcstress_state(run), "NO_TESTS")
        self.assertTrue(jvmfuzz.jcstress_bad(None, run, "MATCH", "MATCH"))

    def test_missing_summary_or_exit_is_truncated(self):
        for text in ("@@EXIT jcstress 0\n", "(Results: 12 planned; 3 passed, 0 failed, 0 soft errs, 0 hard errs)\n"):
            with self.subTest(text=text):
                run = jcstress_run(text)
                self.assertEqual(jvmfuzz.jcstress_state(run), "TRUNCATED")
                self.assertTrue(jvmfuzz.jcstress_bad(jcstress_run(PASSED), run, "MATCH", "MATCH"))

    def test_partial_pass_failures_and_crashes(self):
        partial = "(Results: 12 planned; 11 passed, 0 failed, 0 soft errs, 0 hard errs)\n@@EXIT jcstress 0\n"
        self.assertEqual(jvmfuzz.jcstress_state(jcstress_run(partial)), "FAIL")
        failed = "  [FAILED] o.s.T\n(Results: 12 planned; 11 passed, 1 failed, 0 soft errs, 0 hard errs)\n@@EXIT jcstress 1\n"
        run = jcstress_run(failed)
        self.assertEqual((jvmfuzz.jcstress_state(run), run["failing_tests"]), ("FAIL", ["o.s.T"]))
        self.assertEqual(jvmfuzz.jcstress_state(jcstress_run("# A fatal error\n" + PASSED)), "CRASH")

    def test_group_fails_on_replay_mismatch_or_planned_difference(self):
        ok = jcstress_run(PASSED)
        self.assertFalse(jvmfuzz.jcstress_bad(ok, ok, "MATCH", "MATCH"))
        self.assertFalse(jvmfuzz.jcstress_bad(ok, ok, None, "-"))
        for status in ("MISMATCH", "NOT_COMPARABLE", "ERROR"):
            self.assertTrue(jvmfuzz.jcstress_bad(ok, ok, status, "MATCH"))
        self.assertTrue(jvmfuzz.jcstress_bad(ok, ok, "MATCH", "DIVERGE"))
        fewer = jcstress_run(PASSED.replace("12 planned; 12 passed", "10 planned; 10 passed"))
        self.assertTrue(jvmfuzz.jcstress_bad(ok, fewer, "MATCH", "MATCH"))


if __name__ == "__main__":
    unittest.main()
