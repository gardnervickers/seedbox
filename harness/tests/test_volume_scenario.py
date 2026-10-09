"""The recovery oracle must reject acknowledged writes missing after reboot."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'python'))


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


kv = load('kv_fixture', 'examples/faults/workload/main.py')
with patch.dict(sys.modules, {'main': kv}):
    volume = load('volume_fixture', 'harness/acceptance/driver/volume.py')


class RecoveryTests(unittest.TestCase):
    def exercise(self, broken):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'kv.log'
            store = kv.Store(path, broken)
            events = []

            class World:
                def __enter__(self): return self
                def __exit__(self, *args): return False
                def crash(self, target):
                    events.append(('crash', target))
                    if store.log: store.log.close()
                def sleep(self, seconds): pass
                def reboot(self, target):
                    nonlocal store
                    events.append(('reboot', target))
                    store = kv.Store(path, broken)

            def exchange(endpoint, request, **kwargs):
                return store.reply(request)

            try:
                with patch.object(volume, 'exchange', exchange), patch.object(volume, 'World',
                        types.SimpleNamespace(connect=World)), patch('builtins.print'):
                    if broken:
                        with self.assertRaisesRegex(RuntimeError, 'lost or changed'):
                            volume.main()
                    else:
                        self.assertEqual(volume.main(), 0)
                self.assertEqual(events, [('crash', 'server'), ('reboot', 'server')])
            finally:
                if store.log: store.log.close()

    def test_recovers_persisted_acknowledgements(self):
        self.exercise(False)

    def test_rejects_acknowledgements_kept_only_in_memory(self):
        self.exercise(True)
