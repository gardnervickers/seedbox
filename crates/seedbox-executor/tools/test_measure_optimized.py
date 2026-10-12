import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("optimized", Path(__file__).with_name("measure-optimized.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class WarmAdmission(unittest.TestCase):
    def test_call17_checks_independent_result_count_and_charge(self):
        text = ("EXECUTOR_BENCH kind=Call17 native=false gate=false iterations=10000000 "
                "rounds=1 checksum=130000000 logical=170000002 payments=3400 "
                "remaining=49998 translations=4 warm_ns=123\n")
        self.assertEqual(module.validate_warm(text, "call17", "candidate")["payments"], 3400)
        for before, after in [("130000000", "130000001"), ("10000000", "9999999"),
                              ("170000002", "170000000"), ("3400", "3399"),
                              ("49998", "50000"), ("gate=false", "gate=true")]:
            with self.assertRaises(ValueError):
                module.validate_warm(text.replace(before, after), "call17", "candidate")
        for bad in [text + "EXECUTOR_FAIL scratch\n", text + text, ""]:
            with self.assertRaises(ValueError):
                module.validate_warm(bad, "call17", "candidate")

    def test_plan_keeps_all_historical_and_new_controls_in_five_rotations(self):
        self.assertEqual(set(module.BATCHES), set(module.jit.ROWS) |
                         {"compute", "short", "call17", "legacy-jit"})
        self.assertEqual(len(module.ORDERS), 5)
        for order in module.ORDERS:
            self.assertEqual(set(order), {"native", "dr", "baseline", "candidate"})
        self.assertTrue(all(n > 0 for n in module.BATCHES.values()))


if __name__ == "__main__":
    unittest.main()
