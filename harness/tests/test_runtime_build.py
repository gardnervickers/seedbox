"""Cargo selects explicit or prepared guest assets without executing builders."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class RuntimeBuildTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.compiler_output = tempfile.TemporaryDirectory(prefix='seedbox-build-script-')
        cls.addClassCleanup(cls.compiler_output.cleanup)
        cls.script = Path(cls.compiler_output.name) / 'build-script'
        subprocess.run(
            ['rustc', str(ROOT / 'crates/seedbox-application/build-runtime.rs'),
             '-o', str(cls.script)],
            check=True, capture_output=True, timeout=120,
        )

    def setUp(self):
        directory = tempfile.TemporaryDirectory(prefix='seedbox-runtime-build-')
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.checkout = self.root / 'checkout'
        manifest = self.checkout / 'crates/seedbox-application'
        manifest.mkdir(parents=True)
        (manifest / 'Cargo.toml').write_text(
            '[package]\nname = "seedbox-application"\nversion = "0.0.0"\n'
        )
        self.output = self.root / 'output'
        self.output.mkdir()
        self.marker = self.root / 'guest-build-invoked'
        tools = self.root / 'forbidden-tools'
        tools.mkdir()
        for name in ('nix', 'docker', 'podman', 'python3', 'cc', 'git'):
            executable = tools / name
            executable.write_text(f'#!/bin/sh\necho {name} >> "{self.marker}"\nexit 97\n')
            executable.chmod(0o755)
        # A developer's asset overrides and prepared checkout must not supply
        # any inputs to the mock Cargo invocation.
        self.environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith('SEEDBOX_')
        }
        self.environment.update(
            CARGO_MANIFEST_DIR=str(manifest), OUT_DIR=str(self.output),
            PROFILE='release', CARGO_CFG_TARGET_OS='macos',
            CARGO_CFG_TARGET_ARCH='aarch64', PATH=str(tools),
            SEEDBOX_STATIC_CC=str(tools / 'cc'),
            SEEDBOX_BUILDER=str(tools / 'docker'),
        )

    def prepared_assets(self, architecture='aarch64'):
        assets = self.checkout / 'build/assets'
        assets.mkdir(parents=True)
        (assets / 'architecture').write_text(architecture + '\n')
        (assets / 'runtime.tar.gz').write_bytes(b'prepared helpers')
        (assets / 'kernel').write_bytes(b'prepared kernel')
        return assets

    def run_script(self):
        result = subprocess.run(
            [str(self.script)], env=self.environment, text=True,
            capture_output=True, timeout=30,
        )
        self.assertFalse(self.marker.exists(), result.stdout + result.stderr)
        self.assertFalse((self.output / 'compose-init').exists())
        return result

    def embedded_source(self):
        return (self.output / 'guest-helpers.rs').read_text()

    def test_release_build_script_never_invokes_guest_builders(self):
        helpers = self.root / 'explicit-helpers.tar.gz'
        helpers.write_bytes(b'explicit helpers')
        kernel = self.root / 'explicit-kernel'
        kernel.write_bytes(b'explicit kernel')
        self.environment.update(SEEDBOX_BUILD_HELPERS=str(helpers),
                                SEEDBOX_BUILD_KERNEL=str(kernel))
        for target_os, target_arch in [('linux', 'x86_64'), ('macos', 'aarch64')]:
            with self.subTest(target=(target_os, target_arch)):
                self.environment.update(CARGO_CFG_TARGET_OS=target_os,
                                        CARGO_CFG_TARGET_ARCH=target_arch)
                result = self.run_script()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn('SEEDBOX_RELEASE_ASSETS', result.stdout)
                self.assertIn(str(helpers.resolve()), self.embedded_source())
                self.assertIn(str(kernel.resolve()), self.embedded_source())

    def test_prepared_checkout_assets_are_embedded(self):
        assets = self.prepared_assets()
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(str((assets / 'runtime.tar.gz').resolve()), self.embedded_source())
        self.assertIn(str((assets / 'kernel').resolve()), self.embedded_source())

    def test_explicit_assets_override_prepared_checkout_assets(self):
        assets = self.prepared_assets()
        helpers = self.root / 'override-helpers.tar.gz'
        helpers.write_bytes(b'override helpers')
        kernel = self.root / 'override-kernel'
        kernel.write_bytes(b'override kernel')
        self.environment.update(SEEDBOX_BUILD_HELPERS=str(helpers),
                                SEEDBOX_BUILD_KERNEL=str(kernel))
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(str(helpers.resolve()), self.embedded_source())
        self.assertIn(str(kernel.resolve()), self.embedded_source())
        self.assertNotIn(str(assets.resolve()), self.embedded_source())

    def test_prepared_assets_for_another_architecture_are_rejected(self):
        self.prepared_assets('aarch64')
        self.environment.update(CARGO_CFG_TARGET_OS='linux',
                                CARGO_CFG_TARGET_ARCH='x86_64')
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('guest assets target a different architecture', result.stderr)
        self.assertIn('cargo xtask build-assets', result.stderr)
        self.assertFalse((self.output / 'guest-helpers.rs').exists())

    def test_missing_release_helpers_explain_the_explicit_build(self):
        self.environment['SEEDBOX_BUILD_HELPERS'] = str(self.root / 'missing.tar.gz')
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('cargo xtask build-assets', result.stderr)
        self.assertIn('missing.tar.gz', result.stderr)

    def test_ambient_cached_helpers_are_never_embedded(self):
        cache = self.root / 'ambient-cache'
        stale = cache / 'aarch64/runtime.tar.gz'
        stale.parent.mkdir(parents=True)
        stale.write_bytes(b'stale archive')
        self.environment['SEEDBOX_ASSET_DIR'] = str(cache)
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.embedded_source(),
            'pub static EMBEDDED: Option<&[u8]> = None;\n'
            'pub static EMBEDDED_KERNEL: Option<&[u8]> = None;\n',
        )
