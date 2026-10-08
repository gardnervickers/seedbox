"""Compile the real portable core through the standalone package boundary."""
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]


class CoreBuildTests(unittest.TestCase):
    def test_standalone_core_metadata_and_bytes_are_checkout_independent(self):
        root_manifest = tomllib.loads((ROOT / 'Cargo.toml').read_text())
        with tempfile.TemporaryDirectory(prefix='seedbox-core-metadata-') as directory:
            temporary = Path(directory).resolve()
            observed = {False: [], True: []}
            for spelling in ['first', 'long checkout with spaces']:
                root = temporary / spelling
                (root / 'harness').mkdir(parents=True)
                shutil.copy2(ROOT / 'harness/vmm-rustc.py', root / 'harness/vmm-rustc.py')
                for name in ['seedbox-control', 'seedbox-engine-core']:
                    shutil.copytree(ROOT / 'crates' / name, root / 'crates' / name)
                engine = root / 'crates/seedbox-vmm'
                (engine / 'src').mkdir(parents=True)
                (engine / 'src/lib.rs').write_text('// The Linux machine target is not compiled.\n')
                for name in ['control', 'engine-core']:
                    shutil.copytree(ROOT / 'crates/seedbox-vmm' / name, engine / name, symlinks=True)
                manifest = tomllib.loads((ROOT / 'crates/seedbox-vmm/Cargo.toml').read_text())
                self.assertEqual(manifest['workspace']['package'], root_manifest['workspace']['package'])
                for key in ['serde', 'serde_json', 'sha2']:
                    self.assertEqual(manifest['workspace']['dependencies'][key], root_manifest['workspace']['dependencies'][key])
                text = (ROOT / 'crates/seedbox-vmm/Cargo.toml').read_text().replace('[workspace]\n', '[workspace]\nmembers = ["probe"]\n')
                (engine / 'Cargo.toml').write_text(text)
                (root / 'Cargo.toml').write_text('''[workspace]
members=["crates/seedbox-control","crates/seedbox-engine-core"]
exclude=["crates/seedbox-vmm"]
[workspace.package]
version="0.1.0"
edition="2021"
publish=false
[workspace.dependencies]
serde={version="1",features=["derive"]}
serde_json="1"
sha2="0.10"
seedbox-control={path="crates/seedbox-control"}
''')
                probe = engine / 'probe'
                (probe / 'src').mkdir(parents=True)
                (probe / 'src/lib.rs').write_text('pub fn value() -> u64 { core_policy::rng::Rng::new(1,"probe").next() }\n')
                for external in [False, True]:
                    dependency = ('core_policy={package="seedbox-engine-core",path="../../seedbox-engine-core"}'
                                  if external else 'core_policy={package="seedbox-vmm-engine-core",path="../engine-core"}')
                    (probe / 'Cargo.toml').write_text('[package]\nname="core-probe"\nversion.workspace=true\nedition.workspace=true\n[dependencies]\n' + dependency + '\n')
                    # Mirror the prescribed standalone target inside the remapped checkout.
                    target = engine / 'target' / ('external' if external else 'isolated')
                    env = os.environ.copy()
                    env.pop('CARGO_BUILD_TARGET', None)
                    result = subprocess.run([
                        'cargo', 'build', '--offline', '--release', '-vv',
                        '--manifest-path', str(engine / 'Cargo.toml'), '-p', 'core-probe',
                        '--target-dir', str(target), '--config', "build.rustc-wrapper='harness/vmm-rustc.py'",
                    ], cwd=root, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=180)
                    self.assertEqual(result.returncode, 0, result.stdout)
                    metadata = {name: value for name, value in re.findall(r'--crate-name (\S+).*? -C metadata=(\w+)', result.stdout)
                                if name in ['seedbox_control', 'seedbox_engine_core', 'core_probe']}
                    self.assertEqual(set(metadata), {'seedbox_control', 'seedbox_engine_core', 'core_probe'}, result.stdout)
                    binaries = {}
                    for name in ['core_probe', 'seedbox_engine_core', 'seedbox_control']:
                        paths = list((target / 'release/deps').glob('lib' + name + '-*.rlib'))
                        self.assertEqual(len(paths), 1, paths)
                        binary = paths[0].read_bytes()
                        if not external:
                            self.assertNotIn(str(root).encode(), binary, 'unremapped fixture checkout path')
                        binaries[name] = hashlib.sha256(binary).hexdigest()
                    observed[external].append((metadata, binaries))
            self.assertEqual(observed[False][0][0], observed[False][1][0])
            self.assertEqual(observed[False][0][1], observed[False][1][1])
            for name in ['seedbox_control', 'seedbox_engine_core', 'core_probe']:
                self.assertNotEqual(observed[True][0][0][name], observed[True][1][0][name], name)
            for name in ['seedbox_control', 'seedbox_engine_core', 'core_probe']:
                self.assertNotEqual(observed[True][0][1][name], observed[True][1][1][name], name)
