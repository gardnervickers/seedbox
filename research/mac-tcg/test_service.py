import unittest

from service import archive, classify, contents, fixture, scenario


class ServiceResults(unittest.TestCase):
    def test_debug_exit_status_is_bound_to_guest_completion(self):
        text = 'RESULT pass acked=128 failed=0\nINIT exit=0\n'
        self.assertEqual(classify(text, '', 1, False, 'healthy')[0], 'PASS')
        self.assertEqual(classify(text, '', 0, False, 'healthy')[0], 'ERROR')
        self.assertEqual(classify('INIT exit=0\n', '', 1, False, 'healthy')[0], 'INCOMPLETE')

    def test_incomplete_and_timeout_do_not_become_application_failure(self):
        text = 'RESULT incomplete acked=12 target=128\nINIT exit=4\n'
        self.assertEqual(classify(text, '', 9, False, 'healthy')[0], 'INCOMPLETE')
        self.assertEqual(classify(text, '', -9, True, 'healthy')[0], 'TIMEOUT')
        text = 'RESULT fail missing k/99/000000\nINIT exit=1\n'
        self.assertEqual(classify(text, '', 3, False, 'negative')[0], 'FAIL')

    def test_fault_application_and_live_drop_evidence_required(self):
        text = 'RESULT pass acked=128 failed=0\nINIT exit=0\n'
        requested = ['partition n1 n2', 'heal n1 n2', 'stop n2', 'start n2']
        applied = ['partition n1 n2', 'heal n1 n2', 'stop n2 stopped', 'start n2 launched']
        mgmt = 'DROP_COUNTER_BEGIN\n(dropped 2, overlimits 0)\nDROP_COUNTER_END\n'
        for i, (r, a) in enumerate(zip(requested, applied)):
            mgmt += f'REQUEST {r} target_ns={100+i*10} guest_ns={101+i*10}\nFAULT {a} guest_ns={102+i*10}\n'
        self.assertEqual(classify(text, mgmt, 1, False, 'fault')[0], 'PASS')
        self.assertEqual(classify(text, '', 1, False, 'fault')[0], 'INCOMPLETE')
        self.assertEqual(classify(text, mgmt.replace('dropped 2', 'dropped 0'), 1, False, 'fault')[0], 'INCOMPLETE')
        self.assertEqual(classify(text, mgmt.replace('guest_ns=102', 'guest_ns=99'), 1, False, 'fault')[0], 'ERROR')
        self.assertEqual(classify(text, mgmt.replace('guest_ns=102', 'guest_ns=100'), 1, False, 'fault')[0], 'ERROR')
        lines = mgmt.splitlines()
        lines[3], lines[4] = lines[4], lines[3]
        self.assertEqual(classify(text, '\n'.join(lines), 1, False, 'fault')[0], 'ERROR')

    def test_queued_requests_do_not_require_previous_ack_before_next_target(self):
        text = 'RESULT pass acked=128 failed=0\nINIT exit=0\n'
        requests = ['partition n1 n2', 'heal n1 n2', 'stop n2', 'start n2']
        acks = ['partition n1 n2', 'heal n1 n2', 'stop n2 stopped', 'start n2 launched']
        mgmt = 'DROP_COUNTER_BEGIN\n(dropped 2)\nDROP_COUNTER_END\n'
        mgmt += ''.join(f'REQUEST {r} target_ns={100+i} guest_ns={100+i}\n' for i, r in enumerate(requests))
        mgmt += ''.join(f'FAULT {a} guest_ns={200+i}\n' for i, a in enumerate(acks))
        self.assertEqual(classify(text, mgmt, 1, False, 'fault')[0], 'PASS')
        # Two writers can be preempted between sampling a time and serial I/O.
        queued = mgmt.replace('guest_ns=200', 'guest_ns=102')
        self.assertEqual(classify(text, queued, 1, False, 'fault')[0], 'PASS')

    def test_adapter_preserves_merged_fault_implementation(self):
        adapted = fixture('fault').decode()
        self.assertIn('. /bin/network.sh', adapted)
        self.assertIn('. /bin/members.sh', adapted)
        self.assertIn('< /run/seedbox-control', adapted)
        self.assertNotIn('< /dev/ttyS1', adapted)
        self.assertIn('/tmp/scenario.done', adapted)
        a, events = scenario(1009)
        self.assertEqual(a, scenario(1009)[0])
        self.assertNotEqual(events, scenario(1010)[1])
        self.assertEqual([e[0] for e in events], sorted(e[0] for e in events))

    def test_overlay_archive_roundtrip(self):
        entries = [('modules', 0o040755, b''), ('init', 0o100755, b'executable\0bytes')]
        self.assertEqual(contents(archive(entries)), {'modules': b'', 'init': b'executable\0bytes'})


if __name__ == '__main__':
    unittest.main()
