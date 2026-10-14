"""EDN export of recorded histories: python3 -m unittest test_to_edn.py"""
import unittest

import to_edn


class Export(unittest.TestCase):
    def test_invocations_completions_and_ties(self):
        txns = [
            {"i": 0, "p": 3, "type": "ok", "t0": 1.0, "t1": 2.0, "value": [["append", 1, 1], ["r", 2, [4, 5]]]},
            # Same process, invoked at the instant its previous transaction completed.
            {"i": 1, "p": 3, "type": "fail", "t0": 2.0, "t1": 2.0, "value": [["r", 1, None], ["append", 2, 6]]},
            {"i": 2, "p": 0, "type": "info", "t0": 1.5, "t1": 9.0, "value": [["r", 7, None]]},
            {"i": 3, "p": "final", "type": "ok", "t0": 10.0, "t1": 10.0, "value": [["r", 1, []]]},
        ]
        lines = list(to_edn.ops(txns))
        self.assertEqual(lines, [
            "{:index 0, :time 1000000000, :type :invoke, :process 3, :f :txn, :value [[:append 1 1] [:r 2 nil]]}",
            "{:index 1, :time 1500000000, :type :invoke, :process 0, :f :txn, :value [[:r 7 nil]]}",
            "{:index 2, :time 2000000000, :type :ok, :process 3, :f :txn, :value [[:append 1 1] [:r 2 [4 5]]]}",
            "{:index 3, :time 2000000000, :type :invoke, :process 3, :f :txn, :value [[:r 1 nil] [:append 2 6]]}",
            "{:index 4, :time 2000000000, :type :fail, :process 3, :f :txn, :value [[:r 1 nil] [:append 2 6]]}",
            "{:index 5, :time 9000000000, :type :info, :process 0, :f :txn, :value [[:r 7 nil]]}",
            "{:index 6, :time 10000000000, :type :invoke, :process 4, :f :txn, :value [[:r 1 nil]]}",
            "{:index 7, :time 10000000000, :type :ok, :process 4, :f :txn, :value [[:r 1 []]]}",
        ])


if __name__ == "__main__":
    unittest.main()
