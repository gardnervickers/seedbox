"""Checker tests for kafka_jepsen.py: python3 -m unittest (in this directory)."""
import unittest

import kafka_jepsen as kj


def op(n, kind, value, **extra):
    return dict(type=kind, f="txn", op=n, process=0, value=value, **extra)


def final(logs, status="complete"):
    return dict(type="final", status=status, logs={str(k): [list(p) for p in v] for k, v in logs.items()})


class TxnChecks(unittest.TestCase):
    def test_clean_history(self):
        events = [op(0, "ok", [["send", 0, [0, 10]], ["send", 1, [0, 11]]]),
                  op(1, "fail", [["send", 0, [1, 12]], ["send", 1, 13]], error="intentional"),
                  op(2, "ok", [["poll", {"0": [[0, 10]]}]]),
                  final({0: [(0, 10)], 1: [(0, 11)]})]
        result, details, violation = kj.check(events, "txn")
        self.assertFalse(violation, (result, details))
        self.assertEqual(result["records"], 2)

    def test_aborted_read_from_delayed_commit(self):
        # A failed transaction whose first send was committed by a stale EndTxn.
        events = [op(0, "ok", [["send", 0, [0, 10]]]),
                  op(1, "fail", [["send", 0, [1, 11]], ["send", 1, [0, 12]]]),
                  op(2, "ok", [["poll", {"0": [[1, 11]]}]]),
                  final({0: [(0, 10), (1, 11)], 1: []})]
        result, details, violation = kj.check(events, "txn")
        self.assertTrue(violation)
        self.assertEqual(result["aborted_read"], 1)
        self.assertEqual(result["aborted_poll"], 1)
        self.assertEqual(result["torn"], 1)
        self.assertEqual(details["aborted_read"][0]["visible"], [(0, 11)])

    def test_lost_write_from_delayed_abort(self):
        # An ok transaction whose first send was aborted by a stale EndTxn: torn and lost.
        events = [op(0, "ok", [["send", 0, [0, 10]], ["send", 1, [0, 11]]]),
                  final({0: [], 1: [(0, 11)]})]
        result, details, violation = kj.check(events, "txn")
        self.assertTrue(violation)
        self.assertEqual((result["lost"], result["torn"]), (1, 1))
        self.assertEqual(details["lost"][0]["lost"], [(0, 10)])

    def test_info_may_go_either_way(self):
        events = [op(0, "info", [["send", 0, [0, 10]]]), op(1, "info", [["send", 0, 11]]),
                  final({0: [(0, 10)]})]
        result, _, violation = kj.check(events, "txn")
        self.assertFalse(violation)
        self.assertEqual(result["ops"], {"info": 2, "info-visible": 1, "info-invisible": 1})

    def test_inconsistent_offsets_and_duplicates(self):
        events = [op(0, "ok", [["send", 0, [0, 10]]]), op(1, "ok", [["send", 0, [1, 11]]]),
                  final({0: [(0, 11), (1, 11)]})]
        result, _, violation = kj.check(events, "txn")
        self.assertTrue(violation)
        self.assertEqual(result["duplicates"], 1)
        self.assertEqual(result["lost"], 1)
        self.assertGreaterEqual(result["inconsistent"], 1)

    def test_incomplete_final_read_does_not_claim_loss(self):
        events = [op(0, "ok", [["send", 0, [0, 10]]]), final({0: []}, status="timeout")]
        result, _, violation = kj.check(events, "txn")
        self.assertFalse(violation)
        self.assertEqual(result["final"], "timeout")

    def test_g0_is_reported_but_not_a_violation(self):
        events = [op(0, "ok", [["send", 0, [0, 10]], ["send", 1, [1, 11]]]),
                  op(1, "ok", [["send", 0, [1, 12]], ["send", 1, [0, 13]]]),
                  final({0: [(0, 10), (1, 12)], 1: [(0, 13), (1, 11)]})]
        result, _, violation = kj.check(events, "txn")
        self.assertFalse(violation)
        self.assertEqual(result["elle"].get("G0"), 1)


class ProduceChecks(unittest.TestCase):
    def test_lost_acknowledged_produce(self):
        events = [dict(type="ok", f="send", k=0, v=1, offset=0), dict(type="ok", f="send", k=0, v=2, offset=1),
                  dict(type="info", f="send", k=0, v=3), final({0: [(0, 1)]})]
        result, details, violation = kj.check(events, "produce")
        self.assertTrue(violation)
        self.assertEqual(result["lost"], 1)
        self.assertEqual(details["lost"][0]["value"], 2)


if __name__ == "__main__":
    unittest.main()
