"""Independent timing-admission regressions; no process or VM execution."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("measure_jit", Path(__file__).with_name("measure-jit.py"))
measure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measure)


class MeasurementAdmission(unittest.TestCase):
    def record(self, **changes):
        fields = dict(kind="Patch", native="false", diagnostic="false", rounds=64,
                      calls=4096, publications=64, store_width=4, checksum=115712,
                      checks=256, logical=70144, payments=1, remaining=29856,
                      total_ns=100, write_barrier_ns=10, index_gate_ns=10,
                      first_ns=20, warm_ns=20, translations=69, resolutions=69,
                      invalidated=64, neighbor_reuse=64, pinned_bytes=0, live=5,
                      overlap_probes=320, gate_probes=768)
        fields.update(changes)
        return "JIT_RESULT " + " ".join(f"{k}={v}" for k, v in fields.items())

    def test_exact_patch_work_and_ledger(self):
        self.assertEqual(measure.validate(self.record(), "patch-high", "candidate")["calls"], 4096)

    def test_wrong_version_count_ledger_or_retirement_fails(self):
        for field, bad in [("checksum", 115713), ("calls", 4095), ("publications", 63),
                           ("checks", 255), ("remaining", 29857), ("neighbor_reuse", 63),
                           ("translations", 68), ("pinned_bytes", 4096), ("payments", 2)]:
            with self.subTest(field=field), self.assertRaises(ValueError):
                measure.validate(self.record(**{field: bad}), "patch-high", "candidate")

    def test_failed_or_instrumented_sample_is_not_timing(self):
        for text in [self.record() + "\nEXECUTOR_FAIL bad", self.record(diagnostic="true"),
                     self.record(native="true"), ""]:
            with self.assertRaises(ValueError):
                measure.validate(text, "patch-high", "candidate")

    def test_nested_clocks_cannot_be_summed_as_disjoint(self):
        with self.assertRaises(ValueError):
            measure.validate(self.record(total_ns=59), "patch-high", "candidate")


if __name__ == "__main__":
    unittest.main()
