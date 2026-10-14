"""Checker tests for nats_jepsen.py; run with `python3 -m unittest` in this directory."""
import unittest

from nats_jepsen import check, ranges


def acked(*pairs):
    return {value: (seq, float(seq)) for value, seq in pairs}


class CheckTest(unittest.TestCase):
    def test_every_node_holds_every_ack(self):
        acks = acked(("0-0", 1), ("1-0", 2))
        reads = {n: {"messages": {1: "0-0", 2: "1-0"}} for n in ("n1", "n2")}
        result, _, violation = check(acks, acks, reads)
        self.assertFalse(violation)
        self.assertEqual(result["lost"], 0)

    def test_lost_everywhere_is_lost_not_divergent(self):
        acks = acked(("0-0", 1), ("0-1", 2), ("0-2", 3))
        reads = {n: {"messages": {1: "0-0", 3: "0-2"}} for n in ("n1", "n2")}
        result, details, violation = check(acks, acks, reads)
        self.assertTrue(violation)
        self.assertEqual((result["lost"], result["divergent"]), (1, 0))
        self.assertEqual(result["lost_classes"], {"lost-middle": 1})
        self.assertEqual(details["lost_seqs"], [2])

    def test_divergence_and_sequence_conflicts(self):
        acks = acked(("0-0", 1), ("0-1", 2))
        attempted = dict(acks, **{"1-0": None})
        reads = {"n1": {"messages": {1: "0-0", 2: "0-1"}}, "n2": {"messages": {1: "0-0", 2: "1-0"}}}
        result, details, violation = check(attempted, acks, reads)
        self.assertTrue(violation)
        self.assertEqual((result["lost"], result["divergent"], result["seq_conflicts"]), (0, 1, 1))
        self.assertEqual(details["missing_by_node"], {"n2": [2]})

    def test_one_sequence_acknowledged_twice(self):
        acks = {"0-0": (1, 0.0), "1-0": (1, 1.0)}
        reads = {"n1": {"messages": {1: "1-0"}}}
        result, details, violation = check(acks, acks, reads)
        self.assertTrue(violation)
        self.assertEqual(result["acked_seq_reused"], 1)
        self.assertEqual(details["acked_seq_reused_examples"], {1: ["0-0", "1-0"]})

    def test_missing_stream(self):
        acks = acked(("0-0", 1))
        result, _, violation = check(acks, acks, {"n1": {"stream_missing": True}})
        self.assertTrue(violation)
        self.assertEqual(result["stream_missing"], ["n1"])
        self.assertEqual(result["lost"], 1)

    def test_ranges(self):
        self.assertEqual(ranges([5, 1, 2, 3, 7, 3]), ["1-3", 5, 7])


if __name__ == "__main__":
    unittest.main()
