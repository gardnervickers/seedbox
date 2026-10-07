"""Host gate orchestration controls; no Linux topology, root or KVM required."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / 'host-check.sh'


class HostCheckTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.repo = self.root / 'repo with spaces'
        self.repo.mkdir()
        self.git('init', '-q')
        (self.repo / 'harness').mkdir()
        (self.repo / 'harness' / 'acceptance.sh').write_text('''
echo 'portable suite' >> "$HOST_CHECK_TEST_LOG"
cargo test-vmm
cargo test --workspace --locked
''')
        self.git('add', '.')
        self.git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
                 '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture')
        self.sha = self.git('rev-parse', 'HEAD').strip()
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.work = self.root / 'work with spaces'
        self.work.mkdir()
        self.log = self.root / 'commands'
        scripts = {
            'cat': f'''#!/bin/sh
case "$1" in
  /sys/devices/system/cpu/*/topology/thread_siblings_list) echo 2,3 ;;
  *) exec "{shutil.which('cat')}" "$@" ;;
esac
''',
            'cargo': '''#!/bin/sh
echo "cargo $*" >> "$HOST_CHECK_TEST_LOG"
if [ "$1" = build ]; then
  mkdir -p target/release/build/seedbox-test/out
  touch target/release/build/seedbox-test/out/compose-init
fi
if [ "$1" = test-vmm ]; then exit "$HOST_CHECK_TEST_STATUS"; fi
''',
            'python3': '''#!/bin/sh
echo "python3 $*" >> "$HOST_CHECK_TEST_LOG"
echo 'phase run pass 0s'
''',
            'sha256sum': '#!/bin/sh\necho "0123456789abcdef kernel"\n',
            'nice': '#!/bin/sh\nexec "$@"\n',
            'sudo': '#!/bin/sh\nexec "$@"\n',
        }
        for name, script in scripts.items():
            path = self.bin / name
            path.write_text(script)
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=f'{self.bin}{os.pathsep}{os.environ["PATH"]}',
                        SEEDBOX_KERNEL=str(self.root / 'kernel'),
                        SEEDBOX_BUSYBOX=str(self.root / 'busybox'),
                        PINNED_ENGINE=str(self.root / 'pinned-engine'),
                        HOST_CHECK_DIR=str(self.work),
                        HOST_CHECK_TEST_LOG=str(self.log), HOST_CHECK_TEST_STATUS='0')
        self.env.pop('SEEDBOX_VMM', None)

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.repo), *args], check=True,
                              capture_output=True, text=True).stdout

    def invoke(self, *args):
        return subprocess.run(['bash', str(SCRIPT), *args], cwd=self.repo,
                              env=self.env, capture_output=True, text=True, check=False)

    def assert_pass(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f'host-check {self.sha} cpu 2 (2,3)', result.stdout)
        summaries = list(self.work.glob('*.txt'))
        self.assertEqual(len(summaries), 1)
        summary = summaries[0].read_text()
        self.assertIn(self.sha, summary)
        self.assertIn('phase run pass 0s', summary)
        self.assertEqual(list(self.work.iterdir()), summaries)
        self.assertEqual(self.git('worktree', 'list', '--porcelain').count('worktree '), 1)
        return self.log.read_text().splitlines()

    def test_default_keeps_portable_and_vmm_checks(self):
        commands = self.assert_pass(self.invoke('HEAD', '2'))
        self.assertEqual(commands[:7], [
            'cargo build --release --locked -p seedbox', 'cargo build-vmm',
            'cargo fmt -p seedbox --check',
            'cargo clippy -p seedbox --all-targets --locked -- -D warnings',
            'portable suite', 'cargo test-vmm', 'cargo test --workspace --locked',
        ])
        self.assertTrue(commands[7].startswith('python3 harness/smoke/amd.py '))

    def test_skip_portable_keeps_builds_vmm_tests_and_smoke(self):
        result = self.invoke('--skip-portable', 'HEAD', '2')
        commands = self.assert_pass(result)
        self.assertEqual(commands[:3], [
            'cargo build --release --locked -p seedbox',
            'cargo build-vmm', 'cargo test-vmm',
        ])
        self.assertEqual(len(commands), 4)
        self.assertTrue(commands[3].startswith('python3 harness/smoke/amd.py '))
        self.assertRegex(result.stdout, r'(?m)^vmm-test pass +\d+s$')

    def test_skip_portable_respects_stored_engine(self):
        self.env['SEEDBOX_VMM'] = str(self.root / 'stored-engine')
        self.env.pop('PINNED_ENGINE')
        commands = self.assert_pass(self.invoke('--skip-portable', self.sha, '2'))
        self.assertEqual(commands[:2], [
            'cargo build --release --locked -p seedbox', 'cargo test-vmm',
        ])
        self.assertEqual(len(commands), 3)
        self.assertIn(f'--engine {self.env["SEEDBOX_VMM"]}', commands[2])

    def test_skip_portable_vmm_test_failure_keeps_evidence(self):
        self.env['HOST_CHECK_TEST_STATUS'] = '7'
        result = self.invoke('--skip-portable', 'HEAD', '2')
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertRegex(result.stdout, r'(?m)^vmm-test FAIL +\d+s$')
        self.assertNotIn('python3', self.log.read_text())
        sources = list(self.work.glob('*/src'))
        self.assertEqual(len(sources), 1)
        self.assertTrue((sources[0].parent / 'vmm-test.log').exists())
        self.assertIn(str(sources[0].parent), result.stdout)
        self.git('worktree', 'remove', '--force', str(sources[0]))

    def test_invalid_arguments_fail_before_worktree_or_commands(self):
        for args in ((), ('--unknown', 'HEAD', '2'), ('--skip-portable', 'HEAD'),
                     ('--skip-portable', 'HEAD', 'bad'), ('HEAD', '2', 'extra')):
            with self.subTest(args=args):
                result = self.invoke(*args)
                self.assertEqual(result.returncode, 2)
                self.assertIn('Usage: harness/host-check.sh [--skip-portable]', result.stderr)
                self.assertFalse(self.log.exists())
                self.assertEqual(list(self.work.iterdir()), [])


if __name__ == '__main__':
    unittest.main()
