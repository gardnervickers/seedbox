"""Exercise publication and tool wiring with cheap stand-ins for Nix and Cargo."""
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import os
import subprocess
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('complete_build', ROOT / 'tools/build.py')
workflow = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workflow)


class CompleteBuildTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        tools = self.root / 'tools'
        tools.mkdir()
        nix = tools / 'nix'
        nix.write_text('''#!/usr/bin/env python3
import pathlib,sys
root=pathlib.Path.cwd()
value=(root/'guest-source').read_text()
if value=='broken': sys.exit(7)
output=root/'.nix-fixture'/value
output.mkdir(parents=True,exist_ok=True)
if sys.argv[-1]=='.#guestKernel':
 (output/'bzImage').write_text('kernel:'+value)
 print(output)
else:
 (output/'runtime.tar.gz').write_text('helpers:'+value)
 print(output/'runtime.tar.gz')
''')
        cargo = tools / 'cargo'
        cargo.write_text('''#!/usr/bin/env python3
import json,os,pathlib,sys
cache=pathlib.Path(sys.argv[sys.argv.index('--target-dir')+1])
cache.mkdir(parents=True,exist_ok=True)
binary=cache/'seedbox'
binary.write_text('embedded:'+pathlib.Path(os.environ['SEEDBOX_BUILD_HELPERS']).read_text())
print(json.dumps({'reason':'compiler-artifact','target':{'name':'seedbox'},'executable':str(binary)}))
''')
        for tool in (nix, cargo): tool.chmod(0o755)
        self.env = patch.dict(os.environ, PATH=str(tools)+os.pathsep+os.environ['PATH'], SEEDBOX_BUILD_HELPERS='/unrelated/archive', SEEDBOX_ASSET_DIR='/unrelated/cache')
        self.env.start(); self.addCleanup(self.env.stop)
        self.system = patch.object(workflow.platform, 'system', return_value='Linux')
        self.system.start(); self.addCleanup(self.system.stop)
        self.machine = patch.object(workflow.platform, 'machine', return_value='x86_64')
        self.machine.start(); self.addCleanup(self.machine.stop)

    def checkout(self, name, value):
        root=self.root/name
        root.mkdir()
        (root/'guest-source').write_text(value)
        return root

    def assert_bundle(self, path, value):
        self.assertEqual((path/'kernel').read_text(), 'kernel:'+value)
        self.assertEqual((path/'runtime.tar.gz').read_text(), 'helpers:'+value)
        self.assertEqual((path/'seedbox').read_text(), 'embedded:helpers:'+value)

    def test_changed_guest_input_rebuilds_embedding_without_mutating_old_build(self):
        root=self.checkout('one','old')
        old=workflow.build(root)
        (root/'guest-source').write_text('new')
        new=workflow.build(root)
        self.assert_bundle(old,'old')
        self.assert_bundle(new,'new')
        self.assertEqual((root/'build/current').resolve(),new)

    def test_failed_build_and_assets_only_leave_complete_build_selected(self):
        root=self.checkout('one','good')
        old=workflow.build(root)
        (root/'guest-source').write_text('broken')
        with self.assertRaises(subprocess.CalledProcessError): workflow.build(root)
        self.assertEqual((root/'build/current').resolve(),old)
        self.assertFalse(list((root/'build').glob('.pending-*')))
        (root/'guest-source').write_text('assets')
        assets=workflow.build(root,assets_only=True)
        self.assertFalse((assets/'seedbox').exists())
        self.assertEqual((root/'build/current').resolve(),old)
        self.assert_bundle(old,'good')

    def test_concurrent_checkouts_and_same_checkout_have_isolated_completed_outputs(self):
        a=self.checkout('a','alpha'); b=self.checkout('b','beta')
        with ThreadPoolExecutor(max_workers=3) as workers:
            futures=[workers.submit(workflow.build,root) for root in (a,b,a)]
            results=[f.result(timeout=30) for f in futures]
        self.assertEqual(len(set(results)),3)
        for result,value in zip(results,('alpha','beta','alpha')): self.assert_bundle(result,value)
        self.assertIn((a/'build/current').resolve(),(results[0],results[2]))
        self.assertEqual((b/'build/current').resolve(),results[1])
