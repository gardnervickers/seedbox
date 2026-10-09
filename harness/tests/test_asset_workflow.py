"""Exercise the actual asset-planning script against force-push event shapes."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[2]


class AssetWorkflowTests(unittest.TestCase):
    def test_pr_compares_base_even_when_previous_head_is_unavailable(self):
        workflow = (ROOT / '.github/workflows/kernels.yml').read_text()
        script = textwrap.dedent(workflow.split("python3 - <<'PYTHON'\n", 1)[1].split('          PYTHON', 1)[0])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            environment = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull)
            def git(*args):
                return subprocess.check_output(['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@invalid', *args], cwd=root, env=environment, text=True).strip()
            git('init', '-q')
            for manifest, source in [('kernel/inputs.json', 'kernel/source'), ('tools/runtime/inputs.json', 'tools/runtime/source')]:
                path = root / manifest
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(json.dumps({'common': [source]}))
                (root / source).write_text('initial')
            git('add', '.')
            git('commit', '-qm', 'initial')
            base = git('rev-parse', 'HEAD')
            (root / 'tools/runtime/source').write_text('changed')
            git('commit', '-qam', 'runtime change')
            events = [
                ('pull_request', {'before': 'f' * 40, 'pull_request': {'base': {'sha': base}}}, False, True),
                ('push', {'before': base}, False, True),
                ('workflow_dispatch', {}, True, True),
            ]
            for name, event, kernel, runtime in events:
                with self.subTest(event=name):
                    event_path = root / 'event.json'
                    event_path.write_text(json.dumps(event))
                    output = root / 'output'
                    output.write_text('')
                    env = dict(environment, GITHUB_EVENT_PATH=str(event_path), GITHUB_EVENT_NAME=name, GITHUB_OUTPUT=str(output), ASSET_RUN_ID='123')
                    result = subprocess.run([sys.executable, '-c', script], cwd=root, env=env, text=True, capture_output=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn(f'kernel_changed={str(kernel).lower()}\n', output.read_text())
                    self.assertIn(f'runtime_changed={str(runtime).lower()}\n', output.read_text())
