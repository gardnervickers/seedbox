"""Portable pre-check controls with fake commands and real Git worktrees."""
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
            ['bash', '-c', 'source "$1"\n' + commands + '\ncheck_summary',
             'check-test', str(HARNESS / 'mac-check-steps.sh')],
            capture_output=True, text=True, check=False,
        )

    def test_all_pass_and_timing(self):
        result = self.run_steps('''date() { echo "${CHECK_CLOCK:-0}"; }
check_step first true
advance_clock() { CHECK_CLOCK=3; }
check_step timed advance_clock''')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, 'first    pass    0s\ntimed    pass    3s\n')

    def test_failures_do_not_hide_later_steps_or_pollute_summary(self):
        result = self.run_steps('''check_step first bash -c 'echo first-log; exit 7'
check_step second bash -c 'echo second-log; exit 2'
check_step last bash -c 'echo last-log'
''')
        self.assertEqual(result.returncode, 1)
        self.assertRegex(result.stdout, r'^first    FAIL +\d+s\nsecond   FAIL +\d+s\nlast     pass +\d+s\n$')
        for log in ('first-log', 'second-log', 'last-log'):
            self.assertIn(log, result.stderr)

    def test_phase_lines_preserve_host_summary_format(self):
        result = self.run_steps("check_step smoke bash -c 'echo phase run pass 2s; echo other-log'")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(result.stdout, r'^smoke    pass +\d+s\nphase run pass 2s\n$')
        self.assertIn('other-log', result.stderr)
        self.assertNotIn('other-log', result.stdout)


