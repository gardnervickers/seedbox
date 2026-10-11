"""Resolve container entrypoint scripts without changing arguments or user policy."""
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class ProgressLaunchTests(unittest.TestCase):
    def test_interpreter_arguments_path_permissions_and_recursion(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'probe.c'
            source.write_text(r'''
#include "progress-launch.h"
int main(int argc, char **argv) {
    (void)argc;
    char **resolved = seedbox_script_arguments(argv + 1);
    if (!resolved) { printf("errno=%d\n", errno); return 2; }
    for (; *resolved; ++resolved) puts(*resolved);
    return 0;
}
''')
            probe = root / 'probe'
            subprocess.run(shlex.split(os.environ.get('CC', 'cc')) + [
                '-Wall', '-Wextra', '-Werror', '-I', str(ROOT / 'guest'),
                str(source), '-o', str(probe)], check=True, capture_output=True)
            interpreter = root / 'interpreter'
            interpreter.write_bytes(b'\x7fELF' + b'\x00' * 20)
            interpreter.chmod(0o755)
            script = root / 'entrypoint'
            script.write_text(f'#!  {interpreter}\targument with spaces  \nignored\n')
            script.chmod(0o755)
            def run(*args):
                return subprocess.run([str(probe), *args], cwd=root,
                                      env=dict(os.environ, PATH=str(root)),
                                      capture_output=True, text=True, timeout=10)
            result = run('entrypoint', 'literal $arg', 'two words')
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(result.stdout.splitlines(), [str(interpreter),
                'argument with spaces', str(script), 'literal $arg', 'two words'])
            self.assertEqual(run(str(interpreter), 'original').stdout.splitlines(),
                             [str(interpreter), 'original'])
            script.chmod(0o644)
            self.assertEqual(run(str(script)).returncode, 2)
            script.chmod(0o755)
            script.write_text(f'#!{script}\n')
            self.assertEqual(run(str(script)).returncode, 2)
            script.write_text('#!' + 'x' * 300)
            self.assertEqual(run(str(script)).returncode, 2)
            self.assertEqual(run('missing').returncode, 2)


class ProgressBudgetTests(unittest.TestCase):
    def test_strict_budget_parser_accepts_u32_range_and_rejects_bad_options(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'budget.c'
            source.write_text(r'''#include "progress/budget.h"
#include <stdio.h>
int main(int argc, char **argv) {
    uint32_t budget = 0;
    if (argc != 3 || !seedbox_progress_budget(argv[1], argv[2], &budget)) return 2;
    printf("%u\n", budget);
    return 0;
}
''')
            probe = root / 'budget'
            subprocess.run(shlex.split(os.environ.get('CC', 'cc')) + [
                '-std=c11', '-Wall', '-Wextra', '-Werror', '-I', str(ROOT / 'guest'),
                str(source), '-o', str(probe)], check=True, capture_output=True)
            for budget in ['1', '17', '10000', '4294967295']:
                result = subprocess.run([str(probe), '--blocks-per-progress-call', budget],
                                        capture_output=True, text=True, timeout=10)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stdout, budget + '\n')
            for budget in ['', '0', '-1', '+1', ' 1', '1 ', '1x', '4294967296',
                           '999999999999999999999999999999']:
                result = subprocess.run([str(probe), '--blocks-per-progress-call', budget],
                                        capture_output=True, timeout=10)
                self.assertEqual(result.returncode, 2, budget)
            for option in ['--other', '--blocks-per-progress-call-extra', '--blocks-per-progress-cal']:
                self.assertEqual(subprocess.run([str(probe), option, '1'], timeout=10).returncode, 2)


class ProgressJitterTests(unittest.TestCase):
    def test_jitter_option_is_bounded_and_draws_average_to_the_requested_time(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'jitter.c'
            source.write_text(r'''#include "progress/budget.h"
#include <stdio.h>
int main(int argc, char **argv) {
    uint32_t jitter = 0;
    if (argc != 3 || !seedbox_progress_jitter(argv[1], argv[2], &jitter)) return 2;
    uint64_t state = 42, again = 42, total = 0, max = 0;
    for (int i = 0; i < 100000; ++i) {
        uint32_t extra = seedbox_progress_extra_calls(&state, jitter);
        if (extra != seedbox_progress_extra_calls(&again, jitter)) return 3;
        total += extra;
        if (extra > max) max = extra;
    }
    printf("%llu %llu\n", (unsigned long long)total, (unsigned long long)max);
    return 0;
}
''')
            probe = root / 'jitter'
            subprocess.run(shlex.split(os.environ.get('CC', 'cc')) + [
                '-std=c11', '-Wall', '-Wextra', '-Werror', '-I', str(ROOT / 'guest'),
                str(source), '-o', str(probe)], check=True, capture_output=True)
            def run(value, option='--progress-jitter-ns'):
                return subprocess.run([str(probe), option, value],
                                      capture_output=True, text=True, timeout=10)
            for jitter in [1, 999, 1000, 2500, 1000000]:
                result = run(str(jitter))
                self.assertEqual(result.returncode, 0, jitter)
                total, most = map(int, result.stdout.split())
                # Extra calls of 1 us each average jitter / 2 per checkpoint.
                expected = 100000 * jitter / 2 / 1000
                self.assertLess(abs(total - expected), max(60, expected * 0.02), jitter)
                self.assertLessEqual(most, -(-jitter // 1000), jitter)
            for jitter in ['', '0', '1000001', '-1', '1us', '4294967296']:
                self.assertEqual(run(jitter).returncode, 2, jitter)
            self.assertEqual(run('1', '--blocks-per-progress-call').returncode, 2)
