"""Portable precheck controls; fake shell entry, no Nix, Cargo or KVM."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


HARNESS = Path(__file__).resolve().parents[1]


class SummaryTests(unittest.TestCase):
    def run_steps(self, commands):
        return subprocess.run(
            ['bash', '-c', 'source "$1"\n' + commands + '\nprecheck_summary',
             'precheck-test', str(HARNESS / 'mac-precheck-steps.sh')],
            capture_output=True, text=True, check=False,
        )

    def test_all_pass_and_timing(self):
        result = self.run_steps('''precheck_step first true
advance_clock() { SECONDS=$((SECONDS + 3)); }
precheck_step timed advance_clock''')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(result.stdout, r'^PASS first \d+s\nPASS timed \d+s\n$')
        self.assertGreaterEqual(int(result.stdout.splitlines()[1].split()[2][:-1]), 3)

    def test_failures_do_not_hide_later_steps_or_pollute_summary(self):
        result = self.run_steps('''precheck_step first bash -c 'echo first-log; exit 7'
precheck_step second bash -c 'echo second-log; exit 2'
precheck_step last bash -c 'echo last-log'
''')
        self.assertEqual(result.returncode, 1)
        self.assertRegex(result.stdout, r'^FAIL first \d+s\nFAIL second \d+s\nPASS last \d+s\n$')
        for log in ('first-log', 'second-log', 'last-log'):
            self.assertIn(log, result.stderr)


class SuiteTests(unittest.TestCase):
    def test_runner_executes_all_six_commands_after_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'bin').mkdir()
            (root / 'harness').mkdir()
            (root / 'harness' / 'acceptance.sh').write_text(
                'echo "portable $*" >> "$PRECHECK_TEST_LOG"\n')
            for name in ('cargo', 'python3'):
                path = root / 'bin' / name
                path.write_text(f'''#!/bin/sh
echo "{name} $*" >> "$PRECHECK_TEST_LOG"
echo command-output
if [ "$1" = clippy ]; then exit "$PRECHECK_TEST_STATUS"; fi
''')
                path.chmod(0o755)
            log = root / 'commands'
            env = dict(os.environ, PATH=f'{root / "bin"}{os.pathsep}{os.environ["PATH"]}',
                       PRECHECK_TEST_LOG=str(log), PRECHECK_TEST_STATUS='7')
            result = subprocess.run(['bash', str(HARNESS / 'mac-precheck-steps.sh')],
                                    cwd=root, env=env, capture_output=True,
                                    text=True, check=False)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual(log.read_text().splitlines(), [
                'cargo fmt -p seedbox --check',
                'cargo clippy -p seedbox --all-targets --locked -- -D warnings',
                'cargo test --workspace --locked',
                'portable portable',
                'python3 -m unittest discover harness/tests',
                'python3 -m unittest discover python/tests',
            ])
            self.assertEqual([line.split()[:2] for line in result.stdout.splitlines()], [
                ['PASS', 'fmt'], ['FAIL', 'clippy'], ['PASS', 'workspace'],
                ['PASS', 'portable'], ['PASS', 'harness-python'], ['PASS', 'python'],
            ])
            self.assertIn('command-output', result.stderr)


class WorktreeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.repo = self.root / 'repo with spaces'
        self.repo.mkdir()
        self.git('init', '-q')
        (self.repo / 'tracked').write_text('committed\n')
        self.git('add', 'tracked')
        self.git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
                 '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture')
        self.git('branch', 'fixture-branch')
        # Keep the runner uncommitted: the selected revision does not have it.
        (self.repo / 'harness').mkdir()
        for name in ('mac-precheck.sh', 'mac-precheck-steps.sh'):
            shutil.copyfile(HARNESS / name, self.repo / 'harness' / name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.tmpdir = self.root / 'temporary worktrees'
        self.tmpdir.mkdir()
        self.log = self.root / 'shell-entry'
        for name, script in {
            'uname': '#!/bin/sh\necho Darwin\n',
            'nix': '''#!/bin/sh
[ "$#" -eq 4 ] && [ "$1" = develop ] && [ "$2" = --command ] && [ "$3" = bash ] || exit 99
[ -f "$4" ] || exit 98
pwd > "$PRECHECK_TEST_LOG"
cat tracked >> "$PRECHECK_TEST_LOG"
exit "$PRECHECK_TEST_STATUS"
''',
        }.items():
            path = self.bin / name
            path.write_text(script)
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=f'{self.bin}{os.pathsep}{os.environ["PATH"]}',
                        TMPDIR=str(self.tmpdir), PRECHECK_TEST_LOG=str(self.log),
                        PRECHECK_TEST_STATUS='0')

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.repo), *args], check=True,
                              capture_output=True, text=True).stdout

    def invoke(self, *args):
        return subprocess.run(['bash', 'harness/mac-precheck.sh', *args],
                              cwd=self.repo, env=self.env, capture_output=True,
                              text=True, check=False)

    def test_revision_cleanup_on_success_and_failure(self):
        (self.repo / 'tracked').write_text('dirty checkout\n')
        before = self.git('worktree', 'list', '--porcelain')
        for status, revision in (('0', 'HEAD'), ('1', 'fixture-branch'),
                                 ('42', self.git('rev-parse', 'HEAD').strip())):
            with self.subTest(status=status):
                self.env['PRECHECK_TEST_STATUS'] = status
                result = self.invoke(revision)
                self.assertEqual(result.returncode, int(status), result.stderr)
                lines = self.log.read_text().splitlines()
                self.assertTrue(Path(lines[0]).is_relative_to(self.tmpdir.resolve()))
                self.assertEqual(lines[1], 'committed')
                self.assertFalse(Path(lines[0]).exists())
                self.assertEqual(list(self.tmpdir.iterdir()), [])
                self.assertEqual(self.git('worktree', 'list', '--porcelain'), before)
        self.assertEqual((self.repo / 'tracked').read_text(), 'dirty checkout\n')

    def test_omitted_revision_uses_local_changes(self):
        (self.repo / 'tracked').write_text('local changes\n')
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text().splitlines(),
                         [str(self.repo.resolve()), 'local changes'])
        self.assertEqual(list(self.tmpdir.iterdir()), [])

    def test_invalid_revision_does_not_create_worktree(self):
        result = self.invoke('missing-revision')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.log.exists())
        self.assertEqual(list(self.tmpdir.iterdir()), [])

    def test_termination_removes_worktree(self):
        fake_nix = self.bin / 'nix'
        fake_nix.write_text('#!/bin/sh\nkill -TERM "$PPID"\n')
        before = self.git('worktree', 'list', '--porcelain')
        result = self.invoke('HEAD')
        self.assertEqual(result.returncode, 143, result.stderr)
        self.assertEqual(list(self.tmpdir.iterdir()), [])
        self.assertEqual(self.git('worktree', 'list', '--porcelain'), before)


if __name__ == '__main__':
    unittest.main()
