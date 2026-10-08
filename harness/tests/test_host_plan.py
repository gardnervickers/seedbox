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
        # Exercise each row with an engine difference; test the cluster guard separately.
        for path in paths:
            with self.subTest(path=path):
                plan = host_plan.select_checks({'revision': [path]}, engine_changed=True)
                self.assertEqual(set(plan['checks']), CORE | set(extra))
                self.assertFalse(plan['warnings'])
                for reasons in plan['checks'].values():
                    self.assertTrue(all(reason['path'] == path for reason in reasons))

    def test_portable_row(self):
        paths = ['docs/runtime.md', 'README.md', 'kernel/README.md', 'crates/seedbox-vmm/README.md',
                 'crates/seedbox/src/cli/mod.rs', 'python/tests/test_control.py',
                 'harness/tests/test_smoke.py', 'crates/seedbox/tests/cli.rs',
                 'crates/seedbox-vmm/tests/control.rs', 'crates/new/tests/volume.rs',
                 'templates/example.yaml', 'crates/seedbox/templates/compose.yaml',
                 'tools/mac-tcg/run.py', 'docs/generated.json']
        plan = host_plan.select_checks({'revision': paths},
            diffs={('revision','crates/seedbox/src/cli/mod.rs'): '-    /// Old help.\n+    /// New help.'})
        self.assertFalse(plan['checks'])
        self.assertEqual(len(plan['skipped']), len(paths))

    def test_core_inputs_and_unknown_paths_fail_closed(self):
        self.assert_paths(['examples/faults/workload/main.py', 'examples/faults/generate.py',
                           'harness/smoke/amd.py', 'harness/host-check.sh', 'harness/host_plan.py',
                           'python/seedbox/rng.py', 'flake.nix', 'flake.lock', 'Cargo.lock',
                           'rust-toolchain', 'rust-toolchain.toml', '.cargo/config.toml',
                           '.cargo/new-config', 'scripts/new.py', 'harness/new.sh',
                           'crates/seedbox/src/cli/help.rs', 'unknown/input.json'])

    def test_new_rows_retain_their_path_reasons(self):
        for path, category in (
                ('examples/faults/workload/scenario.py', 'core inputs'),
                ('harness/smoke/amd.py', 'core inputs'),
                ('harness/host_check.py', 'core inputs'),
                ('python/seedbox/control.py', 'core inputs'),
                ('flake.nix', 'toolchain'), ('flake.lock', 'toolchain'),
                ('rust-toolchain.toml', 'toolchain'), ('Cargo.lock', 'toolchain'),
                ('.cargo/config.toml', 'toolchain'),
                ('harness/vmm-rustc.py', 'toolchain'),
                ('crates/seedbox-control/Cargo.toml', 'toolchain'),
                ('harness/batch.sh', 'batch runner')):
            with self.subTest(path=path):
                plan = host_plan.select_checks({'rev': [path]})
                reasons = [reason for values in plan['checks'].values() for reason in values]
                self.assertIn(dict(revision='rev', path=path, category=category), reasons)

    def test_world_and_batch_rows(self):
        mapping = host_plan.load_map()
        for name, spec in mapping['checks'].items():
            if spec['kind'] == 'fixture':
                path = 'harness/worlds/' + spec['world'] + '.json'
                plan = host_plan.select_checks({'rev': [path]}, engine_changed=True)
                self.assertIn(name, plan['checks'], path)
                self.assertIn('world fixture', {r['category'] for r in plan['checks'][name]})
        self.assert_paths(['harness/worlds/new-fixture.json'])
        for path in ('harness/batch.sh', 'harness/batch.py'):
            plan = host_plan.select_checks({'rev': [path]})
            self.assertEqual(set(plan['checks']), CORE)

    def test_batch_and_fixture_rows_keep_cluster_fixed(self):
        for path in ('harness/batch.sh', 'harness/worlds/cluster-fixed.json'):
            plan = host_plan.select_checks({'r': [path]})
            self.assertIn('cluster-fixed', plan['checks'], path)

    def test_cluster_workload_keeps_fixture_and_reviewer_note(self):
        for path in ('examples/etcd/workload/main.go', 'examples/etcd/workload/go.mod',
                     'examples/etcd/workload/go.sum'):
            with self.subTest(path=path):
                plan = host_plan.select_checks({'r': [path]}, engine_changed=False)
                self.assertEqual(set(plan['checks']), CORE)
                self.assertIn(dict(revision='r', path=path, category='cluster workload'),
                              plan['checks']['cluster-fixed'])
                self.assertEqual(plan['reviewer_notes'],
                                 ["example etcd: its reviewer's run list is required"])

    def test_other_example_requires_its_reviewer_run_list(self):
        plan = host_plan.select_checks({'rev': ['examples/minio/client/cas.py',
                                               'examples/minio/compose.yaml',
                                               'examples/redis/workload/main.py']})
        self.assertEqual(set(plan['checks']), {'core'})
        text = host_plan.format_plan(dict(plan, target='a' * 40, base='b' * 40))
        for name in ('minio', 'redis'):
            note = f"example {name}: its reviewer's run list is required"
            self.assertEqual(text.count(note), 1)

    def test_actual_pr_changed_file_lists(self):
        # Captured with gh pr view NUMBER --json files -q '.files[].path'.
        for number, category in ((110, 'core inputs'), (104, 'example')):
            paths = (HERE / 'tests/fixtures' / f'pr{number}-paths.txt').read_text().splitlines()
            plan = host_plan.select_checks({f'pr{number}': paths})
            self.assertEqual(set(plan['checks']), {'core'})
            self.assertIn(category, {r['category'] for r in plan['checks']['core']})
            text = host_plan.format_plan(dict(plan, target='a' * 40, base='b' * 40))
            self.assertNotIn('host: none', text)
            if number == 110:
                for path in ('examples/faults/workload/main.py', 'harness/smoke/amd.py'):
                    self.assertIn(path, text)
            else:
                self.assertIn("example minio: its reviewer's run list is required", text)

    def test_moved_core_policies_keep_their_host_checks_and_reviewer_runs(self):
        for name in ['rng', 'disk', 'links', 'connections', 'triggers', 'faults', 'control', 'network_counters']:
            with self.subTest(module=name):
                original = host_plan.select_checks({'rev': ['crates/seedbox-vmm/src/' + name + '.rs']})
                moved = host_plan.select_checks({'rev': ['crates/seedbox-engine-core/src/' + name + '.rs']})
                self.assertEqual(set(moved['checks']), set(original['checks']))
                self.assertEqual(set(moved['runs']), set(original['runs']))
                self.assertIn('cluster-fixed', moved['checks'])

    def test_execution_row(self):
        self.assert_paths(['crates/seedbox-vmm/src/rng.rs', 'guest/sbx.c',
                           *['crates/seedbox/src/' + name + '.rs' for name in
                             ('vmm', 'host', 'admission', 'orchestration')]])

    def test_stepping_row_including_counter_overlap(self):
        self.assert_paths(['crates/seedbox-vmm/src/' + path for path in
                           ('stepper.rs', 'machine/counted.rs', 'timebase.rs', 'boundary.rs',
                            'counter.rs', 'undo.rs', 'undo/restore.rs', 'checkpoint.rs')]
                          + ['crates/seedbox/src/counter.rs'], ['perf-replay'])

    def test_disk_worlds_name_a_virtio_blk_kernel(self):
        plan = host_plan.select_checks({'r': ['crates/seedbox-vmm/src/disk.rs']})
        mapping = host_plan.load_map()
        for check in ('disk-crash', 'disk-ext4-crash'):
            self.assertIn(check, plan['checks'])
            self.assertEqual(mapping['checks'][check].get('kernel'), 'disk')

    def test_volume_changes_run_the_compose_volume_workload(self):
        plan = host_plan.select_checks({'r': ['harness/acceptance/driver/volume.py']})
        spec = host_plan.load_map()['checks']['volume-crash']
        self.assertIn('volume-crash', plan['checks'])
        self.assertEqual((spec['kind'], spec.get('project')),
                         ('compose', 'harness/acceptance/workloads/volume-crash.yaml'))
        self.assertEqual(spec['kernel'], 'disk')
        self.assertEqual(spec['expected'], 'PASS')
        workload = host_plan.select_checks({'r': [spec['project']]})
        self.assertIn('volume-crash', workload['checks'])

    def test_cluster_needs_engine_inputs_but_reviewer_can_force_it(self):
        for path in ('crates/seedbox/src/cli/mod.rs', 'harness/smoke/amd.py',
                     'examples/minio/compose.yaml', 'unmapped/input'):
            with self.subTest(path=path):
                plan = host_plan.select_checks({'r': [path]})
                self.assertEqual(set(plan['checks']), {'core'})
        for path in ('crates/seedbox-vmm/Cargo.lock', 'guest/compose-init.c', 'flake.lock',
                     'harness/vmm-rustc.py', 'crates/seedbox-control/Cargo.toml',
                     'crates/seedbox-control/src/lib.rs'):
            with self.subTest(path=path):
                self.assertIn('cluster-fixed', host_plan.select_checks({'r': [path]})['checks'])
        forced = host_plan.select_checks({}, extra=['cluster-fixed'], engine_changed=False)
        self.assertEqual(set(forced['checks']), {'cluster-fixed'})

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
        self.assertEqual(set(plan['checks']), {'core'})
        self.assertEqual(plan['warnings'], ['kernel campaign needed: kernel/config.options'])
        text = host_plan.format_plan(dict(plan, target='a' * 40, base='b' * 40))
        self.assertNotIn('host: none', text)

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
        self.assertEqual(set(plan['checks']), {'core'})

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

    def test_links_plan_exact_replay_and_feature_off_runs(self):
        for path in ('crates/seedbox-vmm/src/links.rs',
                     'crates/seedbox-vmm/src/connections.rs'):
            with self.subTest(path=path):
                plan = host_plan.select_checks({'rev': [path]})
                existing = CORE | ({'net', 'net-pause', 'net-crash'}
                                   if path.endswith('/links.rs') else set())
                self.assertEqual(set(plan['checks']), existing)
                equal_engine = host_plan.select_checks({'rev': [path]}, engine_changed=False)
                self.assertEqual(set(equal_engine['checks']), existing - {'cluster-fixed'})
                self.assertEqual(set(equal_engine['runs']), {'L1', 'L2', 'P1', 'M1'})
                self.assertEqual(set(plan['runs']), {'L1', 'L2', 'P1', 'M1'})
                for name, run in plan['runs'].items():
                    self.assertEqual(run['world'], 'harness/worlds/' +
                                     ('shaped' if name.startswith('L') else 'plain') + '.json')
                    self.assertEqual(run['engine'], 'main' if name == 'M1' else 'target')
                    self.assertEqual(run['seed'], 1)
                    self.assertEqual(run['fixture_pr'], 97)
                    self.assertEqual(run['reasons'],
                                     [dict(revision='rev', path=path, category='links')])
                    self.assertIn('L1 == L2' if name.startswith('L') else 'P1 == M1',
                                  run['comparison'])
                text = host_plan.format_plan(dict(plan, target='a' * 40, base='b' * 40))
                self.assertIn('reviewer host run required', text)
                self.assertIn('run M1: harness/worlds/plain.json engine=main', text)
                self.assertNotIn('U1', text)
                self.assertNotIn('run-links.sh', text)

    def test_short_undo_pair_only_comes_from_shared_undo_rule(self):
        mapping = host_plan.load_map()
        owners = [rule['category'] for rule in mapping['rules']
                  if {'U1', 'U2'} & set(rule.get('runs', []))]
        self.assertEqual(owners, ['undo-checkpoint-shared'])
        shared = next(rule for rule in mapping['rules'] if rule['category'] == owners[0])
        for path in shared['paths']:
            for code in ('+checkpoint = undo;', '-rollback(margin);', '+counter.stepper();'):
                with self.subTest(path=path, code=code):
                    plan = host_plan.select_checks({'rev': [path]}, diffs={('rev', path): code})
                    self.assertEqual(set(plan['runs']), {'U1', 'U2'})
                    self.assertIn('perf-replay', plan['checks'])
                    for run in plan['runs'].values():
                        self.assertEqual(run['world'], 'harness/worlds/shaped-tol-short.json')
                        self.assertEqual(run['timeout_seconds'], 600)
                        self.assertEqual(run['fixture_pr'], 97)
                        self.assertEqual(run['comparison'], 'U1 == U2 (short undo invariance)')
                    self.assertEqual(plan['runs']['U1']['undo'], 256)
                    self.assertEqual(plan['runs']['U2']['undo'], 0)
            for code in ('', '+self.net = link;', '+checkpoint_count += 1;'):
                self.assertFalse(host_plan.select_checks({'rev': [path]},
                                 diffs={('rev', path): code})['runs'])
        for path in ('crates/seedbox-vmm/src/stepper.rs',
                     'crates/seedbox-vmm/src/undo.rs', 'crates/seedbox-vmm/src/network.rs'):
            self.assertFalse(host_plan.select_checks({'rev': [path]})['runs'])
        self.assertFalse(host_plan.select_checks({}, extra=['perf-replay'])['runs'])

    def test_every_planned_performance_entry_is_short_and_bounded(self):
        mapping = host_plan.load_map()
        # Select every rule, including diff-gated rules, and every reviewer extra.
        paths = [path.replace('**', 'src').replace('*', 'test')
                 for rule in mapping['rules'] for path in rule['paths']]
        code = '+undo checkpoint rollback margin stepper counter disk net volume'
        plan = host_plan.select_checks({'rev': paths}, extra=list(mapping['checks']),
                                      diffs={('rev', path): code for path in paths})
        entries = {name: mapping['checks'][name] for name in plan['checks']} | plan['runs']
        self.assertEqual(set(plan['runs']), set(mapping['runs']))
        performance = {name: spec for name, spec in entries.items()
                       if spec.get('undo') == 0 or 'margin' in spec}
        self.assertEqual(set(performance), {'perf-replay', 'U1', 'U2'})
        for name, spec in performance.items():
            with self.subTest(entry=name):
                self.assertIn(spec['world'], ('examples/faults/compose.yaml',
                                             'harness/worlds/shaped-tol-short.json'))
                self.assertGreater(spec['timeout_seconds'], 0)
                self.assertLessEqual(spec['timeout_seconds'], 600)
        self.assertNotIn('shaped-tol.json', json.dumps(entries))
        self.assertNotIn('run-links.sh', json.dumps(entries))

    def test_required_runs_union_and_unknown_run_fails_closed(self):
        path = 'crates/seedbox-vmm/src/machine.rs'
        plan = host_plan.select_checks({'rev': [path, 'crates/seedbox-vmm/src/links.rs']},
                                      diffs={('rev', path): '+undo = checkpoint;'})
        self.assertEqual(set(plan['runs']), {'L1', 'L2', 'P1', 'M1', 'U1', 'U2'})
        mapping = host_plan.load_map()
        mapping['rules'][0]['runs'] = ['invented']
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'map.json'
            path.write_text(json.dumps(mapping))
            with self.assertRaisesRegex(ValueError, 'unknown run'):
                host_plan.load_map(path)


class GitPlannerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git('init', '-q')
        self.git('config', 'user.name', 'Portable Test')
        self.git('config', 'user.email', 'portable@example.invalid')
        # Temporary fixtures must never invoke the caller's signing service.
        self.git('config', 'commit.gpgsign', 'false')
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

    def test_cluster_compares_exact_engine_inputs_with_main(self):
        path = 'crates/seedbox-vmm/src/main.rs'
        self.commit(path, 'old engine')
        self.git('branch', '-f', 'base')
        main = self.commit(path, 'new engine')
        self.git('update-ref', 'refs/remotes/origin/main', main)
        self.commit('crates/seedbox/src/cli/mod.rs', 'fn run() {}')
        plan = host_plan.plan_revision(self.repo, 'base', 'HEAD')
        self.assertIn(path, plan['changes'][plan['target']])
        self.assertFalse(plan['engine_changed'])
        self.assertNotIn('cluster-fixed', plan['checks'])
        self.assertIn('core', plan['checks'])
        self.assertEqual(plan['main'], main)
        self.commit(path, 'different engine')
        self.assertIn('cluster-fixed', host_plan.plan_revision(self.repo, 'HEAD^', 'HEAD')['checks'])
        # Engine difference from main survives even a CLI-only merge-base diff.
        self.git('branch', '-f', 'base')
        self.commit('crates/seedbox/src/cli/mod.rs', 'fn run() { execute(); }')
        self.assertIn('cluster-fixed', host_plan.plan_revision(self.repo, 'base', 'HEAD')['checks'])
        self.commit(path, 'new engine')
        self.assertNotIn('cluster-fixed', host_plan.plan_revision(self.repo, 'base', 'HEAD')['checks'])
        self.assertIn('cluster-fixed', host_plan.plan_revision(self.repo, 'base', 'HEAD',
                                                             extra=['cluster-fixed'])['checks'])

    def test_fixture_inputs_select_cluster_with_identical_engine_trees(self):
        for path in ('harness/batch.sh', 'harness/batch.py',
                     'harness/worlds/cluster-fixed.json', 'examples/etcd/workload/main.go'):
            with self.subTest(path=path):
                self.git('branch', '-f', 'base')
                self.commit(path)
                plan = host_plan.plan_revision(self.repo, 'base', 'HEAD')
                self.assertFalse(plan['engine_changed'])
                self.assertEqual(set(plan['checks']), CORE)

    def test_core_inputs_are_compared_with_main_even_after_a_cli_only_change(self):
        path = 'crates/seedbox-engine-core/src/rng.rs'
        self.commit(path, 'original')
        self.git('update-ref', 'refs/remotes/origin/main', 'HEAD')
        self.git('branch', '-f', 'base')
        self.commit(path, 'changed')
        self.git('branch', '-f', 'base')
        self.commit('crates/seedbox/src/cli/mod.rs', 'fn run() { execute(); }')
        plan = host_plan.plan_revision(self.repo, 'base', 'HEAD')
        self.assertTrue(plan['engine_changed'])
        self.assertIn('cluster-fixed', plan['checks'])
        self.commit(path, 'original')
        plan = host_plan.plan_revision(self.repo, 'base', 'HEAD')
        self.assertFalse(plan['engine_changed'])
        self.assertNotIn('cluster-fixed', plan['checks'])

    def test_wrapper_and_control_are_engine_inputs_compared_with_main(self):
        for path in ('harness/vmm-rustc.py', 'crates/seedbox-control/Cargo.toml',
                     'crates/seedbox-control/src/lib.rs'):
            with self.subTest(path=path):
                self.commit(path, 'original')
                self.git('update-ref', 'refs/remotes/origin/main', 'HEAD')
                self.git('branch', '-f', 'base')
                self.commit(path, 'changed')
                plan = host_plan.plan_revision(self.repo, 'base', 'HEAD')
                self.assertTrue(plan['engine_changed'])
                self.assertIn('cluster-fixed', plan['checks'])
                self.assertIn(dict(revision=plan['target'], path=path, category='toolchain'),
                              plan['checks']['cluster-fixed'])
                self.commit(path, 'original')
                plan = host_plan.plan_revision(self.repo, 'base', 'HEAD')
                self.assertFalse(plan['engine_changed'])
                self.assertNotIn('cluster-fixed', plan['checks'])

    def test_renames_keep_old_runtime_path_coverage(self):
        source = 'crates/seedbox-vmm/src/disk.rs'
        self.commit(source)
        self.git('update-ref', 'refs/remotes/origin/main', 'HEAD')
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
        self.assertEqual(set(host_plan.plan_revision(self.repo, 'base', 'HEAD')['checks']), {'core'})

    def test_public_plan_names_link_runs_and_diff_gates_short_undo(self):
        path = 'crates/seedbox-vmm/src/machine.rs'
        self.commit(path, 'fn checkpoint() {}\nfn run() {}\n')
        self.git('branch', '-f', 'base')
        self.commit('crates/seedbox-vmm/src/connections.rs')
        result = self.invoke('host-plan.py', 'base', 'HEAD', '--json')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(set(json.loads(result.stdout)['runs']), {'L1', 'L2', 'P1', 'M1'})
        # An unchanged checkpoint line must not select the short undo pair.
        self.commit(path, 'fn checkpoint() {}\nfn run() { execute(); }\n')
        self.assertEqual(set(host_plan.plan_revision(self.repo, 'base', 'HEAD')['runs']),
                         {'L1', 'L2', 'P1', 'M1'})
        self.commit(path, 'fn checkpoint() {}\nfn run() { undo(); }\n')
        result = self.invoke('host-plan.py', 'base', 'HEAD')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('run U2: harness/worlds/shaped-tol-short.json', result.stdout)
        self.assertIn('timeout=600s undo=0 margin=adaptive', result.stdout)
        plan = host_plan.plan_revision(self.repo, 'base', 'HEAD')
        self.assertEqual(set(plan['runs']), {'L1', 'L2', 'P1', 'M1', 'U1', 'U2'})

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
        self.assertIn('check core:', planned.stdout)
        self.assertNotIn('host: none', planned.stdout)
        self.assertEqual(planned.stderr, '')
        self.assertEqual(self.invoke('host_check.py').returncode, 2)

    def test_cli_output_and_legacy_cpu_contract(self):
        self.commit('harness/smoke/amd.py')
        for flags in (['HEAD', '2'], ['--skip-portable', 'HEAD', '2'],
                      ['HEAD', '--lanes', '2']):
            result = self.invoke('host_check.py', '--plan', *flags)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stderr, '')
            self.assertIn('target: ' + self.git('rev-parse', 'HEAD'), result.stdout)
            self.assertIn('check core:', result.stdout)
        for flags in (['HEAD', '0'], ['HEAD', '16'], ['HEAD', 'bad'],
                      ['HEAD', '2', '--lanes', '3']):
            result = self.invoke('host_check.py', '--plan', *flags)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout, '')
            self.assertIn('error:', result.stderr)
        bad = self.invoke('host_check.py', '--plan', '--extra', 'bad')
        self.assertEqual(bad.returncode, 2)
        self.assertEqual(bad.stdout, '')
        self.assertEqual(bad.stderr, 'error: unknown extra check: bad\n')

    def test_invalid_revision_is_error(self):
        self.assertEqual(self.invoke('host-plan.py', 'base', 'missing').returncode, 2)


if __name__ == '__main__':
    unittest.main()
