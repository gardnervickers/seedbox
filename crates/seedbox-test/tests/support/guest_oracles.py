"""Language-only fixture boundary for Rust-owned guest oracle checks.

The actual guest programs remain Python. Rust supplies inputs and checks all
observations; this adapter contains no test cases or expectations.
"""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import types

ROOT = Path(__file__).resolve().parents[4]
sys.path.insert(0, str(ROOT / 'python'))


def load(name, relative):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


request = json.load(sys.stdin)
output = io.StringIO()
observed = {'calls': []}
try:
    with contextlib.redirect_stdout(output):
        if request['program'] == 'race':
            race = load('race', 'harness/acceptance/driver/race.py')
            responses = iter(request['responses'])

            def run(argv, **kwargs):
                observed['calls'].append({'argv': argv, 'kwargs': kwargs})
                response = next(responses)
                if response['returncode'] and kwargs.get('check'):
                    raise subprocess.CalledProcessError(response['returncode'], argv)
                return subprocess.CompletedProcess(argv, response['returncode'], response['stdout'])

            race.subprocess = types.SimpleNamespace(run=run)
            observed['value'] = race.main()
        elif request['program'] == 'volume':
            kv = load('kv', 'examples/faults/workload/main.py')
            sys.modules['main'] = kv
            volume = load('volume', 'harness/acceptance/driver/volume.py')
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / 'kv.log'
                store = kv.Store(path, request['broken'])

                class World:
                    def __enter__(self):
                        return self

                    def __exit__(self, *args):
                        return False

                    def crash(self, target):
                        observed['calls'].append(['crash', target])
                        if store.log:
                            store.log.close()

                    def sleep(self, seconds):
                        observed['calls'].append(['sleep', seconds])

                    def reboot(self, target):
                        global store
                        observed['calls'].append(['reboot', target])
                        store = kv.Store(path, request['broken'])

                def exchange(endpoint, message, **kwargs):
                    return store.reply(message)

                volume.exchange = exchange
                volume.World = types.SimpleNamespace(connect=World)
                try:
                    observed['value'] = volume.main()
                finally:
                    if store.log:
                        store.log.close()
        else:
            raise ValueError('unknown guest program')
except Exception as error:
    observed['error'] = {'type': type(error).__name__, 'message': str(error)}
observed['stdout'] = output.getvalue()
print(json.dumps(observed))
