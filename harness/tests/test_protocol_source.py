"""Shared protocol package inputs must follow the consumer's current checkout."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[2]
WORKSPACE = '''[workspace.package]
version="0.1.0"
edition="2021"
publish=false
[workspace.dependencies]
serde={version="1",features=["derive"]}
serde_json="1"
'''


class ProtocolSourceTests(unittest.TestCase):
    def test_warm_combined_and_standalone_graphs_follow_shared_source_changes(self):
        wrappers = [p for p in (ROOT / 'crates').glob('*/control/Cargo.toml')
                    if p.parent.parent.name in ['seedbox-vmm', 'seedbox-arm64']]
        self.assertTrue(wrappers)
        with tempfile.TemporaryDirectory(prefix='seedbox-protocol-cache-') as directory:
            temporary = Path(directory)
            first = temporary / 'first'
            shared = first / 'crates/seedbox-control'
            (shared / 'src').mkdir(parents=True)
            shutil.copy2(ROOT / 'crates/seedbox-control/Cargo.toml', shared / 'Cargo.toml')
            (shared / 'src/lib.rs').write_text('pub fn value() -> u32 { 1 }\n')
            first.mkdir(exist_ok=True)
            (first / 'Cargo.toml').write_text('[workspace]\nmembers=["crates/consumer","crates/seedbox-control"]\nexclude=[' + ','.join('"crates/' + p.parent.parent.name + '"' for p in wrappers) + ']\n' + WORKSPACE)
            consumer = first / 'crates/consumer'
            (consumer / 'src').mkdir(parents=True)
            aliases = ['protocol']
            dependencies = ['protocol={package="seedbox-control",path="../seedbox-control"}']
            for wrapper in wrappers:
                engine = wrapper.parent.parent.name
                data = tomllib.loads(wrapper.read_text())
                self.assertEqual(data['lib']['path'], 'src/lib.rs')
                source = wrapper.parent / 'src/lib.rs'
                self.assertTrue(source.is_symlink(), source)
                self.assertEqual(source.resolve(), (ROOT / 'crates/seedbox-control/src/lib.rs').resolve())
                destination = first / 'crates' / engine / 'control'
                shutil.copytree(wrapper.parent, destination, symlinks=True)
                alias = engine.replace('-', '_') + '_protocol'
                aliases.append(alias)
                dependencies.append(alias + '={package="' + data['package']['name'] + '",path="../' + engine + '/control"}')
                (destination.parent / 'src').mkdir()
                (destination.parent / 'Cargo.toml').write_text('[package]\nname="' + engine + '-probe"\nversion="0.1.0"\nedition="2021"\n[workspace]\n' + WORKSPACE + '[dependencies]\nprotocol={package="' + data['package']['name'] + '",path="control"}\n')
                (destination.parent / 'src/main.rs').write_text('fn main(){println!("{}",protocol::value());}\n')
            (consumer / 'Cargo.toml').write_text('[package]\nname="combined-probe"\nversion.workspace=true\nedition.workspace=true\n[dependencies]\n' + '\n'.join(dependencies) + '\n')
            (consumer / 'src/main.rs').write_text('fn main(){println!("{}",' + '+'.join(a + '::value()' for a in aliases) + ');}\n')
            graphs = [('combined', 'crates/consumer', 'combined-probe', aliases)]
            graphs.extend((p.parent.parent.name, 'crates/' + p.parent.parent.name, p.parent.parent.name + '-probe', ['protocol']) for p in wrappers)

            def build(root, name, relative, binary, expected):
                environment = os.environ.copy()
                environment.pop('CARGO_BUILD_TARGET', None)
                environment['CARGO_TARGET_DIR'] = str(temporary / (name + '-target'))
                result = subprocess.run(['cargo', 'build', '--offline', '--manifest-path', str(root / relative / 'Cargo.toml')], env=environment, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=120)
                self.assertEqual(result.returncode, 0, result.stdout)
                actual = subprocess.check_output([str(temporary / (name + '-target/debug') / binary)], text=True).strip()
                self.assertEqual(actual, str(expected), result.stdout)

            for name, relative, binary, used in graphs:
                build(first, name, relative, binary, len(used))
            # Only the shared source changes: consumers must not retain old output.
            (shared / 'src/lib.rs').write_text('pub fn value() -> u32 { 2 }\n')
            for name, relative, binary, used in graphs:
                build(first, name, relative, binary, 2 * len(used))
            second = temporary / 'second'
            shutil.copytree(first, second, symlinks=True)
            (second / 'crates/seedbox-control/src/lib.rs').write_text('pub fn new_api() -> u32 { 3 }\n')
            for name, relative, binary, used in graphs:
                main = second / relative / 'src/main.rs'
                main.write_text('fn main(){println!("{}",' + '+'.join(a + '::new_api()' for a in used) + ');}\n')
                # The precheck cache is reused across different worktree paths.
                build(second, name, relative, binary, 3 * len(used))
