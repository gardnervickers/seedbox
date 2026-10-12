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


if __name__ == "__main__":
    unittest.main()
