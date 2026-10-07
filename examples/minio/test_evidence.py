"""Negative controls for the example verifier, using actual client receipts."""
from contextlib import redirect_stdout
import copy
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import cas
from test_client import Store, factory
from verify import check_console, verify, EvidenceError
import stage_inputs
from generate import generate


def console(mode):
    store = Store()
    make = factory(store)
    output = io.StringIO()
    with redirect_stdout(output), tempfile.TemporaryDirectory() as directory:
        # Setup's fake transport intentionally covers object requests only.
        cas.emit('MINIO_HTTP', operation='list_buckets', key=None, condition={}, status=200, etag=None)
        cas.emit('MINIO_HTTP', operation='create_bucket', key=None, condition={}, status=200, etag=None)
        if mode == 'qualification':
            cas.qualify(make(), make)
        else:
            cas.planted(make(), make, mode, Path(directory) / 'ledger')
    return output.getvalue()

def alter(text, kind, index, **changes):
    lines = text.splitlines()
    matches = [i for i, line in enumerate(lines) if line.startswith(kind + ' ')]
    i = matches[index]
    receipt = json.loads(lines[i][len(kind) + 1:])
    receipt.update(changes)
    lines[i] = kind + ' ' + json.dumps(receipt)
    return '\n'.join(lines)

class EvidenceControls(unittest.TestCase):
    def test_finite_transient_readiness_receipts(self):
        text = console('fixed')
        transient = alter(text.splitlines()[0], 'MINIO_HTTP', 0, status=503)
        check_console(transient + '\n' + text, 'fixed')
        with self.assertRaises(EvidenceError):
            check_console((transient + '\n') * 8 + text, 'fixed')
        with self.assertRaises(EvidenceError):
            check_console(alter(text, 'MINIO_HTTP', 0, status=403), 'fixed')

    def test_real_client_receipts_and_missing_duplicate_or_wrong_verdict(self):
        for mode in ('qualification', 'broken', 'fixed'):
            text = console(mode)
            check_console(text, mode)
            for bad in (text[:text.index('MINIO_RESULT')], text + text,
                        alter(text, 'MINIO_RESULT', 0, passed=(mode == 'broken')),
                        'qualification/setup error: timeout\n' + text):
                with self.subTest(mode=mode), self.assertRaises(EvidenceError):
                    check_console(bad, mode)

    def test_cas_matrix_negatives(self):
        text = console('qualification')
        mutations = [('MINIO_HTTP', 2, {'condition': {}}),
                     ('MINIO_HTTP', 4, {'status': 200}),
                     ('MINIO_HTTP', 6, {'condition': {'IfMatch': 'invented'}}),
                     ('MINIO_HTTP', 10, {'status': 412}),
                     ('MINIO_HTTP', 11, {'status': 200}),
                     ('MINIO_GET', 1, {'value_hex': cas.B.hex()}),
                     ('MINIO_GET', 2, {'etag': 'invented'})]
        for kind, index, changes in mutations:
            with self.subTest(changes=changes), self.assertRaises(EvidenceError):
                check_console(alter(text, kind, index, **changes), 'qualification')
        # An acknowledged loser with the winner's ETag still fails byte checking.
        winner = json.loads([line for line in text.splitlines() if line.startswith('MINIO_GET ')][-1].split(' ', 1)[1])
        loser = cas.B.hex() if winner['value_hex'] == cas.A.hex() else cas.A.hex()
        with self.assertRaises(EvidenceError):
            check_console(alter(text, 'MINIO_GET', 4, value_hex=loser), 'qualification')

    def test_planted_negatives(self):
        text = console('broken')
        for bad in (alter(text, 'MINIO_HTTP', 4, condition={'IfNoneMatch': '*'}),
                    alter(text, 'MINIO_GET', 0, value_hex=cas.A.hex()),
                    alter(text, 'MINIO_RESULT', 0, acknowledged_hex=cas.B.hex()),
                    alter(text, 'MINIO_HTTP', 2, status=412)):
            with self.assertRaises(EvidenceError):
                check_console(bad, 'broken')

    def test_retained_outcome_exit_memory_faults_and_replay_negatives(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'output/machines').mkdir(parents=True)
            (root / 'output/machines/client.console').write_text(console('broken'))
            base = {'outcome': 'FAIL', 'error': None, 'omitted_logs': {},
                    'report': {'reason': 'COMPLETED', 'witness': 'synthetic-witness',
                               'workload_exit': {'service': 'client', 'code': 1}},
                    'identity': {'inputs': {'profile': {'semantic_parameters': {'memory_mb': '2048'}},
                                           'placement': {'client': 'client', 'minio': 'minio'}}},
                    'replay_of': 'original', 'replay': {'status': 'MATCH', 'outcome_matches': True,
                    'witness_matches': True, 'fault_history_matches': True, 'full_state_matches': None}}
            world = {'machines': [{'name': 'minio'}, {'name': 'client'}], 'faults': []}
            def write(record, w=world):
                (root / 'manifest.json').write_text(json.dumps({'record': record}))
                (root / 'output/vmm.json').write_text(json.dumps(w))
            write(base)
            verify(root, 'broken', replay=True)
            mutations = [lambda r: r.update(outcome='ERROR'),
                         lambda r: r['report']['workload_exit'].update(code=2),
                         lambda r: r['report'].update(reason='WALL_TIME_LIMIT'),
                         lambda r: r['identity']['inputs']['profile']['semantic_parameters'].update(memory_mb='512'),
                         lambda r: r['identity']['inputs']['placement'].update(client='minio'),
                         lambda r: r.update(omitted_logs={'client.console': 'cap'}),
                         lambda r: r.update(replay=None),
                         lambda r: r['replay'].update(status='MISMATCH'),
                         lambda r: r['replay'].update(witness_matches=False),
                         lambda r: r['replay'].update(outcome_matches=False),
                         lambda r: r['replay'].update(fault_history_matches=False)]
            for mutation in mutations:
                record = copy.deepcopy(base)
                mutation(record)
                write(record)
                with self.assertRaises(EvidenceError):
                    verify(root, 'broken', replay=True)
            write(base, {**world, 'faults': [{'action': 'partition'}]})
            with self.assertRaises(EvidenceError):
                verify(root, 'broken', replay=True)

    def test_staging_and_generation_refuse_corruption_and_overwrite(self):
        import hashlib
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'binary'
            binary.write_bytes(b'tiny-test-input')
            pin = {'size': binary.stat().st_size, 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}
            wheels = root / 'wheels'
            wheels.mkdir()
            wheel = wheels / 'test.whl'
            wheel.write_bytes(binary.read_bytes())
            pins = {'minio': pin, 'wheels': [{**pin, 'filename': 'test.whl'}]}
            with patch.object(stage_inputs, 'PINS', pins):
                generated = generate(root / 'projects', binary, wheels)
                for mode in ('qualification', 'broken', 'fixed'):
                    project = generated / mode
                    self.assertEqual((project / 'minio/minio').read_bytes(), binary.read_bytes())
                    compose = json.loads((project / 'compose.yaml').read_text())
                    self.assertEqual(compose['services']['client']['command'][0], mode)
                    with self.assertRaises(FileExistsError):
                        stage_inputs.stage(project, binary, wheels)
                with self.assertRaises(FileExistsError):
                    generate(generated, binary, wheels)
                binary.write_bytes(b'evil-test-input')  # Same size, wrong hash.
                target = root / 'rejected'
                with self.assertRaisesRegex(ValueError, 'hash mismatch'):
                    stage_inputs.stage(target, binary, wheels)
                self.assertFalse(target.exists())

if __name__ == '__main__':
    unittest.main()
