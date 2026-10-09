import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from run import HERE
from service import read_witness
from service_audit import verify_batch, verify_inventory, verify_profiles
from service_process import execute, save


class ReviewRegressions(unittest.TestCase):
    def test_terminal_decoder_uses_opcode_not_suffix(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'decode.c'
            source.write_text('''#include "witness-decode.h"
#include <assert.h>
#define CHECK(expected, ...) do { unsigned char b[]={__VA_ARGS__}; assert(terminal_out(b,sizeof b)==expected); } while(0)
int main(void) {
 CHECK(0, 0x3c,0xee); CHECK(0, 0xe6,0xee); CHECK(0, 0xe7,0xef);
 CHECK(0, 0xcd,0xee); CHECK(0, 0xb8,0,0,0xe7,0xf4);
 CHECK(1, 0xee); CHECK(1, 0x66,0xef); CHECK(1, 0xf3,0xee);
 CHECK(2, 0xe6,0xf4); CHECK(2, 0x66,0xe7,0xf4); CHECK(2, 0x48,0xe6,0xf4);
 CHECK(0, 0x66); CHECK(0, 0xe6); CHECK(0, 0xee,0);
 return 0;
}
''')
            subprocess.run(['clang', '-Wall', '-Wextra', '-Werror', '-I', str(HERE), str(source), '-o', str(root / 'decode')], check=True)
            subprocess.run([str(root / 'decode')], check=True)

    def test_atomic_save_preserves_previous_document_on_failed_write(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'budget.json'
            save(path, {'old': 1})
            def fail(value, stream, **kwargs):
                stream.write('{partial')
                raise OSError('simulated disk failure')
            with patch('run.json.dump', side_effect=fail), self.assertRaises(OSError):
                save(path, {'new': 2})
            self.assertEqual(json.loads(path.read_text()), {'old': 1})

    def test_signals_kill_child_and_account_interrupted_attempt(self):
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            with self.subTest(signal=sig), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                script = '''import json,sys
from pathlib import Path
from service_process import execute
root=Path(sys.argv[1])
budget={"limit_seconds":600,"spent_seconds":0,"runs":[]}
child="import os,time; from pathlib import Path; Path("+repr(str(root/'child.pid'))+").write_text(str(os.getpid())); time.sleep(60)"
execute([sys.executable,'-c',child],root/'s1-r0',root/'budget.json',budget,1,30)
'''
                env = dict(os.environ, PYTHONPATH=str(HERE))
                worker = subprocess.Popen([sys.executable, '-c', script, str(root)], env=env, start_new_session=True)
                try:
                    deadline = time.monotonic() + 5
                    while not (root / 'child.pid').exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    self.assertTrue((root / 'child.pid').exists())
                    child_pid = int((root / 'child.pid').read_text())
                    worker.send_signal(sig)
                    self.assertEqual(worker.wait(timeout=5), 0)
                    with self.assertRaises(ProcessLookupError):
                        os.kill(child_pid, 0)
                    budget = json.loads((root / 'budget.json').read_text())
                    self.assertNotIn('pending', budget)
                    self.assertGreater(budget['spent_seconds'], 0)
                    self.assertEqual(budget['runs'][0]['process_status'], 'INTERRUPTED')
                    self.assertEqual(budget['runs'][0]['signal'], sig)
                    self.assertEqual(json.loads((root / 's1-r0.attempt.json').read_text()), budget['runs'][0])
                finally:
                    if worker.poll() is None:
                        worker.kill()
                        worker.wait()

    def test_timeout_and_spawn_failure_are_accounted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            budget = {'limit_seconds': 600, 'spent_seconds': 0, 'runs': []}
            row = execute([sys.executable, '-c', 'import time; time.sleep(60)'], root/'s1-r0', root/'budget.json', budget, 1, 0.05)
            self.assertEqual(row['process_status'], 'TIMEOUT')
            with self.assertRaises(FileNotFoundError):
                execute([str(root/'does-not-exist')], root/'s1-r1', root/'budget.json', budget, 1, 1)
            self.assertEqual(len(budget['runs']), 2)
            self.assertEqual(budget['runs'][1]['process_status'], 'ERROR')
            budget['pending'] = {'path': 'hard-crash'}
            with self.assertRaisesRegex(ValueError, 'pending'):
                execute([], root/'s1-r2', root/'budget.json', budget, 1, 1)

    def make_batch(self, root):
        source = HERE/'testdata/service-qualified-fault'
        batch = root/'service-qualified-fault'
        batch.mkdir()
        for suffix in ('console', 'mgmt', 'witness.json', 'manifest.json'):
            (batch/f's1009-r0.{suffix}').write_bytes((source/f's1009-r0.{suffix}').read_bytes())
        save(batch/'runs.json', json.loads((source/'runs.json').read_text())[:1])
        return batch

    def test_audit_rejects_tampered_raw_artifacts_and_null_witness(self):
        for kind in ('console', 'mgmt', 'witness', 'null', 'orphan', 'classification', 'manifest'):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                batch = self.make_batch(root)
                verify_batch(batch, mode='fault', witness_required=True)
                if kind in ('console', 'mgmt'):
                    path = batch/f's1009-r0.{kind}'
                    path.write_bytes(path.read_bytes()+b'TAMPERED\n')
                elif kind == 'witness':
                    path = batch/'s1009-r0.witness.json'
                    value = json.loads(path.read_text()); value['sha256'] = '0'*64; save(path, value)
                elif kind == 'orphan':
                    (batch/'s1009-r9.console').write_text('orphan')
                elif kind == 'manifest':
                    path = batch/'s1009-r0.manifest.json'
                    value = json.loads(path.read_text()); value['seed'] = 1010; save(path, value)
                else:
                    path = batch/'runs.json'; value = json.loads(path.read_text())
                    if kind == 'null':
                        value[0]['witness'] = None
                        (batch/'s1009-r0.witness.json').unlink()
                    else:
                        value[0]['reason'] = 'pass fabricated'
                    save(path, value)
                with self.assertRaises(ValueError):
                    verify_batch(batch, mode='fault', witness_required=True)

    def test_inventory_rejects_orphan_attempt_without_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); batch = root/'service-interrupted'; batch.mkdir()
            (batch/'s1-r0.console').write_text('partial')
            with self.assertRaisesRegex(ValueError, 'unreported attempt'):
                verify_inventory(root, {'limit_seconds':600, 'spent_seconds':0, 'runs':[]})

    def test_rejected_witness_is_retained_and_auditable(self):
        for raw in (b'{malformed', b'{"schema":"seedbox.tcg.boundaries.v2","terminal":false}'):
            with self.subTest(raw=raw), tempfile.TemporaryDirectory() as directory:
                root = Path(directory); batch = self.make_batch(root)
                path = batch/'s1009-r0.witness.json'; path.write_bytes(raw)
                witness, diagnostics, rejected = read_witness(path, 0)
                self.assertIsNone(witness)
                self.assertIsNotNone(rejected)
                rows = json.loads((batch/'runs.json').read_text())
                rows[0].update(status='ERROR', reason='missing or malformed requested witness', witness=witness,
                               witness_diagnostics=diagnostics, witness_rejected_sha256=rejected)
                save(batch/'runs.json', rows)
                budget = {'limit_seconds':600, 'spent_seconds':rows[0]['wall_seconds'],
                          'runs':[{'path':str(batch/'s1009-r0'), **rows[0]}]}
                verify_inventory(root, budget)
                path.write_bytes(raw+b'TAMPERED')
                with self.assertRaises(ValueError):
                    verify_inventory(root, budget)

    def test_compared_batches_require_matching_runtime_and_plugin(self):
        for key in ('witness_plugin_sha256', 'qemu_sha256', 'kernel_sha256', 'shift'):
            with self.subTest(key=key), tempfile.TemporaryDirectory() as directory:
                batch = self.make_batch(Path(directory))
                path = batch/'s1009-r0.manifest.json'; reference = json.loads(path.read_text())
                verify_profiles([batch], reference)
                changed = dict(reference); changed[key] = 'mismatched'; save(path, changed)
                with self.assertRaisesRegex(ValueError, 'profile mismatch'):
                    verify_profiles([batch], reference)


if __name__ == '__main__':
    unittest.main()
