"""Portable map/planner controls using disposable Git repositories."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HERE))
import host_check
import host_plan


class MapTests(unittest.TestCase):
    def select(self, *paths):
        return host_plan.select_checks({'revision': paths})

    def test_engine_core_and_scoped_extensions(self):
        mapping = host_plan.load_map()
        core = set(mapping['core'])
        categories = {
            'crates/seedbox-vmm/src/rng.rs': set(),
            'crates/seedbox-vmm/src/undo.rs': {'undo-margin'},
            'crates/seedbox-vmm/src/counter.rs': {'undo-margin'},
            'crates/seedbox-vmm/src/stepper.rs': {'undo-margin'},
            'crates/seedbox-vmm/src/disk.rs': {'disk-nocrash', 'disk-crash', 'disk-crash-fork', 'disk-ext4-crash'},
            'crates/seedbox-vmm/src/virtio_net.rs': {'net', 'net-pause', 'net-crash', 'net-crash-fork'},
            'guest/compose-init.c': {'compose-redis', 'compose-etcd', 'compose-race'},
            'kernel/seedbox.c': {'pv-abi1', 'pv-abi2'},
            'crates/seedbox-vmm/src/timebase.rs': {'pv-abi1', 'pv-abi2'},
        }
        for path, extra in categories.items():
            with self.subTest(path=path):
                plan = self.select(path)
                self.assertEqual(set(plan['checks']), core | extra)
                self.assertFalse(plan['warnings'])
                for reasons in plan['checks'].values():
                    self.assertTrue(all(reason['path'] == path for reason in reasons))

    def test_docs_cli_text_harness_and_portable_need_no_host(self):
        plan = self.select('docs/runtime.md', 'README.md', 'harness/smoke/amd.py',
                           'harness/worlds/disk-crash.json', 'tools/mac-tcg/runner.py',
                           'crates/seedbox/src/cli/mod.rs', 'crates/seedbox/tests/cli.rs',
                           'crates/seedbox/src/diff.rs')
        self.assertFalse(plan['checks'])
        self.assertFalse(plan['warnings'])
        self.assertEqual(len(plan['skipped']), 8)

    def test_unknown_engine_fails_safe_even_for_deleted_file(self):
        for path in ('crates/seedbox-vmm/src/future/nested.rs', 'kernel/new-feature.c', 'guest/new.init'):
            plan = self.select(path)
            self.assertEqual(set(plan['checks']), set(host_plan.load_map()['core']))
            self.assertIn(path, plan['warnings'][0])
            self.assertIn('unmapped engine', plan['warnings'][0])

    def test_globs_are_path_components_and_roots_cannot_be_shadowed(self):
        self.assertTrue(host_plan.matches('README.md', '/*.md'))
        self.assertFalse(host_plan.matches('guest/new.md', '/*.md'))
        self.assertFalse(host_plan.matches('notguest/compose-init.c', 'guest/*'))
        self.assertTrue(host_plan.matches('harness/tests/test_host_plan.py', 'harness/**'))
        self.assertFalse(host_plan.matches('guest/deep/net.init', 'guest/net*'))

    def test_all_current_engine_paths_mapped(self):
        repo = HERE.parent
        files = subprocess.check_output(['git', '-C', str(repo), 'ls-files', 'crates/seedbox-vmm', 'kernel', 'guest'], text=True).splitlines()
        plan = host_plan.select_checks({'tree': files})
        self.assertEqual(plan['warnings'], [])

    def test_bad_map_refuses(self):
        mapping = host_plan.load_map()
        mapping['rules'][0]['checks'] = ['invented']
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'map.json'
            path.write_text(json.dumps(mapping))
            with self.assertRaisesRegex(ValueError, 'unknown'):
                host_plan.load_map(path)

    def test_shared_machine_hunks_add_only_the_affected_category(self):
        path = 'crates/seedbox-vmm/src/machine.rs'
        core = set(host_plan.load_map()['core'])
        for code, extra in [('+self.undo = checkpoint;', {'undo-margin'}),
                            ('+self.pv = true;', {'pv-abi1', 'pv-abi2'}),
                            ('+self.blk = disk;', {'disk-nocrash', 'disk-crash', 'disk-crash-fork', 'disk-ext4-crash'}),
                            ('+self.net = virtio_net;', {'net', 'net-pause', 'net-crash', 'net-crash-fork'}),
                            ('+self.name = name;', set())]:
            plan = host_plan.select_checks({'revision': [path]}, diffs={('revision', path): code})
            self.assertEqual(set(plan['checks']), core | extra)

    def test_shared_hunk_filters_context_and_file_headers(self):
        diff = '--- disk.rs\n+++ net.rs\n@@ -1 +1 @@ fn checkpoint()\n timebase context\n-self.undo = old;\n+self.undo = new;'
        code = host_plan.changed_code(diff)
        self.assertNotIn('disk.rs', code)
        self.assertNotIn('net.rs', code)
        self.assertNotIn('timebase', code)
        self.assertIn('checkpoint()', code)
        self.assertIn('self.undo', code)


class GitPlannerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git('init', '-q')
        self.git('config', 'user.name', 'Portable Test')
        self.git('config', 'user.email', 'portable@example.invalid')
        self.commit('README.md')
        self.git('branch', 'base')

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.repo), *args], stderr=subprocess.PIPE).decode().strip()

    def commit(self, path, text='content'):
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        self.git('add', '--', path)
        self.git('commit', '-qm', path)
        return self.git('rev-parse', 'HEAD')

    def test_list_unions_merge_base_diffs_and_executes_last_tip(self):
        disk = self.commit('crates/seedbox-vmm/src/disk.rs')
        self.git('checkout', '-qb', 'train', 'base')
        net = self.commit('crates/seedbox-vmm/src/virtio_net.rs')
        plan = host_plan.plan_revisions(self.repo, [disk, net], 'base')
        self.assertIn('disk-crash', plan['checks'])
        self.assertIn('net-crash', plan['checks'])
        self.assertNotIn('undo-margin', plan['checks'])
        self.assertEqual(plan['target'], net)
        self.assertEqual(set(plan['merge_bases'].values()), {self.git('rev-parse', 'base')})
        train = host_plan.plan_revisions(self.repo, ['train'], 'base')
        self.assertNotIn('disk-crash', train['checks'])
        self.assertIn('net-crash', train['checks'])

    def test_merge_base_not_base_tip_and_rename_selects_both_paths(self):
        first = self.commit('crates/seedbox-vmm/src/disk.rs')
        self.git('branch', 'topic')
        self.git('checkout', '-q', 'base')
        self.commit('guest/compose-init.c')
        self.git('checkout', '-q', 'topic')
        dest = self.repo / 'crates/seedbox-vmm/src/virtio_net.rs'
        (self.repo / 'crates/seedbox-vmm/src/disk.rs').rename(dest)
        self.git('add', '-A')
        self.git('commit', '-qm', 'rename disk to net')
        plan = host_plan.plan_revisions(self.repo, [first, 'topic'], 'base')
        self.assertIn('disk-crash', plan['checks'])
        self.assertIn('net', plan['checks'])
        self.assertNotIn('compose-etcd', plan['checks'])

    def test_plan_runs_no_host_code_and_needs_no_inputs(self):
        self.commit('guest/compose-init.c')
        env = {key: value for key, value in os.environ.items() if not key.startswith(('SEEDBOX_', 'PINNED_ENGINE'))}
        completed = subprocess.run(['bash', str(HERE / 'host-check.sh'), '--plan', '--pairs', '3', '--base', 'base', '--train', 'HEAD'],
                                   cwd=self.repo, env=env, capture_output=True, text=True)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn('check fixed:', completed.stdout)
        self.assertIn('guest/compose-init.c', completed.stdout)
        self.assertNotIn('undo-margin', completed.stdout)
        self.assertFalse(list(self.repo.glob('host-check-*')))
        # Same-process guard ensures the dry-run boundary is exercised with host calls forbidden.
        with patch.object(host_check, 'execute', side_effect=AssertionError('host work')), \
             patch.object(host_check, 'read_pairs', side_effect=AssertionError('topology probe')), \
             patch.object(host_check, 'plan_revisions', return_value=host_plan.plan_revisions(self.repo, ['HEAD'], 'base')), \
             contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(host_check.main(['--plan']), 0)

    def test_invalid_revision_is_error(self):
        with self.assertRaises(subprocess.CalledProcessError):
            host_plan.plan_revisions(self.repo, ['missing'], 'base')

    def test_git_shared_file_diff_does_not_select_unrelated_categories(self):
        self.commit('crates/seedbox-vmm/src/machine.rs', 'fn checkpoint() {\n    self.undo = old;\n}\n')
        self.git('branch', '-f', 'base')
        self.commit('crates/seedbox-vmm/src/machine.rs', 'fn checkpoint() {\n    self.undo = new;\n}\n')
        plan = host_plan.plan_revisions(self.repo, ['HEAD'], 'base')
        self.assertEqual(set(plan['checks']), set(host_plan.load_map()['core']) | {'undo-margin'})


if __name__ == '__main__':
    unittest.main()
