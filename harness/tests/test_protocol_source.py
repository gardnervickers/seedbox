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
sha2="0.10"
seedbox-control={path="crates/seedbox-control"}
'''


class ProtocolSourceTests(unittest.TestCase):
    def test_warm_combined_and_standalone_graphs_follow_shared_source_changes(self):
        self.check_shared_sources('seedbox-control', 'control')

    def test_core_modules_follow_edits_additions_and_checkout_switches(self):
        self.check_shared_sources('seedbox-engine-core', 'engine-core')

    def check_shared_sources(self, canonical, package):
        wrappers = [p for p in (ROOT / 'crates').glob('*/' + package + '/Cargo.toml')
                    if p.parent.parent.name in ['seedbox-vmm', 'seedbox-arm64']]
        self.assertTrue(wrappers)
        with tempfile.TemporaryDirectory(prefix='seedbox-protocol-cache-') as directory:
            temporary = Path(directory)
            first = temporary / 'first'
            shared = first / 'crates' / canonical
            (shared / 'src').mkdir(parents=True)
            shutil.copy2(ROOT / 'crates' / canonical / 'Cargo.toml', shared / 'Cargo.toml')
            def source(root, value, new_api=False):
                src = root / 'crates' / canonical / 'src'
                function = 'new_api' if new_api else 'value'
                if package == 'engine-core':
                    # lib.rs stays unchanged during the warm edit. The second
                    # checkout adds a new submodule required by the consumer.
                    module = 'added' if new_api else 'policy'
                    if not (src / 'lib.rs').exists() or new_api:
                        (src / 'lib.rs').write_text('mod ' + module + '; pub use ' + module + '::' + function + ';\n')
                    (src / (module + '.rs')).write_text('pub fn ' + function + '() -> u32 { ' + str(value) + ' }\n')
                else:
                    (src / 'lib.rs').write_text('pub fn ' + function + '() -> u32 { ' + str(value) + ' }\n')
            source(first, 1)
            if package == 'engine-core':
                protocol = first / 'crates/seedbox-control'
                (protocol / 'src').mkdir(parents=True)
                shutil.copy2(ROOT / 'crates/seedbox-control/Cargo.toml', protocol / 'Cargo.toml')
                (protocol / 'src/lib.rs').write_text('// protocol dependency fixture\n')
            first.mkdir(exist_ok=True)
            (first / 'Cargo.toml').write_text('[workspace]\nmembers=["crates/consumer","crates/' + canonical + '"]\nexclude=[' + ','.join('"crates/' + p.parent.parent.name + '"' for p in wrappers) + ']\n' + WORKSPACE)
            consumer = first / 'crates/consumer'
            (consumer / 'src').mkdir(parents=True)
            aliases = ['protocol']
            dependencies = ['protocol={package="' + canonical + '",path="../' + canonical + '"}']
            for wrapper in wrappers:
                engine = wrapper.parent.parent.name
                data = tomllib.loads(wrapper.read_text())
                self.assertEqual(data['lib']['path'], 'src/lib.rs')
                wrapper_source = wrapper.parent / 'src/lib.rs'
                self.assertTrue(wrapper_source.is_symlink() or wrapper_source.parent.is_symlink(), wrapper_source)
                self.assertEqual(wrapper_source.resolve(), (ROOT / 'crates' / canonical / 'src/lib.rs').resolve())
                destination = first / 'crates' / engine / package
                shutil.copytree(wrapper.parent, destination, symlinks=True)
                if package == 'engine-core':
                    shutil.copytree(wrapper.parent.parent / 'control', destination.parent / 'control', symlinks=True)
                alias = engine.replace('-', '_') + '_protocol'
                aliases.append(alias)
                dependencies.append(alias + '={package="' + data['package']['name'] + '",path="../' + engine + '/' + package + '"}')
                (destination.parent / 'src').mkdir()
                (destination.parent / 'Cargo.toml').write_text('[package]\nname="' + engine + '-probe"\nversion="0.1.0"\nedition="2021"\n[workspace]\n' + WORKSPACE + '[dependencies]\nprotocol={package="' + data['package']['name'] + '",path="' + package + '"}\n')
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
            source(first, 2)
            for name, relative, binary, used in graphs:
                build(first, name, relative, binary, 2 * len(used))
            second = temporary / 'second'
            shutil.copytree(first, second, symlinks=True)
            source(second, 3, new_api=True)
            for name, relative, binary, used in graphs:
                main = second / relative / 'src/main.rs'
                main.write_text('fn main(){println!("{}",' + '+'.join(a + '::new_api()' for a in used) + ');}\n')
                # The precheck cache is reused across different worktree paths.
                build(second, name, relative, binary, 3 * len(used))
