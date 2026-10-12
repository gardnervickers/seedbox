import unittest

from check_ic_dependencies import check


def evidence():
    text = []
    emitted = {0: [0, 4, 32, 128, 4092], 1: [16], 2: [0, 4], 3: [128], 4: [4092], 5: [32]}
    deleted = {2: [0, 4], 3: [128], 4: [4092], 5: [0, 4, 16, 32, 128, 4092]}
    for phase in range(7):
        text.append(f"IC_PHASE {phase}")
        for offset in deleted.get(phase, []):
            text.append(f"IC_DELETE phase={phase} offset={offset}")
        for offset in emitted.get(phase, []):
            text.append(f"IC_BLOCK phase={phase} offset={offset} reconstruct=0 trace=0")
    text += ["ic-dependencies: checks=18 failures=0", "seedbox-ic-images: ic=24 admissions=5 "
             "scanned_bytes=29036 scan_us=39593 emitted=2120 selected=1676 reclaimed=2120"]
    return "\n".join(text)


class IcDependenciesEvidence(unittest.TestCase):
    def test_exact_selection_and_reclamation(self):
        self.assertEqual(check(evidence())["admissions"], 5)

    def test_reconstruction_is_not_an_emission(self):
        check(evidence() + "\nIC_BLOCK phase=1 offset=32 reconstruct=1 trace=0")
        with self.assertRaises(ValueError):
            check(evidence().replace("offset=16 reconstruct=0", "offset=16 reconstruct=1"))

    def test_recreated_neighbor_is_not_reuse(self):
        with self.assertRaises(ValueError):
            check(evidence() + "\nIC_BLOCK phase=1 offset=32 reconstruct=0 trace=0")

    def test_missing_deletion_or_reclamation_fails(self):
        for changed in (evidence().replace("IC_DELETE phase=2 offset=4", ""),
                        evidence().replace("reclaimed=2120", "reclaimed=2119")):
            with self.assertRaises(ValueError):
                check(changed)

    def test_success_line_cannot_hide_a_crash(self):
        with self.assertRaises(ValueError):
            check(evidence() + "\nInternal Error: bad context")


if __name__ == "__main__":
    unittest.main()
