"""Focused evidence retention tests; no runtime fixture or privileged execution."""
import contextlib
import hashlib
import io
import json
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import zlib

import artifacts


def elf(companion, crc):
    data = bytearray(512)
    data[:6] = b'\x7fELF\x02\x01'
    struct.pack_into('<Q', data, 40, 64)
    struct.pack_into('<HHH', data, 58, 64, 2, 0)
    names = b'\0.shstrtab\0.gnu_debuglink\0'
    data[256:256 + len(names)] = names
    link = companion.encode() + b'\0'
    link += b'\0' * (-len(link) % 4) + struct.pack('<I', crc)
    data[320:320 + len(link)] = link
    struct.pack_into('<IIQQQQIIQQ', data, 64, 1, 3, 0, 0, 256, len(names), 0, 0, 1, 0)
    struct.pack_into('<IIQQQQIIQQ', data, 128, 11, 1, 0, 0, 320, len(link), 0, 0, 1, 0)
    return bytes(data)


class ArtifactTests(unittest.TestCase):
    def collect(self, root, environment):
        with patch.object(artifacts.subprocess, 'check_output', return_value='exact-head\n'), \
                contextlib.redirect_stderr(io.StringIO()):
            artifacts.collect(root, root / 'temp', environment)
        return json.loads((root / 'temp/dr-artifacts/manifest.json').read_text())

    def test_exact_bytes_crc_identity_reports_caps_and_no_symlink_or_core(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = root / 'fixture/runtime/lib64/release'
            runtime.mkdir(parents=True)
            symbols = b'companion symbols'
            binary = elf('libdynamorio.so.debug', zlib.crc32(symbols))
            (runtime / 'libdynamorio.so').write_bytes(binary)
            (runtime / 'libdynamorio.so.debug').write_bytes(symbols)
            reports = root / 'target/check-reports/failure'
            reports.mkdir(parents=True)
            (reports / 'error.txt').write_bytes(b'x' * 100)
            (reports / 'core').write_bytes(b'core omitted')
            (reports / 'secret.txt').symlink_to(runtime / 'libdynamorio.so')
            with patch.object(artifacts, 'TEXT_FILE', 8), patch.object(artifacts, 'TEXT_TOTAL', 16):
                manifest = self.collect(root, {'SEEDBOX_DYNAMORIO_FIXTURES': str(root / 'fixture')})
            output = root / 'temp/dr-artifacts'
            self.assertEqual((output / 'runtime/release/libdynamorio.so').read_bytes(), binary)
            self.assertEqual((output / 'runtime/release/libdynamorio.so.debug').read_bytes(), symbols)
            self.assertEqual(manifest['files'][0]['sha256'], hashlib.sha256(binary).hexdigest())
            self.assertTrue(manifest['files'][1]['companion_crc_verified'])
            self.assertEqual((output / 'check-reports/failure/error.txt.truncated').read_bytes(), b'x' * 8)
            self.assertFalse((output / 'check-reports/failure/core').exists())
            self.assertFalse((output / 'check-reports/failure/secret.txt').exists())
            self.assertEqual(manifest['seedbox_head'], 'exact-head')

    def test_missing_and_oversized_sources_warn_without_placeholder_binaries(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = root / 'fixture/runtime/lib64/debug'
            runtime.mkdir(parents=True)
            (runtime / 'libdynamorio.so').write_bytes(b'too big')
            with patch.object(artifacts, 'BINARY_FILE', 1):
                manifest = self.collect(root, {'SEEDBOX_DYNAMORIO_DEBUG_FIXTURES': str(root / 'fixture')})
            self.assertEqual(manifest['files'], [])
            self.assertTrue(any('oversized' in warning for warning in manifest['warnings']))
            self.assertTrue(any('missing' in warning for warning in manifest['warnings']))

    def test_mismatching_symbols_are_not_published_as_matching(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = root / 'fixture/runtime/lib64/release'
            runtime.mkdir(parents=True)
            (runtime / 'libdynamorio.so').write_bytes(elf('libdynamorio.so.debug', 42))
            (runtime / 'libdynamorio.so.debug').write_bytes(b'wrong symbols')
            manifest = self.collect(root, {'SEEDBOX_DYNAMORIO_FIXTURES': str(root / 'fixture')})
            self.assertEqual(len(manifest['files']), 2)
            self.assertFalse(manifest['files'][1]['companion_crc_verified'])
            self.assertTrue(any('CRC mismatch' in warning for warning in manifest['warnings']))

    def test_arm_progress_probe_both_profiles_keep_exact_elves_and_loader_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            libc = root / 'glibc/lib/libc.so.6'
            loader = root / 'glibc/lib/ld-linux-aarch64.so.1'
            libc.parent.mkdir(parents=True)
            libc.write_bytes(b'libc')
            loader.write_bytes(b'loader')
            environment = {'RUNNER_ARCH': 'ARM64'}
            for flavor in ('release', 'debug'):
                fixture = root / flavor
                (fixture / 'bin').mkdir(parents=True)
                (fixture / 'lib').mkdir()
                (fixture / 'bin/translator_progress_context').write_bytes(b'probe-' + flavor.encode())
                (fixture / 'lib/libtranslator_progress.so').write_bytes(b'client-' + flavor.encode())
                variable = ('SEEDBOX_DYNAMORIO_FIXTURES' if flavor == 'release'
                            else 'SEEDBOX_DYNAMORIO_DEBUG_FIXTURES')
                environment[variable] = str(fixture)
            headers = (f'      [Requesting program interpreter: {loader}]\n'
                       ' 0x1 (NEEDED) Shared library: [libc.so.6]\n'
                       ' 0x1 (NEEDED) Shared library: [ld-linux-aarch64.so.1]\n'
                       f' 0x1d (RUNPATH) Library runpath: [{libc.parent}]\n')
            def readelf_output(command, **kwargs):
                output = headers if command[-1].endswith('translator_progress_context') else (
                    ' 0x1 (NEEDED) Shared library: [libdynamorio.so]\n')
                return subprocess.CompletedProcess(command, 0, output, '')

            with patch.object(artifacts.subprocess, 'run', side_effect=readelf_output) as readelf:
                manifest = self.collect(root, environment)
            self.assertEqual(readelf.call_count, 4)
            self.assertEqual(len(manifest['files']), 5)
            interpreter = next(record for record in manifest['files']
                               if record['retained'] ==
                               'fixtures/interpreter/ld-linux-aarch64.so.1')
            self.assertEqual((root / 'temp/dr-artifacts' / interpreter['retained']).read_bytes(),
                             loader.read_bytes())
            self.assertEqual(interpreter['sha256'], hashlib.sha256(loader.read_bytes()).hexdigest())
            for record in (record for record in manifest['files'] if 'elf' in record):
                retained = root / 'temp/dr-artifacts' / record['retained']
                self.assertEqual(retained.read_bytes(), Path(record['source']).read_bytes())
                self.assertEqual(record['sha256'], hashlib.sha256(retained.read_bytes()).hexdigest())
                if retained.name == 'translator_progress_context':
                    self.assertEqual(record['elf']['interpreter'], str(loader))
                    self.assertTrue(record['elf']['interpreter_exists'])
                    self.assertEqual(record['elf']['needed_in_embedded_paths']['libc.so.6'], str(libc))
                else:
                    self.assertIsNone(record['elf']['interpreter'])

    def test_arm_progress_probe_missing_binary_is_reported_without_placeholder(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / 'fixture'
            fixture.mkdir()
            manifest = self.collect(root, {'RUNNER_ARCH': 'ARM64',
                                           'SEEDBOX_DYNAMORIO_FIXTURES': str(fixture)})
            self.assertFalse((root / 'temp/dr-artifacts/fixtures/release').exists())
            self.assertTrue(any('translator_progress_context' in warning
                                for warning in manifest['warnings']))

    def test_log_drains_all_bytes_and_retains_bounded_head_tail(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'ordinary.log'
            payload = b'A' * 8 + b'M' * 200000 + b'Z' * 8
            code = 'import sys; import artifacts; artifacts.LOG_HALF=8; artifacts.capture_log(artifacts.Path(sys.argv[1]))'
            result = subprocess.run([sys.executable, '-c', code, str(path)], input=payload,
                                    cwd=Path(artifacts.__file__).parent, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, payload)
            retained = path.read_bytes()
            self.assertTrue(retained.startswith(b'A' * 8))
            self.assertTrue(retained.endswith(b'Z' * 8))
            self.assertIn(b'truncated', retained)
            self.assertLess(len(retained), 128)

    def test_workflow_pipefail_preserves_failed_command_status(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'ordinary.log'
            script = '''"$1" -c 'import sys; print("test failed"); sys.exit(7)' | "$1" "$2" --log "$3"'''
            result = subprocess.run(['bash', '-o', 'pipefail', '-c', script, 'test',
                                     sys.executable, artifacts.__file__, str(path)],
                                    capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 7, result.stderr)
            self.assertEqual(result.stdout, b'test failed\n')
            self.assertEqual(path.read_bytes(), result.stdout)


if __name__ == '__main__':
    unittest.main()
