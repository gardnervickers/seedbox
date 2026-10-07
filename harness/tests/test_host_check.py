"""Portable dispatch and evidence controls using harmless child processes."""
import copy
import contextlib
import io
import json
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


class DispatchTests(unittest.TestCase):
    def test_dependent_replays_share_one_job_no_unrelated_variations(self):
        jobs = host_check.jobs_for(['fixed', 'default-replay', 'test-vmm', 'disk-crash'])
        self.assertEqual([job['name'] for job in jobs], ['core-smoke', 'test-vmm', 'disk-crash'])
        self.assertEqual(jobs[0]['checks'], ['fixed', 'default-replay'])
        with self.assertRaisesRegex(ValueError, 'baseline'):
            host_check.jobs_for(['undo-margin'])

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
                   busybox='busybox', pair=[1, 17], checks=['fixed', 'default-replay'], spec=dict(kind='smoke'))
        commands = Recorder()
        host_worker.run_smoke(job, commands)
        self.assertNotIn('--performance-replay', commands.calls[-1][1])
        self.assertIn('/unused/project/fixed', commands.calls[-1][1])
        self.assertIn('examples/faults/generate.py', commands.calls[0][1])
        job['checks'].append('undo-margin')
        commands = Recorder()
        host_worker.run_smoke(job, commands)
        self.assertIn('--performance-replay', commands.calls[-1][1])
        job['spec'] = dict(kind='compose', project='examples/redis')
        job['checks'] = ['compose-redis']
        commands = Recorder()
        host_worker.run_smoke(job, commands)
        self.assertEqual(len(commands.calls), 1)
        self.assertIn('/source/examples/redis', commands.calls[0][1])
        self.assertNotIn('--performance-replay', commands.calls[0][1])

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
                       PINNED_ENGINE=str(root / 'pinned'), HOST_CHECK_DIR=str(root))
            plan = dict(target='a' * 40, checks=dict(fixed=[], **{'default-replay': [], 'test-vmm': []}))
            args = SimpleNamespace(repo=root, pairs=2, pv_kernel=[])
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

    def exercise_fixture(self, fork=False, mismatch=False):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            output = root / 'output'
            output.mkdir()
            worlds = root / 'harness/worlds'
            worlds.mkdir(parents=True)
            world = dict(end_on_exit='d', machines=[dict(name='d', initrd='unused')])
            if fork:
                world['boot'] = dict(seed=1, fork_at_ns=200000000)
            (worlds / 'disk-crash.json').write_text(json.dumps(world))
            calls = []
            class Recorder:
                env = {'PATH': '/explicit'}
                def __init__(self): self.root = output
                def run(self, name, command, env=None):
                    if name == 'guest-build':
                        return
                    calls.append((name, command, env))
                    def report(seed):
                        witness = 'wrong' if mismatch and name in ('repeat', 'fork-loop') else str(seed)
                        return dict(seed=seed, outcome='end_machine_exited', fault_error=None, world_witness=witness,
                                    machines=[dict(name='d', guest_exit_code=0, guest_init_error=None,
                                                   infrastructure_error=False, state_digest=str(seed))])
                    path = output / 'runs' / f'{name}-s{command[4]}-r0'
                    path.parent.mkdir(exist_ok=True)
                    Path(str(path) + '.status').write_text('0\n')
                    result = dict(seeds=[report(seed) for seed in (1009, 1010, 1011)]) if name == 'fork-loop' else report(int(command[4]))
                    Path(str(path) + '.json').write_text(json.dumps(result))
            job = dict(src=str(root), kernel='frozen-kernel', engine='frozen-engine', pair=[7, 23],
                       spec=dict(kind='fixture', world='disk-crash'))
            if mismatch:
                with self.assertRaisesRegex(RuntimeError, 'differs'):
                    host_worker.run_fixture(job, Recorder())
            else:
                host_worker.run_fixture(job, Recorder())
            self.assertEqual(len(calls), 3 if fork else 2)
            self.assertTrue(all(call[2]['CPU0'] == '7' for call in calls))
            self.assertTrue(all(' ' not in call[1][4] for call in calls))
            if fork:
                self.assertEqual(calls[-1][1][-2:], ['--fork-seeds', '1010,1011'])

    def test_fixture_repeats_sequentially_on_one_cpu_and_rejects_mismatch(self):
        self.exercise_fixture()
        self.exercise_fixture(mismatch=True)

    def test_fork_fixture_compares_cold_to_restored_seeds_on_one_cpu(self):
        self.exercise_fixture(fork=True)
        self.exercise_fixture(fork=True, mismatch=True)

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
