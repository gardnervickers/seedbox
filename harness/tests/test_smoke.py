"""Portable smoke-oracle controls; no device access, OCI preparation or guests."""
import copy
import hashlib
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HERE / 'smoke'))
import owned
spec = importlib.util.spec_from_file_location('amd_smoke', HERE / 'smoke/amd.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class SmokeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.state = self.root / 'state'
        self.run = self.state / 'runs' / 'attempt'
        self.run.mkdir(parents=True)
        (self.state / 'objects/sha256').mkdir(parents=True)
        def put(raw):
            digest = hashlib.sha256(raw).hexdigest()
            (self.state / 'objects/sha256' / digest).write_bytes(raw)
            return digest
        opaque = b'\xff\x00not a JSON event\n'
        item = put(b'input')
        log = put(opaque)
        self.record = {'outcome': 'PASS', 'error': None, 'allow_engine_change': False,
            'identity': {'inputs': {'kernel': item, 'profile': {'engine': item},
                'auxiliary': {'config': item}, 'guests': {'server': item},
                'images': {'server': {'manifest': item, 'config': item, 'layers': [item]}}}},
            'run_id': item, 'report': {'full_state': [{'digest': 'state'}], 'witness': 'witness', 'faults': [], 'workload_exit': 0, 'reason': 'COMPLETED'}, 'omitted_logs': {},
            'logs': {'machines/server.console': log, 'machines/server.mgmt': log},
            'replay': {'status': 'MATCH', 'outcome_matches': True, 'witness_matches': True,
                'full_state_matches': True, 'fault_history_matches': True}}
        for name in self.record['logs']:
            path = self.run / 'output' / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(opaque)

    def verify(self):
        return smoke.verify_record(self.record, self.run, self.state)

    def test_opaque_bytes_preserved_without_application_parser(self):
        streams = self.verify()
        smoke.compare(self.record, copy.deepcopy(self.record), streams, streams)

    def test_corrupt_and_missing_retained_objects_refuse(self):
        path = self.state / 'objects/sha256' / self.record['run_id']
        path.write_bytes(b'corrupt')
        with self.assertRaisesRegex(RuntimeError, 'missing or corrupt'):
            self.verify()
        path.unlink()
        with self.assertRaises(FileNotFoundError):
            self.verify()

    def test_omission_and_missing_machine_stream_refuse(self):
        self.record['omitted_logs'] = {'machines/server.console': 'cap'}
        with self.assertRaisesRegex(RuntimeError, 'complete retained logs'):
            self.verify()
        self.record['omitted_logs'] = {}
        del self.record['logs']['machines/server.console']
        with self.assertRaisesRegex(RuntimeError, 'missing opaque'):
            self.verify()

    def test_same_identity_is_required_even_when_cli_claims_match(self):
        replay = copy.deepcopy(self.record)
        replay['identity']['seed'] = 9
        with self.assertRaisesRegex(RuntimeError, 'identity drifted'):
            smoke.compare(self.record, replay, {}, {})

    def test_state_fault_and_witness_mismatch_cannot_be_masked(self):
        for key in ('full_state_matches', 'fault_history_matches', 'witness_matches'):
            replay = copy.deepcopy(self.record)
            replay['replay'][key] = False
            with self.assertRaisesRegex(RuntimeError, 'incomplete comparison'):
                smoke.compare(self.record, replay, {}, {})
        with self.assertRaisesRegex(RuntimeError, 'opaque guest streams differ'):
            smoke.compare(self.record, self.record, {'console': 'one'}, {'console': 'two'})

    def test_false_match_cannot_hide_actual_guest_state_change(self):
        replay = copy.deepcopy(self.record)
        replay['report']['full_state'][0]['digest'] = 'different'
        with self.assertRaisesRegex(RuntimeError, 'deterministic report'):
            smoke.compare(self.record, replay, {}, {})

    def test_no_full_state_or_engine_override_refuses(self):
        self.record['report']['full_state'] = None
        with self.assertRaisesRegex(RuntimeError, 'full state'):
            self.verify()
        self.record['allow_engine_change'] = True
        with self.assertRaisesRegex(RuntimeError, 'genuinely PASS'):
            self.verify()

    def test_performance_change_must_be_exact_and_default_unchanged(self):
        base = {'performance_parameters': {'SEEDBOX_MARGIN': 'adaptive', 'SEEDBOX_UNDO': '256'}}
        changed = copy.deepcopy(base)
        changed['performance_parameters']['SEEDBOX_MARGIN'] = '32768'
        smoke.verify_performance(base, changed, 'SEEDBOX_MARGIN', '32768')
        for replay in (base, {'performance_parameters': {'SEEDBOX_UNDO': '256'}}):
            with self.assertRaises(RuntimeError):
                smoke.verify_performance(base, replay, 'SEEDBOX_MARGIN', '32768')
        with self.assertRaises(RuntimeError):
            smoke.verify_performance(base, changed)

    def test_public_result_must_be_unique_and_owned(self):
        self.assertEqual(smoke.result_directory(f'PASS: {self.run}\n', self.state), self.run.resolve())
        with self.assertRaisesRegex(RuntimeError, 'newly owned attempt'):
            smoke.result_directory(f'PASS: {self.run}\n', self.state, self.root)
        for text in (f'PASS: {self.run}\nPASS: {self.run}\n', 'FAIL: /bad\n', f'PASS: {self.root}\n'):
            with self.assertRaises(RuntimeError):
                smoke.result_directory(text, self.state)

    def test_wrong_kernel_refusal_stops_before_pair_or_guest(self):
        args = type('Args', (), dict(kernel='kernel', engine='engine', guest_init='init',
                                    busybox='busybox', cpu=8, cli='cli', tool_path=''))()
        campaign = smoke.Campaign(args, self.root, 1e100)
        with patch.object(campaign, 'invoke', return_value=(0, '', '')), patch.object(smoke, 'pair') as pair:
            with self.assertRaisesRegex(RuntimeError, 'required policy refusal'):
                campaign.run()
            pair.assert_not_called()

    def test_cleanup_collected_before_bad_exit_assertion(self):
        args = type('Args', (), dict(tool_path=''))()
        campaign = smoke.Campaign(args, self.root, 1e100)
        campaign.cpus, campaign.locks = [8, 24], {'8': [1, 2], '24': [1, 3]}
        value = {'8': 42, '24': 42}
        events = []
        def invoke(*arguments, **keywords):
            events.append('command')
            return {'failure': 'bad exit', 'manager_handoff': False,
                    'leader_reaped': True, 'owned_session_gone': True}
        with patch.object(smoke, 'snapshot', side_effect=lambda cpus: events.append('snapshot') or value), \
             patch.object(smoke, 'lock_free', side_effect=lambda *args: events.append('locks') or campaign.locks), \
             patch.object(smoke, 'run_owned_replay', side_effect=invoke):
            with self.assertRaisesRegex(RuntimeError, 'restoration/lease evidence'):
                campaign.invoke('owned-error', ['never-spawned'], True)
        self.assertEqual(events, ['snapshot', 'locks', 'command', 'snapshot', 'locks'])
        self.assertTrue((self.root / 'owned-error/cleanup.json').exists())


class OwnershipTests(unittest.TestCase):
    def test_postspawn_receipt_failure_reaps_before_return(self):
        class Child:
            pid = 123
            returncode = None
            def wait(self, timeout):
                self.returncode = -15
                return self.returncode
        child = Child()
        leader = dict(pid=123, start_ticks=99, pgid=123, sid=123, state='S')
        stopped, receipts = [], []
        def signal_member(*args):
            stopped.append(True)
            leader['state'] = 'Z'
        def receipt_failure(child):
            raise OSError('injected postspawn receipt failure')
        with patch.object(owned.subprocess, 'Popen', return_value=child), \
             patch.object(owned, 'process_identity', side_effect=lambda pid: leader.copy()), \
             patch.object(owned, 'owned_session', side_effect=lambda sid: {'members': [leader.copy()], 'errors': []}), \
             patch.object(owned, 'exited_unreaped', side_effect=lambda child: bool(stopped)), \
             patch.object(owned, 'signal_owned_member', side_effect=signal_member):
            result = owned.run_owned_replay(['not executed'], {}, None, None, 1e100, 1e100,
                receipt_failure, lambda: None, receipts.append, cwd='/')
        # Assert directly at the ownership boundary; there is no fallback cleanup.
        self.assertTrue(stopped)
        self.assertEqual(child.returncode, -15)
        self.assertTrue(result['leader_reaped'] and result['owned_session_gone'])
        self.assertIn('postspawn receipt failure', result['failure'])
        self.assertFalse(result['manager_handoff'])
        self.assertEqual(len(receipts), 1)


if __name__ == '__main__':
    unittest.main()
