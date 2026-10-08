"""Build-path isolation and reproducibility-runner lifecycle; no KVM required."""
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
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
for fd in os.environ['TEST_JOBSERVER_FDS'].split(','):
    os.fstat(int(fd))
if sys.argv[1:] == ['--print', 'sysroot']:
    print(os.environ['TEST_SYSROOT'])
elif sys.argv[1:] == ['-vV']:
    print('rustc fixture')
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
            # Cargo marks its jobserver pipe inheritable. Both rustc probes
            # must retain it, as must the exec'd compile.
            read_fd, write_fd = os.pipe()
            try:
                env['TEST_JOBSERVER_FDS'] = f'{read_fd},{write_fd}'
                for probe_args in (['-vV'], args):
                    result = subprocess.run(
                        [str(checkout / 'harness/vmm-rustc.py'), str(compiler), *probe_args],
                        env=env, pass_fds=(read_fd, write_fd),
                        capture_output=True, text=True, check=True)
                    self.assertEqual(result.stderr, '')
                    if probe_args == ['-vV']:
                        self.assertIn('seedbox-vmm-wrapper:', result.stdout)
            finally:
                os.close(read_fd)
                os.close(write_fd)
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
            'rustc': '#!/bin/sh\nprintf "%s\\n" "$TEST_SYSROOT"\n',
            'cargo': r'''#!/usr/bin/env python3
import os
from pathlib import Path
assert os.sys.argv[1:] == ['build-vmm', '-v']
assert Path('harness/check-vmm-reproducible.sh').is_file()
if os.environ['TEST_MODE'] == 'build-failure':
    raise SystemExit(7)
binary = Path('crates/seedbox-vmm/target/release/seedbox-vmm')
binary.parent.mkdir(parents=True)
data = '/nix/store/runtime/lib\n'
if os.environ['TEST_MODE'] == 'mismatch':
    data += str(Path.cwd()) + '/src/main.rs\n'
    metadata = 'aaa' if Path.cwd().name == 'first' else 'bbb'
    print(f'Running `rustc --crate-name seedbox_control -C metadata={metadata} -C extra-filename=test`')
    print(f'Running `rustc --crate-name seedbox_vmm -C metadata={metadata} -C extra-filename=test`')
elif os.environ['TEST_MODE'] == 'shared-leak':
    data += os.environ['TEST_SOURCE_ROOT'] + '/outputs/out/lib\n'
elif os.environ['TEST_MODE'].startswith('shared-'):
    variable = {'shared-home': 'HOME', 'shared-cargo': 'CARGO_HOME',
                'shared-sysroot': 'TEST_SYSROOT'}[os.environ['TEST_MODE']]
    prefix = os.environ[variable]
    if variable == 'CARGO_HOME' and not prefix:
        prefix = str(Path(os.environ['HOME']) / '.cargo')
    data += prefix + '/source.rs\n'
binary.write_text(data)
''',
        }.items():
            path = self.bin / name
            path.write_text(script)
            path.chmod(0o755)

    def run_mode(self, mode, **overrides):
        env = dict(os.environ, PATH=f'{self.bin}{os.pathsep}{os.environ["PATH"]}',
                   TMPDIR=str(self.scratch), TEST_MODE=mode, TEST_SOURCE_ROOT=str(self.repo),
                   HOME=str(self.root / 'home with spaces'),
                   CARGO_HOME=str(self.root / 'custom cargo'),
                   TEST_SYSROOT=str(self.root / 'toolchain'))
        env.update(overrides)
        result = subprocess.run(
            ['bash', str(self.runner)], cwd=self.repo, capture_output=True, text=True,
            env=env)
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
        self.assertIn('per-crate -C metadata follows', result.stderr)
        for crate in ('seedbox_control', 'seedbox_vmm'):
            for metadata in ('aaa', 'bbb'):
                self.assertIn(f'{crate} metadata={metadata}', result.stderr)

    def test_shared_checkout_rpath_is_rejected_even_with_equal_hashes(self):
        result = self.run_mode('shared-leak')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn('retains an absolute build path', result.stderr)

    def test_shared_home_cargo_and_sysroot_leaks_fail_with_equal_hashes(self):
        for mode in ('shared-home', 'shared-cargo', 'shared-sysroot'):
            with self.subTest(mode=mode):
                result = self.run_mode(mode)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn('retains an absolute build path', result.stderr)

    def test_root_home_is_skipped_and_default_cargo_home_is_checked(self):
        result = self.run_mode('match', HOME='/')
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_mode('shared-cargo', CARGO_HOME='')
        # The fake compiler emits Cargo's default registry location when unset.
        self.assertEqual(result.returncode, 1, result.stderr)

    def test_build_failure_cleans_up(self):
        result = self.run_mode('build-failure')
        self.assertEqual(result.returncode, 7, result.stderr)


