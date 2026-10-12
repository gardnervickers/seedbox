import unittest
from check_parked_publication import check


def evidence():
    lines = ["BEGIN case=restartable iterations=20 target=0x1000 poll=0x1008 x28=0x2000"]
    for i in range(1, 21):
        lines += [f"PUBLICATION iteration={i} req=1 ack=1 done={i} latency_us=10",
                  f"ITERATION case=restartable iteration={i} faults=1 signals=1 bad=0 "
                  f"body={0x600+i} all_gpr=31 all_q=32 state=valid"]
    lines += ["END case=restartable completed=20 faults=20 signals=20 bad=0",
              "seedbox-ic-images: ic=168 admissions=41 scanned_bytes=80000 scan_us=100000 "
              "emitted=9000 selected=8300 reclaimed=9000"]
    return "\n".join(lines)


class ParkedPublicationEvidence(unittest.TestCase):
    def test_complete(self):
        self.assertEqual(check(evidence(), 0)["faults"], 20)

    def test_partial_pass_then_failure_is_rejected(self):
        for code in (1, 124, 137, -11):
            with self.assertRaises(ValueError): check(evidence(), code)
        with self.assertRaises(ValueError): check(evidence() + "\nInternal Error", 0)

    def test_missing_ack_fault_state_and_early_release(self):
        for before, after in [("ack=1", "ack=0"), ("req=1", "req=0"),
                              ("faults=1", "faults=0"), ("all_q=32", "all_q=31"),
                              ("body=1537", "body=0"), ("poll=0x1008", "poll=0x0")]:
            with self.assertRaises(ValueError): check(evidence().replace(before, after), 0)

    def test_iteration_and_record_reclamation(self):
        for text in (evidence().replace("reclaimed=9000", "reclaimed=8999"),
                     evidence().replace("iteration=20", "iteration=19"),
                     "\n".join(evidence().splitlines()[1:]),
                     evidence().replace("END case=restartable", "MISSING")):
            with self.assertRaises(ValueError): check(text, 0)


class ProgressLedgerEvidence(unittest.TestCase):
    def evidence(self):
        text = evidence().replace('case=restartable', 'case=progress')
        for i in range(1, 21):
            initial, calls = (2, 1) if i % 2 else (-99998, 3)
            text += (f'\nLEDGER iteration={i} old_charge=5 fresh_charge=7 initial={initial} '
                     f'after_add={initial-5+50000} payments={calls} before_signal=50002 '
                     'released=1 deferred=1 fresh_before=49000 fresh_after=48993')
        return text

    def test_actual_old_ledger(self):
        self.assertEqual(check(self.evidence(), 0)['case'], 'progress')
        for old, new in [('payments=3', 'payments=2'), ('old_charge=5', 'old_charge=7'),
                         ('before_signal=50002', 'before_signal=50004'),
                         ('after_add=-50003', 'after_add=-100003'), ('deferred=1', 'deferred=0'), ('fresh_after=48993', 'fresh_after=48995')]:
            with self.assertRaises(ValueError): check(self.evidence().replace(old, new), 0)


if __name__ == "__main__":
    unittest.main()
