"""Portable tests for every reviewer mapping row and the public planner."""
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

CORE = {'core', 'cluster-fixed'}


class MapTests(unittest.TestCase):
    def assert_paths(self, paths, extra=()):
        for path in paths:
            with self.subTest(path=path):
                plan = host_plan.select_checks({'revision': [path]})
                self.assertEqual(set(plan['checks']), CORE | set(extra))
                self.assertFalse(plan['warnings'])
                for reasons in plan['checks'].values():
                    self.assertTrue(all(reason['path'] == path for reason in reasons))

    def test_portable_row(self):
        paths = ['docs/runtime.md', 'README.md', 'kernel/README.md', 'crates/seedbox-vmm/README.md',
                 'crates/seedbox/src/cli/mod.rs', 'python/tests/test_control.py',
                 'harness/smoke/amd.py', 'crates/seedbox/tests/cli.rs']
        plan = host_plan.select_checks({'revision': paths},
            diffs={('revision','crates/seedbox/src/cli/mod.rs'): '-    /// Old help.\n+    /// New help.'})
        self.assertFalse(plan['checks'])
        self.assertEqual(len(plan['skipped']), len(paths))

    def test_execution_row(self):
        self.assert_paths(['crates/seedbox-vmm/src/rng.rs', 'guest/sbx.c',
                           *['crates/seedbox/src/' + name + '.rs' for name in
                             ('vmm', 'host', 'admission', 'orchestration')]])

    def test_stepping_row_including_counter_overlap(self):
        self.assert_paths(['crates/seedbox-vmm/src/' + path for path in
                           ('stepper.rs', 'machine/counted.rs', 'timebase.rs', 'boundary.rs',
                            'counter.rs', 'undo.rs', 'undo/restore.rs', 'checkpoint.rs')]
                          + ['crates/seedbox/src/counter.rs'], ['perf-replay'])

    def test_disk_row(self):
        self.assert_paths(['crates/seedbox-vmm/src/disk.rs', 'crates/seedbox-vmm/src/virtio_blk.rs',
                           'guest/disk.init', 'guest/build-disk.sh'], ['disk-crash', 'disk-ext4-crash'])

    def test_network_row(self):
        self.assert_paths(['crates/seedbox-vmm/src/' + path for path in
                           ('network.rs', 'network/routing.rs', 'virtio_net.rs', 'link.rs', 'faults.rs')],
                          ['net', 'net-pause', 'net-crash'])

    def test_control_row_overrides_portable_python(self):
        self.assert_paths(['crates/seedbox-vmm/src/control.rs', 'crates/seedbox-control/src/lib.rs',
                           'crates/seedbox/src/host_control.rs', 'python/seedbox/control.py'], ['control-cases'])

    def test_preparation_images_row(self):
        self.assert_paths(['guest/compose-init.c', 'crates/seedbox/src/preparation.rs',
                           'crates/seedbox/src/preparation_command.rs', 'crates/seedbox/src/image.rs',
                           'crates/seedbox/src/oci.rs'], ['sanity'])

    def test_volumes_row(self):
        self.assert_paths(['crates/seedbox/src/volumes.rs', 'crates/seedbox/src/volumes/mount.rs'],
                          ['sanity', 'volume-crash'])

    def test_kernel_row_is_explicitly_uncovered(self):
        plan = host_plan.select_checks({'revision': ['kernel/config.options']})
        self.assertFalse(plan['checks'])
        self.assertEqual(plan['warnings'], ['kernel campaign needed: kernel/config.options'])

    def test_unmatched_crates_row_fails_safe(self):
        self.assert_paths(['crates/new/src/unmapped.rs', 'crates/seedbox/src/diff.rs'])

    def test_mixed_rows_union_all_reasons_and_reviewer_additions(self):
        paths = ['README.md', 'guest/compose-init.c', 'crates/seedbox-vmm/src/disk.rs',
                 'crates/seedbox-vmm/src/timebase.rs', 'python/seedbox/control.py']
        plan = host_plan.select_checks({'revision': paths}, extra=['net', 'net', 'net-crash'])
        self.assertEqual(set(plan['checks']), CORE | {'sanity', 'disk-crash', 'disk-ext4-crash',
                                                    'perf-replay', 'control-cases', 'net', 'net-crash'})
        self.assertEqual({r['path'] for r in plan['checks']['core']}, set(paths) - {'README.md'})
        self.assertEqual(len(plan['checks']['net']), 1)
        self.assertEqual(plan['checks']['net'][0]['path'], '--extra net')
        added = host_plan.select_checks({'rev': ['README.md']}, extra=['disk-crash'])
        self.assertEqual(set(added['checks']), {'disk-crash'})
        with self.assertRaisesRegex(ValueError, 'unknown'):
            host_plan.select_checks({}, extra=['invented'])

    def test_shared_code_adds_only_affected_controls(self):
        for path, code, extra in [
            ('crates/seedbox-vmm/src/machine.rs', '+self.undo = checkpoint;', {'perf-replay'}),
            ('crates/seedbox-vmm/src/machine.rs', '+self.blk = disk;', {'disk-crash','disk-ext4-crash'}),
            ('crates/seedbox-vmm/src/machine.rs', '+self.net = link;', {'net','net-pause','net-crash'}),
            ('guest/compose-init.c', '+list(volumes);', {'sanity','volume-crash'}),
            ('guest/compose-init.c', '+list(arguments);', {'sanity'})]:
            with self.subTest(path=path,code=code):
                plan = host_plan.select_checks({'rev': [path]}, diffs={('rev',path): code})
                self.assertEqual(set(plan['checks']), CORE | extra)
        plan = host_plan.select_checks({'rev': ['crates/seedbox/src/cli/mod.rs']},
                                       diffs={('rev','crates/seedbox/src/cli/mod.rs'): '+execute();'})
        self.assertEqual(set(plan['checks']), CORE)

    def test_help_strings_are_portable_but_assignments_are_not(self):
        self.assertTrue(host_plan.help_only('+#[command(about = "Help text.")]'))
        self.assertTrue(host_plan.help_only('-help = "Old help.",\n+help = "New help.",'))
        self.assertFalse(host_plan.help_only('+help = execute();'))

    def test_extra_performance_has_its_baseline(self):
        plan = host_plan.select_checks({}, extra=['perf-replay'])
        self.assertIn('core', plan['checks'])
        self.assertIn('perf-replay', plan['checks'])

    def test_bad_map_refuses(self):
        mapping = host_plan.load_map()
        mapping['rules'][0]['checks'] = ['invented']
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'map.json'
            path.write_text(json.dumps(mapping))
            with self.assertRaisesRegex(ValueError, 'unknown'):
                host_plan.load_map(path)


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
        self.git('update-ref', 'refs/remotes/origin/main', 'HEAD')

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.repo), *args], stderr=subprocess.PIPE).decode().strip()

    def commit(self, path, text='content'):
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text)
        self.git('add', '--', path)
        self.git('commit', '-qm', path)
        return self.git('rev-parse', 'HEAD')

    def invoke(self, script, *args):
        return subprocess.run([sys.executable, str(HERE / script), *args], cwd=self.repo,
                              capture_output=True, text=True)

    def test_three_dot_diff_excludes_base_only_changes(self):
        self.git('checkout', '-qb', 'topic')
        self.commit('crates/seedbox-vmm/src/disk.rs')
        self.git('checkout', '-q', 'base')
        self.commit('guest/compose-init.c')
        self.git('checkout', '-q', 'topic')
        plan = host_plan.plan_revision(self.repo, 'base', 'HEAD')
        self.assertIn('disk-crash', plan['checks'])
        self.assertNotIn('sanity', plan['checks'])
        expected = self.git('diff', '--name-only', 'base...HEAD').splitlines()
        self.assertEqual(plan['changes'][plan['target']], expected)

    def test_renames_keep_old_runtime_path_coverage(self):
        source = 'crates/seedbox-vmm/src/disk.rs'
        self.commit(source)
        self.git('branch','-f','base')
        self.git('mv',source,'retired.md')
        self.git('commit','-qm','rename')
        plan = host_plan.plan_revision(self.repo,'base','HEAD')
        self.assertEqual(set(plan['checks']),CORE | {'disk-crash','disk-ext4-crash'})
        self.assertIn(source,plan['changes'][plan['target']])

    def test_actual_cli_help_and_dispatch_diffs(self):
        path = 'crates/seedbox/src/cli/mod.rs'
        self.commit(path, '/// Old help.\nfn run() {}\n')
        self.git('branch', '-f', 'base')
        self.commit(path, '/// New help.\nfn run() {}\n')
        self.assertFalse(host_plan.plan_revision(self.repo, 'base', 'HEAD')['checks'])
        self.commit(path, '/// New help.\nfn run() { execute(); }\n')
        self.assertEqual(set(host_plan.plan_revision(self.repo, 'base', 'HEAD')['checks']), CORE)

    def test_public_planner_none_json_and_repeatable_extras(self):
        none = self.invoke('host-plan.py', 'base', 'HEAD')
        self.assertEqual(none.returncode, 0, none.stderr)
        self.assertIn('host: none', none.stdout)
        extra = self.invoke('host-plan.py', 'base', 'HEAD', '--json', '--extra', 'net', '--extra', 'disk-crash')
        self.assertEqual(extra.returncode, 0, extra.stderr)
        self.assertEqual(set(json.loads(extra.stdout)['checks']), {'net', 'disk-crash'})
        self.assertEqual(self.invoke('host-plan.py', 'base', 'HEAD', '--extra', 'bad').returncode, 2)

    def test_host_check_plan_and_no_selection_do_no_host_work(self):
        self.commit('guest/compose-init.c')
        completed = subprocess.run(['bash', str(HERE / 'host-check.sh'), '--plan', '--lanes', '1,2,3',
                                    '--base', 'base', 'HEAD', '--extra', 'net'],
                                   cwd=self.repo, capture_output=True, text=True)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn('check core:', completed.stdout)
        self.assertIn('guest/compose-init.c', completed.stdout)
        self.assertIn('--extra net', completed.stdout)
        self.assertNotIn('perf-replay', completed.stdout)
        with patch.object(host_check, 'execute', side_effect=AssertionError('host work')), \
             patch.object(host_check, 'read_pairs', side_effect=AssertionError('topology probe')), \
             patch.object(host_check, 'plan_revision', return_value=host_plan.plan_revision(self.repo, 'base', 'HEAD')), \
             contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(host_check.main(['--plan']), 0)
        none = self.invoke('host_check.py', '--base', 'HEAD')
        self.assertEqual(none.returncode, 0, none.stderr)
        self.assertIn('host: none', none.stdout)

    def test_kernel_warning_is_plan_success_but_not_execution_success(self):
        self.commit('kernel/config.options')
        planned = self.invoke('host_check.py', '--plan')
        self.assertEqual(planned.returncode, 0)
        self.assertIn('kernel campaign needed', planned.stdout)
        self.assertEqual(self.invoke('host_check.py').returncode, 2)

    def test_invalid_revision_is_error(self):
        self.assertEqual(self.invoke('host-plan.py', 'base', 'missing').returncode, 2)


if __name__ == '__main__':
    unittest.main()