class CargoTests(unittest.TestCase):
    def test_alias_remaps_a_portable_debug_fixture_without_affecting_plain_builds(self):
        # Compile a tiny portable library, not the Linux-only VMM. This exercises
        # Cargo's path-source metadata as well as DWARF/file! remapping. Use the
        # production in-workspace package and shared source path, so removing
        # that isolation fails here even when source paths are remapped.
        production = HARNESS.parent / 'crates/seedbox-vmm'
        manifest = tomllib.loads((production / 'Cargo.toml').read_text())
        root_manifest = tomllib.loads((HARNESS.parent / 'Cargo.toml').read_text())
        self.assertEqual(manifest['workspace']['package'], root_manifest['workspace']['package'])
        for key in ('serde', 'serde_json'):
            self.assertEqual(manifest['workspace']['dependencies'][key],
                             root_manifest['workspace']['dependencies'][key])
        control_dependency = manifest['dependencies']['seedbox-control']
        control_path = control_dependency['path']
        control_manifest = tomllib.loads((production / control_path / 'Cargo.toml').read_text())
        self.assertEqual(control_dependency['package'], control_manifest['package']['name'])
        shared_source = control_manifest['lib']['path']
        self.assertEqual((production / control_path / shared_source).resolve(),
                         (HARNESS.parent / 'crates/seedbox-control/src/lib.rs').resolve())
        def metadata(log):
            return {name: value for name, value in re.findall(r'--crate-name (\S+).*? -C metadata=(\w+)', log) if name != 'build_script_build'}
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory).resolve()
            binaries = []
            metadatas, external_binaries, external_metadatas = [], [], []
            for name in ('first', 'a longer checkout with spaces'):
                root = base / name
                (root / '.cargo').mkdir(parents=True)
                (root / 'harness').mkdir()
                crate = root / 'crates/seedbox-vmm'
                (crate / 'src').mkdir(parents=True)
                shutil.copy2(HARNESS / 'vmm-rustc.py', root / 'harness/vmm-rustc.py')
                shutil.copy2(HARNESS.parent / '.cargo/config.toml', root / '.cargo/config.toml')
                package_keys = '''[workspace.package]
version = "0.1.0"
edition = "2021"
publish = false
'''
                (root / 'Cargo.toml').write_text('''[workspace]
members = ["crates/seedbox-control", "crates/seedbox"]
exclude = ["crates/seedbox-vmm"]
''' + package_keys)
                dep = root / 'crates/seedbox-control'
                (dep / 'src').mkdir(parents=True)
                (dep / 'Cargo.toml').write_text('''[package]
name = "seedbox-control"
version.workspace = true
edition.workspace = true
publish.workspace = true
''')
                (dep / 'src/lib.rs').write_text("pub fn source() -> &'static str { file!() }\n")
                # Mirror the production package boundary while retaining one
                # shared source file, also used by the ordinary root package.
                isolated = crate / control_path
                isolated.mkdir(parents=True)
                (isolated / 'Cargo.toml').write_text(
                    '[package]\nname = ' + json.dumps(control_dependency['package']) +
                    '\nversion.workspace = true\nedition.workspace = true\npublish.workspace = true\n' +
                    '[lib]\nname = ' + json.dumps(control_manifest['lib']['name']) +
                    '\npath = ' + json.dumps(shared_source) + '\n')
                # Use the same watched source layout, including the build script,
                # so this checks metadata and bytes after the cache fix too.
                source = isolated / shared_source
                source.parent.mkdir(parents=True, exist_ok=True)
                source.symlink_to(os.path.relpath(dep / 'src/lib.rs', source.parent))
                shutil.copy2(production / control_path / 'build.rs', isolated / 'build.rs')
                dependency = ('seedbox-control = { package = ' +
                              json.dumps(control_dependency['package']) +
                              ', path = ' + json.dumps(control_path) + ' }')
                fixture_manifest = '''[package]
name = "seedbox-vmm"
version = "0.1.0"
edition = "2021"
[dependencies]
CONTROL_DEPENDENCY
[workspace]
''' + package_keys + '''
[profile.release]
debug = 1
'''
                fixture_manifest = fixture_manifest.replace('CONTROL_DEPENDENCY', dependency)
                (crate / 'Cargo.toml').write_text(fixture_manifest)
                fixture_lock = '''version = 4
[[package]]
name = "seedbox-vmm-control"
version = "0.1.0"
[[package]]
name = "seedbox-vmm"
version = "0.1.0"
dependencies = ["seedbox-vmm-control"]
'''
                (crate / 'Cargo.lock').write_text(fixture_lock)
                (crate / 'src/lib.rs').write_text(
                    "pub fn source() -> &'static str { seedbox_control::source() }\n")
                consumer = root / 'crates/seedbox'
                (consumer / 'src').mkdir(parents=True)
                (consumer / 'Cargo.toml').write_text('''[package]
name = "seedbox"
version.workspace = true
edition.workspace = true
[dependencies]
seedbox-control = { path = "../seedbox-control" }
seedbox-vmm = { path = "../seedbox-vmm" }
''')
                (consumer / 'src/lib.rs').write_text(
                    "pub fn sources() -> [&'static str; 2] { "
                    "[seedbox_control::source(), seedbox_vmm::source()] }\n")
                env = dict(os.environ, CARGO_TARGET_DIR=str(base / 'unrelated-target'))
                # The combined graph must resolve both control packages sharing
                # the same schema source without a Cargo lockfile identity clash.
                combined = subprocess.run(
                    ['cargo', 'build', '--offline', '--manifest-path', str(root / 'Cargo.toml'),
                     '--target-dir', str(base / 'combined-target')],
                    cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(combined.returncode, 0, combined.stderr)
                self.assertNotIn('vmm-rustc.py', combined.stderr)
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
                metadatas.append(metadata(result.stderr))
                self.assertEqual(set(metadatas[-1]), {'seedbox_control', 'seedbox_vmm'})
                self.assertNotIn(str(root).encode(), binaries[-1])
                # A plain build in the same config scope must omit the wrapper.
                plain = subprocess.run(
                    ['cargo', 'build', '--offline', '--locked', '-v', '--manifest-path',
                     str(crate / 'Cargo.toml')], cwd=root, env=env,
                    capture_output=True, text=True)
                self.assertEqual(plain.returncode, 0, plain.stderr)
                self.assertNotIn('vmm-rustc.py', plain.stderr)
                # Negative control: the direct sibling dependency must still
                # reproduce the metadata blocker, even through the wrapper.
                (crate / 'Cargo.toml').write_text(fixture_manifest.replace(
                    dependency, 'seedbox-control = { path = "../seedbox-control" }'))
                (crate / 'Cargo.lock').write_text(
                    fixture_lock.replace('seedbox-vmm-control', 'seedbox-control'))
                external = subprocess.run(['cargo', 'build-vmm', '--offline', '-v'],
                                          cwd=root, env=env, capture_output=True, text=True)
                self.assertEqual(external.returncode, 0, external.stderr)
                external_metadatas.append(metadata(external.stderr))
                external_binaries.append((crate / 'target/release/libseedbox_vmm.rlib').read_bytes())
                (crate / 'Cargo.toml').write_text(fixture_manifest)
                (crate / 'Cargo.lock').write_text(fixture_lock)
            details = f'per-crate -C metadata: {metadatas}'
            self.assertEqual(metadatas[0], metadatas[1], details)
            self.assertEqual(binaries[0], binaries[1], details)
            for key in ('seedbox_control', 'seedbox_vmm'):
                self.assertNotEqual(external_metadatas[0][key], external_metadatas[1][key],
                                    f'per-crate -C metadata: {external_metadatas}')
            self.assertNotEqual(external_binaries[0], external_binaries[1])
            restored = subprocess.run(['cargo', 'build-vmm', '--offline'],
                                      cwd=root, env=env, capture_output=True, text=True)
            self.assertEqual(restored.returncode, 0, restored.stderr)
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
