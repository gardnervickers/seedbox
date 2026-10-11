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
    def test_strict_option_parser_accepts_its_ranges_and_rejects_bad_options(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'budget.c'
            source.write_text(r"""#include "progress/budget.h"
#include <stdio.h>
int main(int argc, char **argv) {
    uint32_t budget = 0, points = 0, jitter = 0;
    if (!seedbox_progress_arguments(argc, (const char **)argv, &budget, &points, &jitter))
        return 2;
    printf("%u %u %u\n", budget, points, jitter);
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
            budget, points, jitter = ('--instructions-per-progress-call',
                                      '--progress-points-per-block', '--progress-jitter-ns')
            def options(b='1', p='1', j='0'):
                return [budget, b, points, p, jitter, j]
            for value in ['1', '17', '50000', '4294967295']:
                result = run(*options(value, value))
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stdout, f'{value} {value} 0\n')
            for value in ['0', '1', '2500', '1000000']:
                result = run(*options(j=value))
                self.assertEqual(result.returncode, 0, value)
                self.assertEqual(result.stdout, f'1 1 {value}\n')
            malformed = ['', '-1', '+1', ' 1', '1 ', '1x', '4294967296',
                         '999999999999999999999999999999']
            for value in malformed + ['0']:
                self.assertEqual(run(*options(b=value)).returncode, 2, value)
                self.assertEqual(run(*options(p=value)).returncode, 2, value)
            for value in malformed + ['1000001', '1us']:
                self.assertEqual(run(*options(j=value)).returncode, 2, value)
            for args in [options()[:4], options()[2:4] + options()[:2] + options()[4:],
                         options() + ['--other', '1'],
                         ['--blocks-per-progress-call', '1'] + options()[2:],
                         [budget + '-extra'] + options()[1:], [budget[:-1]] + options()[1:]]:
                self.assertEqual(run(*args).returncode, 2, args)
