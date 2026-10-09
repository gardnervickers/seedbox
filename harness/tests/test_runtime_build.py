"""Release Cargo builds select assets without executing guest build tools."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class RuntimeBuildTests(unittest.TestCase):
    def test_release_build_script_never_invokes_guest_builders(self):
        with tempfile.TemporaryDirectory(prefix='seedbox-runtime-build-') as directory:
            root = Path(directory)
            script = root / 'build-script'
            subprocess.run(['rustc', str(ROOT / 'crates/seedbox/build-runtime.rs'), '-o', str(script)],
                           check=True, capture_output=True, timeout=120)
            tools = root / 'forbidden-tools'
            tools.mkdir()
            marker = root / 'guest-build-invoked'
            for name in ('nix', 'docker', 'podman', 'python3', 'cc', 'git'):
                executable = tools / name
                executable.write_text(f'#!/bin/sh\necho {name} >> "{marker}"\nexit 97\n')
                executable.chmod(0o755)
            environment = os.environ.copy()
            helpers = root / 'helpers.tar.gz'
            helpers.write_bytes(b'build-only fixture')
            environment['SEEDBOX_BUILD_HELPERS'] = str(helpers)
            environment.update(CARGO_MANIFEST_DIR=str(ROOT / 'crates/seedbox'),
                               OUT_DIR=str(root), PROFILE='release', PATH=str(tools),
                               SEEDBOX_STATIC_CC=str(tools / 'cc'),
                               SEEDBOX_BUILDER=str(tools / 'docker'))
            for target_os, target_arch in [('linux', 'x86_64'), ('macos', 'aarch64')]:
                with self.subTest(target=(target_os, target_arch)):
                    environment.update(CARGO_CFG_TARGET_OS=target_os, CARGO_CFG_TARGET_ARCH=target_arch)
                    result = subprocess.run([str(script)], env=environment, text=True,
                                            capture_output=True, timeout=30)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertNotIn('SEEDBOX_RELEASE_ASSETS', result.stdout)
                    self.assertIn(str(helpers.resolve()), (root / 'guest-helpers.rs').read_text())
                    self.assertFalse(marker.exists(), result.stdout)
                    self.assertFalse((root / 'compose-init').exists())

    def test_missing_release_helpers_explain_the_explicit_build(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / 'build-script'
            subprocess.run(['rustc', str(ROOT / 'crates/seedbox/build-runtime.rs'), '-o', str(script)],
                           check=True, capture_output=True, timeout=120)
            environment = dict(os.environ, CARGO_MANIFEST_DIR=str(ROOT / 'crates/seedbox'),
                               OUT_DIR=str(root), PROFILE='release', CARGO_CFG_TARGET_ARCH='aarch64',
                               SEEDBOX_BUILD_HELPERS=str(root / 'missing.tar.gz'))
            result = subprocess.run([str(script)], env=environment, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('bash tools/build-assets.sh', result.stderr)
            self.assertIn('missing.tar.gz', result.stderr)
