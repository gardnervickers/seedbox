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
    def test_strict_option_parser_accepts_u32_range_and_rejects_bad_options(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'budget.c'
            source.write_text(r"""#include "progress/budget.h"
#include <stdio.h>
int main(int argc, char **argv) {
    uint32_t budget = 0, points = 0;
    if (!seedbox_progress_arguments(argc, (const char **)argv, &budget, &points)) return 2;
    printf("%u %u\n", budget, points);
    return 0;
}
""")
            probe = root / 'budget'
            subprocess.run(shlex.split(os.environ.get('CC', 'cc')) + [
                '-std=c11', '-Wall', '-Wextra', '-Werror', '-I', str(ROOT / 'guest'),
                str(source), '-o', str(probe)], check=True, capture_output=True)
            def run(*args):
                return subprocess.run([str(probe), *args], capture_output=True, text=True,
                                      timeout=10)
            budget, points = '--instructions-per-progress-call', '--progress-points-per-block'
            for value in ['1', '17', '50000', '4294967295']:
                result = run(budget, value, points, value)
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stdout, f'{value} {value}\n')
            for value in ['', '0', '-1', '+1', ' 1', '1 ', '1x', '4294967296',
                           '999999999999999999999999999999']:
                self.assertEqual(run(budget, value, points, '1').returncode, 2, value)
                self.assertEqual(run(budget, '1', points, value).returncode, 2, value)
            for args in [[budget, '1'], [points, '1', budget, '1'],
                         [budget, '1', points, '1', '--other', '1'],
                         ['--blocks-per-progress-call', '1', points, '1'],
                         [budget + '-extra', '1', points, '1'], [budget[:-1], '1', points, '1']]:
                self.assertEqual(run(*args).returncode, 2, args)
