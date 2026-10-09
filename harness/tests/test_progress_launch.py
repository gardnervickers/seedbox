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
