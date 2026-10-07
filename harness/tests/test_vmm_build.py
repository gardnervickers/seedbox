"""Build-path isolation and reproducibility-runner lifecycle; no KVM required."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


HARNESS = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('vmm_rustc', HARNESS / 'vmm-rustc.py')
wrapper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wrapper)


class WrapperTests(unittest.TestCase):
    def test_only_synthetic_rpath_is_removed(self):
        flags = ('-rpath /old/checkout/outputs/out/lib -L/nix/store/libs/lib '
                 '-rpath /nix/store/real/outputs/out/lib -rpath /opt/runtime/lib -z now')
        env = {'NIX_LDFLAGS': flags, 'NIX_LDFLAGS_x86_64_unknown_linux_gnu': flags,
               'RUSTFLAGS': '-C target-cpu=x86-64', 'OTHER': flags}
        expected = flags.removeprefix('-rpath /old/checkout/outputs/out/lib ')
        result = wrapper.linker_env(env)
        self.assertEqual(result['NIX_LDFLAGS'], expected)
        self.assertEqual(result['NIX_LDFLAGS_x86_64_unknown_linux_gnu'], expected)
        self.assertEqual(result['OTHER'], flags)
        self.assertEqual(result['RUSTFLAGS'], env['RUSTFLAGS'])
        self.assertEqual(env['NIX_LDFLAGS'], flags)

    def test_synthetic_output_with_spaces_is_removed(self):
        output = '/checkout with spaces/outputs/out'
        env = {'out': output, 'NIX_LDFLAGS': f'-rpath {output}/lib -rpath /nix/store/libs/lib'}
        self.assertEqual(wrapper.linker_env(env)['NIX_LDFLAGS'], '-rpath /nix/store/libs/lib')

    def test_wrapper_preserves_arguments_and_maps_nested_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            home = root / 'home'
            checkout = home / 'checkout with spaces'
            (checkout / 'harness').mkdir(parents=True)
            shutil.copy2(HARNESS / 'vmm-rustc.py', checkout / 'harness/vmm-rustc.py')
            compiler = root / 'compiler'
            compiler.write_text('''#!/usr/bin/env python3
import json, os, sys
if sys.argv[1:] == ['--print', 'sysroot']:
    print(os.environ['TEST_SYSROOT'])
else:
    print(json.dumps({'args': sys.argv[1:], 'ldflags': os.environ['NIX_LDFLAGS']}))
''')
            compiler.chmod(0o755)
            cargo_home = home / 'custom cargo'
            sysroot = home / 'toolchain'
            args = ['--crate-name', 'fixture', '-C', 'debuginfo=1', '-C', 'target-cpu=x86-64']
            env = dict(os.environ, HOME=str(home), CARGO_HOME=str(cargo_home),
                       PWD=str(checkout), TEST_SYSROOT=str(sysroot),
                       NIX_LDFLAGS='-rpath /inherited/outputs/out/lib -rpath /nix/store/libs/lib')
            result = subprocess.run(
                [str(checkout / 'harness/vmm-rustc.py'), str(compiler), *args],
                env=env, capture_output=True, text=True, check=True)
            recorded = json.loads(result.stdout)
            self.assertEqual(recorded['args'][:len(args)], args)
            self.assertIn(f'--remap-path-prefix={home}=/seedbox-build/home', recorded['args'])
            self.assertIn(f'--remap-path-prefix={cargo_home}=/seedbox-build/cargo', recorded['args'])
            self.assertIn(f'--remap-path-prefix={sysroot}=/seedbox-build/rust', recorded['args'])
            self.assertEqual(recorded['args'][-1], f'--remap-path-prefix={checkout}=.')
            self.assertEqual(recorded['ldflags'], '-rpath /nix/store/libs/lib')


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.repo = self.root / 'repo'
        (self.repo / 'harness').mkdir(parents=True)
        self.runner = self.repo / 'harness/check-vmm-reproducible.sh'
        shutil.copy2(HARNESS / 'check-vmm-reproducible.sh', self.runner)
        for args in (
            ['init', '-q'], ['add', 'harness'],
            ['-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
             '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture'],
        ):
            subprocess.run(['git', *args], cwd=self.repo, check=True, capture_output=True)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.scratch = self.root / 'scratch'
        self.scratch.mkdir()
        for name, script in {
            'uname': '#!/bin/sh\necho Linux\n',
            'cargo': r'''#!/usr/bin/env python3
import os
from pathlib import Path
assert os.sys.argv[1:] == ['build-vmm']
assert Path('harness/check-vmm-reproducible.sh').is_file()
if os.environ['TEST_MODE'] == 'build-failure':
    raise SystemExit(7)
binary = Path('crates/seedbox-vmm/target/release/seedbox-vmm')
binary.parent.mkdir(parents=True)
data = '/nix/store/runtime/lib\n'
if os.environ['TEST_MODE'] == 'mismatch':
    data += str(Path.cwd()) + '/src/main.rs\n'
elif os.environ['TEST_MODE'] == 'shared-leak':
    data += os.environ['TEST_SOURCE_ROOT'] + '/outputs/out/lib\n'
binary.write_text(data)
''',
        }.items():
            path = self.bin / name
            path.write_text(script)
            path.chmod(0o755)

    def run_mode(self, mode):
        result = subprocess.run(
            ['bash', str(self.runner)], cwd=self.repo, capture_output=True, text=True,
            env=dict(os.environ, PATH=f'{self.bin}{os.pathsep}{os.environ["PATH"]}',
                     TMPDIR=str(self.scratch), TEST_MODE=mode, TEST_SOURCE_ROOT=str(self.repo)))
        self.assertEqual(list(self.scratch.iterdir()), [], result.stderr)
        return result

    def test_equal_store_paths_pass_and_clean_up(self):
        result = self.run_mode('match')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('reproducible: byte-identical', result.stdout)

    def test_mismatch_prints_paths_and_cleans_up(self):
        result = self.run_mode('mismatch')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('differing embedded paths follow', result.stderr)
        self.assertIn('/first/src/main.rs', result.stderr)
        self.assertIn('/second/src/main.rs', result.stderr)

    def test_shared_checkout_rpath_is_rejected_even_with_equal_hashes(self):
        result = self.run_mode('shared-leak')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('retains an absolute checkout path', result.stderr)

    def test_build_failure_cleans_up(self):
        result = self.run_mode('build-failure')
        self.assertEqual(result.returncode, 7, result.stderr)


class CargoTests(unittest.TestCase):
    def test_alias_remaps_a_portable_debug_fixture_without_affecting_plain_builds(self):
        # Compile a tiny portable library, not the Linux-only VMM. This exercises
        # real Cargo alias/config resolution and rustc's DWARF/file! remapping.
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            binaries = []
            for name in ('first', 'a longer checkout with spaces'):
                root = base / name
                (root / '.cargo').mkdir(parents=True)
                (root / 'harness').mkdir()
                crate = root / 'crates/seedbox-vmm'
                (crate / 'src').mkdir(parents=True)
                shutil.copy2(HARNESS / 'vmm-rustc.py', root / 'harness/vmm-rustc.py')
                shutil.copy2(HARNESS.parent / '.cargo/config.toml', root / '.cargo/config.toml')
                (crate / 'Cargo.toml').write_text('''[package]
name = "seedbox-vmm"
version = "0.1.0"
edition = "2021"
[workspace]
[profile.release]
debug = 1
''')
                (crate / 'Cargo.lock').write_text('''version = 4
[[package]]
name = "seedbox-vmm"
version = "0.1.0"
''')
                (crate / 'src/lib.rs').write_text("pub fn source() -> &'static str { file!() }\n")
                env = dict(os.environ, CARGO_TARGET_DIR=str(base / 'unrelated-target'))
                # A preexisting ordinary release build must not be reused by
                # the alias after enabling path remapping.
                warm = subprocess.run(
                    ['cargo', 'build', '--release', '--offline', '--locked', '--manifest-path',
                     str(crate / 'Cargo.toml'), '--target-dir', str(crate / 'target')],
                    cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(warm.returncode, 0, warm.stderr)
                result = subprocess.run(['cargo', 'build-vmm', '--offline', '-v'],
                                        cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('vmm-rustc.py', result.stderr)
                self.assertFalse((base / 'unrelated-target/release').exists())
                binaries.append((crate / 'target/release/libseedbox_vmm.rlib').read_bytes())
                self.assertNotIn(str(root).encode(), binaries[-1])
                # A plain build in the same config scope must omit the wrapper.
                plain = subprocess.run(
                    ['cargo', 'build', '--offline', '--locked', '-v', '--manifest-path',
                     str(crate / 'Cargo.toml')], cwd=root, env=env,
                    capture_output=True, text=True)
                self.assertEqual(plain.returncode, 0, plain.stderr)
                self.assertNotIn('vmm-rustc.py', plain.stderr)
            self.assertEqual(binaries[0], binaries[1])
            # Editing the wrapper must invalidate the compiler fingerprint,
            # including artifacts already built through the alias.
            script = root / 'harness/vmm-rustc.py'
            with script.open('a') as stream:
                stream.write('\n# changed wrapper fingerprint\n')
            rebuilt = subprocess.run(['cargo', 'build-vmm', '--offline', '-v'],
                                     cwd=root, env=env, capture_output=True, text=True)
            self.assertEqual(rebuilt.returncode, 0, rebuilt.stderr)
            self.assertIn('vmm-rustc.py', rebuilt.stderr)


if __name__ == '__main__':
    unittest.main()
