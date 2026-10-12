import unittest

from run_ic_kernel_write import classify


class KernelWriteEvidenceTests(unittest.TestCase):
    good = "ic-kernel-write: before=1 after=2 bytes=0x52800040 failures=0\n"
    stale = "ic-kernel-write: before=1 after=1 bytes=0x52800040 failures=1\n"

    def test_complete_native_result(self):
        self.assertEqual(classify(self.good, "", 0, False), "pass")

    def test_actual_stale_translation_is_failure(self):
        self.assertEqual(classify(self.stale, "", 1, False), "stale-translation")

    def test_missing_or_duplicate_result_is_not_pass(self):
        for text in ("", self.good + self.good):
            self.assertEqual(classify(text, "", 0, False), "invalid-output")

    def test_wrong_bytes_or_missing_initial_execution_is_not_stale_evidence(self):
        for text in (self.stale.replace("52800040", "52800020"),
                     self.stale.replace("before=1", "before=0")):
            self.assertEqual(classify(text, "", 1, False), "unexpected-result")

    def test_crash_after_result_is_not_pass(self):
        self.assertEqual(classify(self.good, "", -11, False), "unexpected-result")
        self.assertEqual(classify(self.good, "Internal Error: assertion\n", 0, False),
                         "fatal")

    def test_timeout_after_result_is_not_pass(self):
        self.assertEqual(classify(self.good, "", 0, True), "timeout")


if __name__ == "__main__":
    unittest.main()
