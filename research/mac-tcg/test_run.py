import copy
import json
import unittest

from run import classify, initramfs, seed_bytes


def completed_events():
    events = [
        {"kind": "entropy", "phase": "before_seed", "count": -1, "errno": 11},
        {"kind": "entropy", "phase": "payload", "count": 32, "errno": 0,
         "bytes": "ab" * 32, "at_random": "cd" * 16},
        {"kind": "cpu", "rdrand": 0, "rdseed": 0}, {"kind": "start"},
    ]
    for i in range(3):
        events += [
            {"kind": "cpu_work", "round": i, "before_ns": 1, "after_ns": 2,
             "tsc_before": 1, "tsc_after": 2},
            {"kind": "sleep", "round": i, "before_ns": 1,
             "after_ns": 1 + (i + 1) * 10000000, "requested_ns": (i + 1) * 10000000},
        ]
    events += [{"kind": "poll", "count": 123, "before_ns": 1, "after_ns": 1000001},
               {"kind": "result", "status": "PASS", "reason": "completed"}]
    return events


def console(events):
    return b"kernel log\r\n" + b"".join(b"SBX " + json.dumps(e).encode() + b"\r\n" for e in events)


class Results(unittest.TestCase):
    def test_qemu_exit_zero_is_not_a_pass(self):
        self.assertEqual(classify(b"reboot: Power down\n", 0, False)[0], "INCOMPLETE")

    def test_terminal_result_requires_checks(self):
        events = completed_events()
        self.assertEqual(classify(console(events), 0, False)[0], "PASS")
        del events[4]
        self.assertEqual(classify(console(events), 0, False)[0], "ERROR")

    def test_errors_and_timeouts_override_result(self):
        data = console(completed_events())
        self.assertEqual(classify(data, 1, False)[0], "ERROR")
        self.assertEqual(classify(data, -9, True)[0], "TIMEOUT")
        self.assertEqual(classify(data + b"SBX {", -9, True)[0], "TIMEOUT")
        self.assertEqual(classify(data + b"SBX invalid\n", 0, False)[0], "ERROR")

    def test_ambiguous_result_rejected(self):
        events = completed_events()
        events.append(events[-1])
        self.assertEqual(classify(console(events), 0, False)[0], "ERROR")

    def test_omitted_entropy_and_bad_timing_rejected(self):
        for index, field, value in [(1, "count", -1), (1, "bytes", "00"),
                                     (2, "rdrand", 1), (4, "after_ns", 1),
                                     (5, "after_ns", 3), (10, "after_ns", 3)]:
            with self.subTest(index=index, field=field):
                events = copy.deepcopy(completed_events())
                events[index][field] = value
                self.assertEqual(classify(console(events), 0, False)[0], "ERROR")

    def test_application_failure_stays_failure(self):
        events = completed_events()
        events[-1] = {"kind": "result", "status": "FAIL", "reason": "planted_failure"}
        self.assertEqual(classify(console(events), 0, False)[0], "FAIL")


class Archive(unittest.TestCase):
    def test_archive_has_canonical_seed_and_executable_permissions(self):
        data = initramfs(b"ELF probe", 42, "none")
        entries = {}
        offset = 0
        while True:
            self.assertEqual(data[offset:offset + 6], b"070701")
            fields = [int(data[offset + 6 + n * 8:offset + 14 + n * 8], 16) for n in range(13)]
            offset += 110
            name = data[offset:offset + fields[11] - 1].decode()
            offset = (offset + fields[11] + 3) & ~3
            body = data[offset:offset + fields[6]]
            offset = (offset + fields[6] + 3) & ~3
            self.assertEqual(fields[2:4], [0, 0])
            self.assertEqual(fields[5], 0)
            if name == "TRAILER!!!":
                break
            entries[name] = (fields[1], body)
        self.assertEqual(entries["init"], (0o100755, b"ELF probe"))
        self.assertEqual(entries["seed"][1], seed_bytes(42, "linux-pool"))
        self.assertNotEqual(entries["seed"][1], seed_bytes(43, "linux-pool"))
        self.assertNotEqual(seed_bytes(42, "linux-pool"), seed_bytes(42, "qemu"))
        self.assertEqual(data, initramfs(b"ELF probe", 42, "none"))
        self.assertNotEqual(data, initramfs(b"ELF probe", 42, "no-entropy"))


if __name__ == "__main__":
    unittest.main()