class SuiteTests(unittest.TestCase):
    def test_runner_executes_three_commands_after_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'bin').mkdir()
            (root / 'harness').mkdir()
            (root / 'harness' / 'acceptance.sh').write_text(
                'echo "portable $*" >> "$CHECK_TEST_LOG"\n')
            for name in ('cargo', 'python3'):
                path = root / 'bin' / name
                path.write_text(f'''#!/bin/sh
echo "{name} $*" >> "$CHECK_TEST_LOG"
echo command-output
if [ "$1" = clippy ]; then exit "$CHECK_TEST_STATUS"; fi
''')
                path.chmod(0o755)
            log = root / 'commands'
            env = dict(os.environ, PATH=f'{root / "bin"}{os.pathsep}{os.environ["PATH"]}',
                       CHECK_TEST_LOG=str(log), CHECK_TEST_STATUS='7')
            result = subprocess.run(['bash', str(HARNESS / 'mac-check-steps.sh')],
                                    cwd=root, env=env, capture_output=True,
                                    text=True, check=False)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertEqual(log.read_text().splitlines(), [
                'cargo fmt -p seedbox --check',
                'cargo clippy -p seedbox --all-targets --locked -- -D warnings',
                'portable portable',
            ])
            self.assertEqual([line.split()[:2] for line in result.stdout.splitlines()], [
                ['fmt', 'pass'], ['clippy', 'FAIL'], ['portable', 'pass'],
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
        (self.repo / 'harness').mkdir()
        (self.repo / 'harness' / 'acceptance.sh').write_text('true\n')
        self.git('add', 'tracked', 'harness/acceptance.sh')
        self.git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
                 '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture')
        self.git('branch', 'fixture-branch')
        # Keep the runner uncommitted: the selected revision does not have it.
        for name in ('mac-check.sh', 'mac-check-steps.sh'):
            shutil.copyfile(HARNESS / name, self.repo / 'harness' / name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.tmpdir = self.root / 'temporary worktrees'
        self.tmpdir.mkdir()
        self.log = self.root / 'shell-entry'
        for name, script in {
            'uname': '#!/bin/sh\necho Darwin\n',
            'cargo': '''#!/bin/sh
if [ "$1" = fmt ]; then
pwd > "$CHECK_TEST_LOG"
cat tracked >> "$CHECK_TEST_LOG"
echo "$CARGO_TARGET_DIR" >> "$CHECK_TEST_LOG"
echo build >> "$CARGO_TARGET_DIR/cache-marker"
fi
if [ "$1" = clippy ]; then exit "$CHECK_TEST_STATUS"; fi
''',
            'python3': '#!/bin/sh\nexit 0\n',
            'nix': '''#!/bin/sh
echo 'must use the existing development shell' >&2
exit 99
''',
        }.items():
            path = self.bin / name
            path.write_text(script)
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=f'{self.bin}{os.pathsep}{os.environ["PATH"]}',
                        TMPDIR=str(self.tmpdir), CHECK_TEST_LOG=str(self.log),
                        CHECK_TEST_STATUS='0', HOME=str(self.root))
        for name in ('XDG_CACHE_HOME', 'CARGO_TARGET_DIR', 'SEEDBOX_PRECHECK_TARGET'):
            self.env.pop(name, None)

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.repo), *args], check=True,
                              capture_output=True, text=True).stdout

    def invoke(self, *args):
        return subprocess.run(['bash', 'harness/mac-check.sh', *args],
                              cwd=self.repo, env=self.env, capture_output=True,
                              text=True, check=False)

    def test_revision_cleanup_on_success(self):
        (self.repo / 'tracked').write_text('dirty checkout\n')
        before = self.git('worktree', 'list', '--porcelain')
        sha = self.git('rev-parse', 'HEAD').strip()
        cache = self.root / 'Library' / 'Caches' / 'seedbox' / 'precheck-target'
        for count, revision in enumerate(('HEAD', 'fixture-branch', sha), start=1):
            with self.subTest(revision=revision):
                result = self.invoke(revision)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines()[0], f'revision: {sha}')
                lines = self.log.read_text().splitlines()
                self.assertTrue(Path(lines[0]).is_relative_to(self.tmpdir.resolve()))
                self.assertEqual(lines[1], 'committed')
                self.assertEqual(lines[2], str(cache))
                self.assertEqual((cache / 'cache-marker').read_text().splitlines(),
                                 ['build'] * count)
                self.assertFalse(Path(lines[0]).exists())
                self.assertEqual(list(self.tmpdir.iterdir()), [])
                self.assertEqual(self.git('worktree', 'list', '--porcelain'), before)
        self.assertEqual((self.repo / 'tracked').read_text(), 'dirty checkout\n')

    def test_revision_kept_on_failure(self):
        self.env['CHECK_TEST_STATUS'] = '42'
        result = self.invoke('fixture-branch')
        self.assertEqual(result.returncode, 1, result.stderr)
        source = Path(self.log.read_text().splitlines()[0])
        self.assertTrue(source.exists())
        self.assertIn(f'FAIL: worktree kept at {source}', result.stderr)
        self.assertIn(str(source), self.git('worktree', 'list', '--porcelain'))
        self.assertIn('portable pass', result.stdout)
        self.git('worktree', 'remove', '--force', str(source))

    def test_omitted_revision_uses_local_changes(self):
        (self.repo / 'tracked').write_text('local changes\n')
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0],
                         f'revision: {self.git("rev-parse", "HEAD").strip()}')
        self.assertIn('including local changes', result.stderr)
        self.assertEqual(self.log.read_text().splitlines(),
                         [str(self.repo.resolve()), 'local changes',
                          str(self.root / 'Library' / 'Caches' / 'seedbox' / 'precheck-target')])
        self.assertEqual(list(self.tmpdir.iterdir()), [])

    def test_invalid_revision_does_not_create_worktree(self):
        result = self.invoke('missing-revision')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.log.exists())
        self.assertEqual(list(self.tmpdir.iterdir()), [])

    def test_termination_keeps_worktree(self):
        fake_cargo = self.bin / 'cargo'
        fake_cargo.write_text('#!/bin/sh\npwd > "$CHECK_TEST_LOG"\nkill -TERM "$PPID"\n')
        result = self.invoke('HEAD')
        self.assertEqual(result.returncode, 143, result.stderr)
        source = Path(self.log.read_text().strip())
        self.assertTrue(source.exists())
        self.assertIn(f'FAIL: worktree kept at {source}', result.stderr)
        self.git('worktree', 'remove', '--force', str(source))

    def test_bad_arguments_do_not_create_worktree(self):
        for args in (('--unknown',), ('HEAD', 'extra')):
            with self.subTest(args=args):
                result = self.invoke(*args)
                self.assertEqual(result.returncode, 2)
                self.assertIn('Usage: harness/mac-check.sh', result.stderr)
                self.assertEqual(list(self.tmpdir.iterdir()), [])

    def test_non_mac_host_runs_with_persistent_cache(self):
        (self.bin / 'uname').write_text('#!/bin/sh\necho Linux\n')
        result = self.invoke('HEAD')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text().splitlines()[2],
                         str(self.root / '.cache' / 'seedbox' / 'precheck-target'))
        self.assertEqual(list(self.tmpdir.iterdir()), [])

    def test_xdg_cache_home(self):
        self.env['XDG_CACHE_HOME'] = str(self.root / 'xdg cache')
        result = self.invoke('HEAD')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text().splitlines()[2],
                         str(self.root / 'xdg cache' / 'seedbox' / 'precheck-target'))

    def test_relative_target_overrides_resolve_before_entering_worktree(self):
        for name in ('CARGO_TARGET_DIR', 'SEEDBOX_PRECHECK_TARGET'):
            with self.subTest(name=name):
                self.env[name] = 'shared target'
                result = self.invoke('HEAD')
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.log.read_text().splitlines()[2],
                                 str(self.repo / 'shared target'))
                self.env.pop(name)

    def test_precheck_override_takes_precedence_over_cargo_target(self):
        self.env['CARGO_TARGET_DIR'] = str(self.root / 'cargo target')
        self.env['SEEDBOX_PRECHECK_TARGET'] = str(self.root / 'precheck target')
        result = self.invoke('HEAD')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.log.read_text().splitlines()[2],
                         self.env['SEEDBOX_PRECHECK_TARGET'])
        self.assertFalse(Path(self.env['CARGO_TARGET_DIR']).exists())


if __name__ == '__main__':
    unittest.main()
