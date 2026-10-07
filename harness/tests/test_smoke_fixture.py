"""Short smoke workload controls, with no OCI, guest or hardware access."""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, REPO / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


generator = load('faults_generator', 'examples/faults/generate.py')
portable = load('faults_portable', 'examples/faults/portable.py')
workload = portable.workload
with patch.dict(sys.modules, {'main': workload}), patch.object(sys, 'path', [str(REPO / 'python'), *sys.path]):
    scenario = load('faults_scenario', 'examples/faults/workload/scenario.py')


class FixtureTests(unittest.TestCase):
    def test_invalid_batch_refuses_before_workload_start(self):
        for mode in ('client', 'scenario'):
            for batch in ('0', '33', 'invalid'):
                result = subprocess.run([sys.executable, str(portable.WORKLOAD), mode,
                                         '--batch', batch], capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(result.stdout, '')
                self.assertIn('--batch', result.stderr)

    def test_generated_workload_sizes_and_default_are_explicit(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for batch in (4, 32):
                project = generator.generate(root / str(batch), **({'batch': batch} if batch != 32 else {}))
                for variant in ('fixed', 'broken', 'broken-no-faults'):
                    text = (project / variant / 'compose.yaml').read_text()
                    self.assertEqual(text.count('--batch, "4"'), 2 if batch == 4 else 0)
                    self.assertEqual('--no-faults' in text, variant == 'broken-no-faults')
                    self.assertEqual('--broken' in text, variant != 'fixed')
            for invalid in (0, -1, 33, True, 1.5):
                with self.assertRaises(ValueError):
                    generator.generate(root / 'invalid', batch=invalid)
                self.assertFalse((root / 'invalid').exists())

    def test_short_workload_real_tcp_recovery_and_negative_controls(self):
        # Actual client/server processes: fixed recovery, real acknowledged loss,
        # and the broken server without restarts. Only workload size is reduced.
        with tempfile.TemporaryDirectory() as temporary, contextlib.redirect_stdout(io.StringIO()):
            for broken, restart in ((False, True), (True, True), (True, False)):
                portable.check_case(Path(temporary) / f'{broken}-{restart}', broken, restart, batch=4)

    def test_short_scenario_preserves_faults_probes_and_every_ack_check(self):
        # Exercise the actual scenario, client ledger and disk-backed Store.
        # Fake only transport and World; faults must still cause the right work.
        for broken, effective_cut, no_faults in ((False, True, False), (True, True, False),
                                                (False, False, False), (True, True, True)):
            with self.subTest(broken=broken, effective_cut=effective_cut, no_faults=no_faults), \
                 tempfile.TemporaryDirectory() as temporary, contextlib.redirect_stdout(io.StringIO()):
                state = Path(temporary) / 'kv.log'
                store = workload.Store(state, broken)
                requests, commands, events, handles = [], [], [], []
                def close_store():
                    if store.log:
                        store.log.close()
                def stop(target):
                    self.assertEqual(target, 'server')
                    events.append('stop')
                    close_store()
                def restart(target):
                    nonlocal store
                    self.assertEqual(target, 'server')
                    events.append('restart')
                    store = workload.Store(state, broken)
                def partition(left, right):
                    self.assertEqual((left, right), (['client'], ['server']))
                    events.append('partition')
                    handle = str(len(events))
                    handles.append(handle)
                    return SimpleNamespace(handle=handle)
                def heal(cut):
                    events.append('heal')
                    handles.remove(cut.handle)
                world = SimpleNamespace(stop=stop, restart=restart, partition=partition, heal=heal,
                    inspect=lambda: {'active_handles': handles.copy()}, now_ns=lambda: 0,
                    sleep=lambda seconds: self.assertEqual(seconds, 0.1))
                def exchange(address, command, **kwargs):
                    requests.append(command)
                    if handles and effective_cut:
                        raise TimeoutError('partition dropped request')
                    return store.reply(command)
                with patch.object(workload, 'resolve', return_value=[]):
                    client = workload.Client('server:7000', batch=4)
                def control(address, command):
                    commands.append(command)
                    return json.loads(client.reply(command))
                try:
                    with patch.object(scenario.World, 'connect', return_value=contextlib.nullcontext(world)), \
                         patch.object(scenario, 'client', side_effect=control), \
                         patch.object(workload, 'exchange', side_effect=exchange):
                        if not effective_cut:
                            with self.assertRaises(AssertionError):
                                scenario.run('client:7001', batch=4)
                            continue
                        self.assertEqual(scenario.run('client:7001', no_faults, batch=4), int(broken and not no_faults))
                    expected_events = [] if no_faults else ['stop', 'restart', 'partition', 'partition', 'heal', 'heal', 'stop', 'restart']
                    self.assertEqual(events, expected_events)
                    self.assertEqual(commands.count('PROBE'), 0 if no_faults else 2)
                    self.assertEqual(sum(r.startswith('PUT ') for r in requests), 16 if no_faults else 24)
                    self.assertEqual([r for r in requests if r.startswith('GET ')],
                                     [f'GET k/{i:06}' for i in range(16)])
                    self.assertEqual(client.result['acked'], 16)
                    self.assertEqual(client.result['missing'], 12 if broken and not no_faults else 0)
                finally:
                    close_store()


if __name__ == '__main__':
    unittest.main()
