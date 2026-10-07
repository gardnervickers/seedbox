"""Portable dispatch and evidence controls using harmless child processes."""
import copy
import contextlib
import io
import json
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import host_check
import host_worker
import host_compose


class DispatchTests(unittest.TestCase):
    def test_dependent_replays_share_one_job_no_unrelated_variations(self):
        jobs = host_check.jobs_for(['core', 'cluster-fixed', 'disk-crash'])
        self.assertEqual([job['name'] for job in jobs], ['core-smoke', 'cluster-fixed', 'disk-crash'])
        self.assertEqual(jobs[0]['checks'], ['core'])
        with self.assertRaisesRegex(ValueError, 'baseline'):
            host_check.jobs_for(['perf-replay'])

    def test_concurrent_lanes_bounded_and_all_times_retained(self):
        jobs = [dict(name=str(i), checks=[str(i)]) for i in range(5)]
        children, peak, pairs_seen = [], [], []
        def launch(job, pair):
            active = sum(child.poll() is None for child in children)
            peak.append(active + 1)
            pairs_seen.append(pair)
            child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(.2)'])
            children.append(child)
            return child
        result = host_check.run_jobs(jobs, [(1, 17), (2, 18)], launch)
        self.assertEqual(result['status'], 'PASS')
        self.assertEqual(len(result['jobs']), 5)
        self.assertEqual(max(peak), 2)
        self.assertTrue(all(job['wall_seconds'] > 0 for job in result['jobs']))
        self.assertEqual(set(pairs_seen), {(1, 17), (2, 18)})
        self.assertTrue(all(child.returncode == 0 for child in children))

    def test_failure_stops_dispatch_and_joins_active_child(self):
        jobs = [dict(name=str(i), checks=[str(i)]) for i in range(4)]
        children = []
        def launch(job, pair):
            script = 'import sys; sys.exit(1)' if job['name'] == '0' else 'import time; time.sleep(10)'
            child = subprocess.Popen([sys.executable, '-c', script])
            children.append(child)
            return child
        result = host_check.run_jobs(jobs, [(1, 17), (2, 18)], launch)
        self.assertEqual(result['status'], 'FAIL')
        self.assertEqual(result['undispatched'], ['2', '3'])
        self.assertEqual(len(result['jobs']), 2)
        self.assertTrue(all(child.returncode is not None for child in children))

    def test_cancellation_before_dispatch(self):
        launch = lambda *args: self.fail('cancelled batch dispatched')
        result = host_check.run_jobs([dict(name='fixed', checks=['fixed'])], [(1, 17)], launch, lambda: True)
        self.assertEqual(result['status'], 'FAIL')
        self.assertEqual(result['undispatched'], ['fixed'])

    def test_fixture_oracle_refuses_identical_failures_and_missing_state(self):
        report = dict(outcome='end_machine_exited', fault_error=None, world_witness='abc',
                      machines=[dict(name='d', guest_exit_code=0, guest_init_error=None,
                                     infrastructure_error=False, state_digest='def')])
        world = dict(end_on_exit='d')
        self.assertEqual(host_worker.fixture_evidence(report, world), ('abc', {'d': 'def'}))
        for field, value in [('guest_exit_code', 1), ('guest_init_error', 'failed'),
                             ('infrastructure_error', True), ('state_digest', None)]:
            bad = copy.deepcopy(report)
            bad['machines'][0][field] = value
            with self.assertRaises(RuntimeError):
                host_worker.fixture_evidence(bad, world)
        bad = dict(report, outcome='duration')
        with self.assertRaises(RuntimeError):
            host_worker.fixture_evidence(bad, world)

    def test_smoke_commands_only_include_planned_performance_change(self):
        class Recorder:
            root = Path('/unused')
            env = {'PATH': '/explicit'}
            def __init__(self): self.calls = []
            def run(self, name, command, env=None): self.calls.append((name, command))
        job = dict(src='/source', cli='cli', engine='engine', kernel='kernel', guest_init='init',
                   busybox='busybox', pair=[1, 17], checks=['core'], spec=dict(kind='smoke'))
        commands = Recorder()
        host_worker.run_smoke(job, commands)
        self.assertNotIn('--perf-replay', commands.calls[-1][1])
        self.assertIn('/unused/project/fixed', commands.calls[-1][1])
        self.assertIn('examples/faults/generate.py', commands.calls[0][1])
        job['checks'].append('perf-replay')
        commands = Recorder()
        host_worker.run_smoke(job, commands)
        self.assertIn('--perf-replay', commands.calls[-1][1])

    def test_execution_refuses_mac_before_inputs_or_mutation(self):
        with patch.object(host_check.sys, 'platform', 'darwin'), \
             patch.object(host_check.tempfile, 'mkdtemp', side_effect=AssertionError('mutation')):
            with self.assertRaisesRegex(RuntimeError, 'Linux'):
                host_check.execute({}, None)

    def test_lane_admission_refusal_stops_before_parallel_dispatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ('kernel', 'busybox', 'pinned'):
                (root / name).write_bytes(name.encode())
            calls = []
            class Source:
                cancelled = False
                def __init__(self, work, src, env): self.work = work
                def interrupt(self, *args): self.cancelled = True
                def run(self, name, command):
                    calls.append(name)
                    (self.work / (name + '.wall.json')).write_text('{"wall_seconds": 0}')
                    if name.startswith('doctor-'):
                        raise RuntimeError('admission refusal')
            @contextlib.contextmanager
            def reserved(*args, **kwargs):
                yield [(1, 17), (2, 18)]
            env = dict(SEEDBOX_KERNEL=str(root / 'kernel'), SEEDBOX_BUSYBOX=str(root / 'busybox'),
                       PINNED_ENGINE=str(root / 'pinned'), HOST_CHECK_DIR=str(root), PATH='/explicit')
            plan = dict(target='a' * 40, checks=dict(core=[]))
            args = SimpleNamespace(repo=root, lanes=[1,2], skip_portable=False)
            with patch.object(host_check.sys, 'platform', 'linux'), \
                 patch.dict(host_check.os.environ, env, clear=True), \
                 patch.object(host_check, 'Commands', Source), \
                 patch.object(host_check, 'read_pairs', return_value=[(1, 17), (2, 18)]), \
                 patch.object(host_check, 'allocate_pairs', reserved), \
                 patch.object(host_check, 'run_jobs', side_effect=AssertionError('guest dispatch')), \
                 contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, 'admission refusal'):
                    host_check.execute(plan, args)
            self.assertEqual(calls[-1], 'doctor-cpu1')
            self.assertNotIn('doctor-cpu2', calls)
            summary = json.loads(next(root.glob('host-check-*/summary.json')).read_text())
            self.assertEqual(summary['status'], 'FAIL')
            self.assertTrue(all(check['status'] == 'NOT_RUN' for check in summary['checks'].values()))

    def exercise_gate(self, skip_portable=False, stored=False, fail=None):
        with tempfile.TemporaryDirectory(prefix='host gate with spaces ') as tmp:
            root = Path(tmp)
            for name in ('kernel', 'busybox', 'pinned', 'stored'):
                (root / name).write_bytes(name.encode())
            calls = []
            class Source:
                cancelled = False
                def __init__(self, work, src, env): self.work = work
                def interrupt(self, *args): self.cancelled = True
                def run(self, name, command):
                    calls.append((name, command))
                    (self.work / (name + '.wall.json')).write_text(json.dumps(
                        dict(wall_seconds=0, status='FAIL' if name == fail else 'PASS')))
                    if name == fail:
                        raise RuntimeError(name + ': injected failure')
            @contextlib.contextmanager
            def reserved(*args, **kwargs):
                yield [(2, 18)]
            env = dict(SEEDBOX_KERNEL=str(root / 'kernel'), SEEDBOX_BUSYBOX=str(root / 'busybox'),
                       PINNED_ENGINE=str(root / 'pinned'), HOST_CHECK_DIR=str(root), PATH='/explicit')
            if stored:
                env['SEEDBOX_VMM'] = str(root / 'stored')
                env.pop('PINNED_ENGINE')
            plan = dict(target='a' * 40, checks=dict(core=[]))
            args = SimpleNamespace(repo=root, lanes=[2], skip_portable=skip_portable)
            with patch.object(host_check.sys, 'platform', 'linux'), \
                 patch.dict(host_check.os.environ, env, clear=True), \
                 patch.object(host_check, 'Commands', Source), \
                 patch.object(host_check, 'read_pairs', return_value=[(2, 18)]), \
                 patch.object(host_check, 'allocate_pairs', reserved), \
                 patch.object(host_check, 'run_jobs', return_value=dict(status='PASS', jobs=[], undispatched=[])) as dispatch, \
                 patch.object(host_check, 'finish'), contextlib.redirect_stdout(io.StringIO()):
                if fail:
                    with self.assertRaisesRegex(RuntimeError, 'injected failure'):
                        host_check.execute(plan, args)
                    dispatch.assert_not_called()
                else:
                    self.assertEqual(host_check.execute(plan, args), 0)
                    dispatch.assert_called_once()
            return calls

    def test_default_portable_steps_before_guest_admission(self):
        calls = self.exercise_gate()
        names = [name for name, command in calls]
        self.assertEqual(names, ['worktree', 'build', 'vmm-build', 'fmt', 'clippy',
                                 'suite', 'guest-init', 'doctor-cpu2'])
        self.assertIn(['cargo', 'fmt', '-p', 'seedbox', '--check'], [cmd for name, cmd in calls])
        self.assertIn(['nice', 'cargo', 'clippy', '-p', 'seedbox', '--all-targets', '--locked',
                       '--', '-D', 'warnings'], [cmd for name, cmd in calls])
        self.assertIn(['nice', 'bash', 'harness/acceptance.sh', 'portable'], [cmd for name, cmd in calls])

    def test_skip_portable_keeps_vmm_tests_with_built_or_stored_engine(self):
        for stored in (False, True):
            with self.subTest(stored=stored):
                calls = self.exercise_gate(skip_portable=True, stored=stored)
                names = [name for name, cmd in calls]
                self.assertEqual(names, ['worktree', 'build'] + ([] if stored else ['vmm-build']) +
                                 ['vmm-test', 'guest-init', 'doctor-cpu2'])
                self.assertIn(['nice', 'cargo', 'test-vmm'], [cmd for name, cmd in calls])

    def test_portable_or_vmm_test_failure_prevents_guest_dispatch(self):
        for skip, name in ((False, 'fmt'), (False, 'clippy'), (False, 'suite'), (True, 'vmm-test')):
            with self.subTest(name=name):
                calls = self.exercise_gate(skip_portable=skip, fail=name)
                self.assertEqual(calls[-1][0], name)
                self.assertFalse(any(step.startswith('doctor-') for step, cmd in calls))

    def test_dispatched_check_failure_exit_and_stderr_contract(self):
        plan = dict(target='a' * 40, base='b' * 40, checks={'core': []}, warnings=[])
        with patch.object(host_check, 'plan_revision', return_value=plan), \
             patch.object(host_check, 'execute', side_effect=host_worker.CheckFailure('vmm-test: exit 7')), \
             contextlib.redirect_stdout(io.StringIO()) as out, \
             contextlib.redirect_stderr(io.StringIO()) as err:
            self.assertEqual(host_check.main(['--skip-portable', 'HEAD', '2']), 1)
        self.assertEqual(err.getvalue(), 'error: vmm-test: exit 7\n')
        self.assertIn('target: ' + 'a' * 40, out.getvalue())
        with tempfile.TemporaryDirectory() as tmp:
            commands = host_worker.Commands(Path(tmp), Path(tmp), {})
            with self.assertRaises(host_worker.CheckFailure):
                commands.run('vmm-test', [sys.executable, '-c', 'import sys; sys.exit(7)'])
            receipt = json.loads((Path(tmp) / 'vmm-test.wall.json').read_text())
            self.assertEqual(receipt['status'], 'FAIL')

    def test_legacy_cpu_and_other_pr_flags_reach_execution(self):
        plan = dict(target='a' * 40, base='b' * 40, checks={'core': []}, warnings=[])
        for argv, lanes, skip in ((['HEAD', '2'], [2], False),
                                 (['--skip-portable', 'HEAD', '2', '--extra', 'net'], [2], True),
                                 (['HEAD', '--lanes', '2,3'], [2,3], False)):
            with patch.object(host_check, 'plan_revision', return_value=plan) as planner, \
                 patch.object(host_check, 'execute', return_value=0) as execute, \
                 contextlib.redirect_stdout(io.StringIO()) as output:
                self.assertEqual(host_check.main(argv), 0)
                args = execute.call_args.args[1]
                self.assertEqual(args.lanes, lanes)
                self.assertEqual(args.skip_portable, skip)
                self.assertEqual(planner.call_args.args[3], ['net'] if skip else [])
                self.assertIn('target: ' + 'a' * 40, output.getvalue())

    def test_fixture_runs_only_selected_world_once_on_assigned_pair(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            output = root / 'output'
            output.mkdir()
            worlds = root / 'harness/worlds'
            worlds.mkdir(parents=True)
            world = dict(end_on_exit='d', machines=[dict(name='d', initrd='unused')])
            (worlds / 'disk-crash.json').write_text(json.dumps(world))
            calls = []
            class Recorder:
                env = {'PATH': '/explicit'}
                def __init__(self): self.root = output
                def run(self, name, command, env=None):
                    calls.append((name, command, env))
                    if name == 'guest-build': return
                    path = output / 'runs' / f'{name}-s{command[4]}-r0'
                    path.parent.mkdir(exist_ok=True)
                    Path(str(path) + '.status').write_text('0\n')
                    report = dict(outcome='end_machine_exited', fault_error=None, world_witness='w',
                                  machines=[dict(name='d', guest_exit_code=0, guest_init_error=None,
                                                 infrastructure_error=False, state_digest='s')])
                    Path(str(path) + '.json').write_text(json.dumps(report))
            job = dict(src=str(root), kernel='kernel', engine='engine', pair=[7,23],
                       spec=dict(kind='fixture', world='disk-crash'))
            host_worker.run_fixture(job, Recorder())
            self.assertEqual(len(calls), 2)
            self.assertEqual(calls[0][1][0], 'nice')
            self.assertNotIn('taskset', calls[0][1])
            self.assertEqual(calls[1][1][0:2], ['bash', str(host_worker.HERE / 'batch.sh')])
            self.assertEqual(calls[1][1][4:], ['1009', '--digest'])
            self.assertEqual(calls[1][2]['CPU0'], '7')
            self.assertEqual(calls[1][2]['SEEDBOX_MARGIN'], 'adaptive')

    def test_explicit_lanes_reject_host_pair_and_duplicates(self):
        self.assertEqual(host_check.parse_lanes('3,1,2'), [3,1,2])
        import argparse
        for text in ('0', '16', '0,1', '1,1', '2,18', '', '-1', 'x'):
            with self.subTest(text=text), self.assertRaises(argparse.ArgumentTypeError):
                host_check.parse_lanes(text)

    def test_cluster_compares_this_engine_and_main_with_identical_inputs(self):
        for differs in (False, True):
            with self.subTest(differs=differs), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                output = root / 'output'
                output.mkdir()
                worlds = root / 'harness/worlds'
                worlds.mkdir(parents=True)
                (worlds / 'cluster-fixed.json').write_text(json.dumps(
                    dict(end_on_exit='d', machines=[dict(name='d', initrd='unused')], kernel='unused')))
                calls = []
                class Recorder:
                    env = {'PATH': '/explicit'}
                    def __init__(self): self.root = output
                    def run(self, name, command, env=None):
                        calls.append((name, command, dict(env or {})))
                        if name.endswith('build'): return
                        path = output / 'runs' / f'{name}-s1009-r0'
                        path.parent.mkdir(exist_ok=True)
                        Path(str(path) + '.status').write_text('0\n')
                        witness = 'different' if differs and name == 'main' else 'w'
                        report = dict(outcome='end_machine_exited', fault_error=None, world_witness=witness,
                                      run_ms=1 if name == 'main' else 2,
                                      machines=[dict(name='d', guest_exit_code=0, guest_init_error=None,
                                                     infrastructure_error=False, state_digest='s', witness='m')])
                        Path(str(path) + '.json').write_text(json.dumps(report))
                job = dict(src=str(root), kernel='kernel', engine='candidate', main_engine='main',
                           main_revision='main-sha', etcd='etcd', pair=[3,19],
                           spec=dict(kind='fixture',world='cluster-fixed'))
                host_worker.run_fixture(job, Recorder())
                self.assertEqual([call[0] for call in calls], ['client-build','guest-build','fixed','main'])
                self.assertTrue(all(call[1][0] == 'nice' and 'taskset' not in call[1] for call in calls[:2]))
                self.assertEqual(calls[2][1][2], calls[3][1][2])
                self.assertEqual(calls[2][1][4:], ['1009','--digest'])
                self.assertEqual([call[2]['VMM'] for call in calls[2:]], ['candidate','main'])
                result = json.loads((output / 'cluster-comparison.json').read_text())
                self.assertEqual(result['status'], 'DIFFERENT' if differs else 'MATCH')
                self.assertEqual(result['main_revision'], 'main-sha')

    def test_pass_keeps_summaries_and_deletes_state_failure_keeps_state(self):
        for passed in (False, True):
            with self.subTest(passed=passed), tempfile.TemporaryDirectory() as tmp:
                work = Path(tmp) / 'owned work'
                (work / 'src').mkdir(parents=True)
                (work / 'main').mkdir()
                (work / 'state').mkdir()
                commands = []
                class Source:
                    def run(self, name, command):
                        commands.append(command)
                        shutil.rmtree(command[-1])
                source = Source()
                summary = dict(status='PASS' if passed else 'FAIL',source_wall_seconds={'build':1},
                               jobs=[dict(name='core-smoke',exit=0 if passed else 1,wall_seconds=2,pair=[1,17],
                                          receipt={'phase_wall_seconds':{'fixed':1,'default':.5}})])
                def remove(command, **kwargs):
                    self.assertEqual(command[:5], ['sudo','-n','rm','-rf','--'])
                    shutil.rmtree(command[-1])
                output = io.StringIO()
                with patch.object(host_check.subprocess,'run',side_effect=remove) as cleanup, \
                     contextlib.redirect_stdout(output):
                    host_check.finish(work, Path(tmp), summary, source)
                self.assertEqual(work.exists(), not passed)
                self.assertEqual(cleanup.call_count, int(passed))
                self.assertIn('phase default: 0.5s', output.getvalue())
                self.assertIn('build    UNKNOWN    1s', output.getvalue())
                self.assertEqual('cleanup:' in output.getvalue(), not passed)
                if passed:
                    self.assertEqual(len(commands), 2)
                    self.assertEqual(json.loads(work.with_suffix('.json').read_text())['status'],'PASS')
                else:
                    self.assertIn("'" + str(work) + "'", output.getvalue())

    def test_focused_control_runs_then_replays_without_performance_flags(self):
        from types import SimpleNamespace
        campaign = SimpleNamespace(args=SimpleNamespace(cli='cli',kernel='kernel',engine='engine',
            guest_init='init',busybox='busybox',cpu=1), state=Path('/owned/state'), last_run=Path('/owned/run'))
        calls, verified = [], []
        def invoke(name, command, execution):
            calls.append((name,command,execution))
            return (1 if name == 'run' else 0), 'FAIL: /owned/run\n', ''
        campaign.invoke = invoke
        campaign.inspect = lambda *args: dict(outcome='FAIL',error=None,identity={'key':'same'},
            replay=dict(status='MATCH',outcome_matches=True,witness_matches=True,
                        full_state_matches=True,fault_history_matches=True))
        job = dict(spec={'expected':'FAIL'})
        host_compose.run_pair(job,campaign,Path('/project/control-cases'),verified.append)
        self.assertEqual([call[0] for call in calls], ['run','replay'])
        self.assertEqual(verified, [Path('/owned/run'),Path('/owned/run')])
        self.assertIn('--full-state',calls[0][1])
        self.assertEqual(calls[1][1], ['cli','replay','/owned/run','--cpu','1','--state','/owned/state'])
        for call in calls:
            self.assertTrue(call[2])
            self.assertNotIn('--undo',call[1])
            self.assertNotIn('--margin',call[1])

    def test_unit_command_uses_existing_ownership_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            sys.path.insert(0, str(host_worker.HERE / 'smoke'))
            import owned
            life = dict(signals=[], failure=None, manager_handoff=False, leader_reaped=True,
                        owned_session_gone=True, exit=0)
            commands = host_worker.Commands(root, root, {})
            with patch.object(owned, 'run_owned_replay', return_value=life) as run:
                commands.run('unit', ['never-executed'], owned=True)
            run.assert_called_once()
            self.assertTrue((root / 'unit.wall.json').exists())
            life['manager_handoff'] = True
            with patch.object(owned, 'run_owned_replay', return_value=life):
                with self.assertRaisesRegex(RuntimeError, 'cleanup failed or uncertain'):
                    commands.run('uncertain', ['never-executed'], owned=True)


if __name__ == '__main__':
    unittest.main()
