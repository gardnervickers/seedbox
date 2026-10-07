"""Portable runner coordination, hostile summaries and root-boundary controls."""
from contextlib import ExitStack
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import Mock, patch

HERE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HERE))
import host_runner as runner

spec = importlib.util.spec_from_file_location('host_cpu_lease', HERE.parent / 'nix/host-cpu-lease.py')
wrapper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wrapper)
spec = importlib.util.spec_from_file_location('host_smoke_adapter', HERE.parent / 'nix/host-smoke.py')
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)


class PairTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.sysfs = self.root / 'sysfs'
        self.sysfs.mkdir()
        (self.sysfs / 'online').write_text('0-31')
        self.canonical = self.root / 'canonical'
        self.canonical.mkdir()
        self.locks = self.root / 'locks'
        self.locks.mkdir()
        for cpu in range(1, 16):
            (self.locks / f'active-{cpu}').touch()
        for cpu in range(32):
            topology = self.sysfs / f'cpu{cpu}/topology'
            topology.mkdir(parents=True)
            (topology / 'thread_siblings_list').write_text(f'{cpu % 16},{cpu % 16 + 16}')
            (self.canonical / f'seedbox-msr-cpu-{cpu}.lock').touch()

    def reserve(self, cap=2, pairs=None):
        return runner.reserve(self.locks, cap, pairs or range(1, 16), self.canonical)

    def test_online_reciprocal_allowed_pairs_exclude_boot_core(self):
        self.assertEqual(runner.eligible_pairs(self.sysfs, range(32)), list(range(1, 16)))
        (self.sysfs / 'cpu18/topology/thread_siblings_list').write_text('2,19')
        (self.sysfs / 'online').write_text('0-29')
        allowed = set(range(32)) - {17}
        self.assertEqual(runner.eligible_pairs(self.sysfs, allowed), list(range(3, 14)))

    def test_pinned_userspace_is_busy_but_kernel_threads_do_not_exclude_pairs(self):
        proc = self.root / 'proc'
        affinities = {101: {1, 17}, 102: {2}, 103: {3}, 104: set(range(32))}
        for pid in affinities:
            thread = proc / str(pid) / 'task' / str(pid)
            thread.mkdir(parents=True)
            fields = ['S'] + ['0'] * 6
            if pid == 103:
                fields[6] = str(0x00200000)
            (thread / 'stat').write_text(f'{pid} (arbitrary name) ' + ' '.join(fields))
        self.assertEqual(runner.unpinned_pairs([1, 2, 3, 4], proc, affinities.__getitem__), [3, 4])

    def test_concurrent_checks_choose_distinct_pairs_and_obey_cap(self):
        with ExitStack() as stack:
            self.assertEqual(stack.enter_context(self.reserve()), 1)
            self.assertEqual(stack.enter_context(self.reserve()), 2)
            with self.assertRaises(runner.CapacityBusy):
                with self.reserve():
                    self.fail('cap exceeded')
        with self.reserve() as cpu:
            self.assertEqual(cpu, 1)

    def test_either_busy_sibling_is_skipped_without_claiming_cli_lock(self):
        with runner.locked(self.canonical / 'seedbox-msr-cpu-17.lock'):
            with self.reserve() as cpu:
                self.assertEqual(cpu, 2)
                # The CLI must be able to take both canonical leases while the
                # coordinator continues to hold its distinct pair reservation.
                with runner.locked(self.canonical / 'seedbox-msr-cpu-2.lock'), \
                     runner.locked(self.canonical / 'seedbox-msr-cpu-18.lock'):
                    pass

    def test_living_smoke_guard_blocks_reuse_after_coordinator_exit(self):
        with runner.locked(self.locks / 'active-1'):
            with self.reserve() as cpu:
                self.assertEqual(cpu, 2)
        with self.reserve() as cpu:
            self.assertEqual(cpu, 1)

    def test_missing_qualification_leases_are_not_created(self):
        path = self.canonical / 'seedbox-msr-cpu-1.lock'
        path.unlink()
        with self.reserve() as cpu:
            self.assertEqual(cpu, 2)
        self.assertFalse(path.exists())

    def test_exception_releases_capacity_without_unlinking_locks(self):
        with self.assertRaisesRegex(RuntimeError, 'failure'):
            with self.reserve():
                info = (self.locks / 'pair-1').stat()
                raise RuntimeError('failure')
        with self.reserve() as cpu:
            self.assertEqual(cpu, 1)
            self.assertEqual(info.st_ino, (self.locks / 'pair-1').stat().st_ino)

    def test_process_death_releases_reservation(self):
        code = '''
import sys, time
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import host_runner
with host_runner.reserve(Path(sys.argv[2]), 2, [1, 2], Path(sys.argv[3])) as cpu:
    print(cpu, flush=True)
    time.sleep(30)
'''
        child = subprocess.Popen([sys.executable, '-c', code, str(HERE), str(self.locks), str(self.canonical)],
                                 stdout=subprocess.PIPE, text=True)
        try:
            self.assertEqual(child.stdout.readline().strip(), '1')
            with self.reserve() as cpu:
                self.assertEqual(cpu, 2)
            child.kill()
            child.wait(timeout=5)
            with self.reserve() as cpu:
                self.assertEqual(cpu, 1)
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            child.stdout.close()

    def test_invalid_pairs_caps_and_symlink_leases_refuse(self):
        for cap in (0, 16):
            with self.assertRaises(ValueError), self.reserve(cap):
                pass
        for cpu in (0, 16, 31):
            with self.assertRaises(ValueError), self.reserve(pairs=[cpu]):
                pass
        self.locks.mkdir(exist_ok=True)
        (self.locks / 'pair-1').symlink_to(self.canonical / 'seedbox-msr-cpu-1.lock')
        with self.assertRaises(OSError), self.reserve():
            pass


class CoordinatorTests(unittest.TestCase):
    def check(self, mode, failure=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            jobs, locks, canonical = (root / name for name in ('jobs', 'locks', 'canonical'))
            for path in (jobs, locks, canonical):
                path.mkdir()
            (locks / 'active-1').touch()
            for cpu in (1, 17):
                (canonical / f'seedbox-msr-cpu-{cpu}.lock').touch()
            args = type('Args', (), dict(sha='b' * 40, mode=mode,
                summary=root / 'summary.txt', result=root / 'result.json'))()
            environment = dict(HOST_RUNNER_JOBS=str(jobs), HOST_RUNNER_LOCKS=str(locks),
                HOST_RUNNER_CAP='1', HOST_CHECK_CPU_WRAPPER='/installed/wrapper',
                HOST_RUNNER_PINNED_ENGINE='/stored/engine',
                PINNED_ENGINE='/ambient/engine', SEEDBOX_VMM='/ambient/vmm')
            child = Mock(pid=987654)
            child.poll.return_value = None if failure == 'cancel' else 0
            child.wait.side_effect = [KeyboardInterrupt('cancel'), -15] if failure == 'cancel' else None
            child.wait.return_value = 1 if failure == 'check' else 0
            commands = []
            def clean(command, **kwargs):
                commands.append(command)
                if command[:3] == ['sudo', '-n', '/installed/wrapper']:
                    import shutil
                    shutil.rmtree(command[-1])
                return subprocess.CompletedProcess(command, 0)
            actual_reserve = runner.reserve
            with patch.dict(os.environ, environment), \
                 patch.object(runner.os, 'sched_getaffinity', create=True, return_value=range(32)), \
                 patch.object(runner, 'eligible_pairs', return_value=[1]), \
                 patch.object(runner, 'unpinned_pairs', return_value=[1]), \
                 patch.object(runner, 'reserve', side_effect=lambda d, c, p: actual_reserve(d, c, p, canonical)), \
                 patch.object(runner.subprocess, 'Popen', return_value=child) as spawn, \
                 patch.object(runner.subprocess, 'run', side_effect=clean), \
                 patch.object(runner.os, 'killpg') as kill:
                if failure == 'spawn':
                    spawn.side_effect = OSError('injected spawn failure')
                code = runner.run(args)
                env = spawn.call_args.kwargs['env']
                self.assertEqual(env.get('PINNED_ENGINE'), '/stored/engine' if mode == 'dev' else None)
                self.assertEqual(env.get('SEEDBOX_VMM'), '/stored/engine' if mode == 'pinned' else None)
                self.assertEqual(spawn.call_args.args[0][-2:], [args.sha, '1'])
                if failure == 'cancel':
                    kill.assert_called_once()
            value = json.loads(args.result.read_text())
            self.assertEqual(value['status'], 'cancelled' if failure == 'cancel' else 'fail' if failure else 'pass')
            self.assertEqual(code, 1 if failure else 0)
            self.assertEqual(list(jobs.iterdir()), [])
            self.assertTrue(any(command[0] == 'git' and command[-1] == 'prune' for command in commands))
            with actual_reserve(locks, 1, [1], canonical) as cpu:
                self.assertEqual(cpu, 1)

    def test_modes_use_stored_engine_and_clear_ambient_overrides(self):
        self.check('dev')
        self.check('pinned')

    def test_failed_checks_spawn_errors_and_cancellation_clean_job_and_release_pair(self):
        for failure in ('check', 'spawn', 'cancel'):
            self.check('dev', failure)


class CommentTests(unittest.TestCase):
    sha = 'a' * 40
    url = 'https://github.com/gardnervickers/seedbox/actions/runs/123'

    def comment(self, text, status='pass', mode='dev'):
        return runner.format_comment(text, self.sha, self.url, status, mode)

    def test_summary_and_sha_are_quoted_with_visible_qualification(self):
        text = 'host-check abc cpu 8 (8,24)\nsmoke pass 122s\nphase fixed: 50.0s\n'
        body = self.comment(text)
        self.assertIn(text.rstrip(), body)
        self.assertIn(self.sha, body)
        self.assertIn('**PASS**', body)
        self.assertIn('dev-unpinned (unqualified)', body)
        self.assertIn(f'[Workflow logs]({self.url})', body)
        self.assertIn('stored pinned engine', self.comment(text, 'fail', 'pinned'))

    def test_guest_text_cannot_close_fence_or_ping_owner(self):
        body = self.comment('```\n@everyone <script>\n\x00bad\rtext\n`````')
        self.assertIn('``````text\n', body)
        self.assertNotIn('@everyone', body)
        self.assertNotIn('<script>', body)
        self.assertNotIn('\x00', body)
        self.assertNotIn('\r', body)
        self.assertTrue(body.endswith('\n``````\n'))

    def test_comment_size_and_pathological_backticks_are_bounded(self):
        for text in ('x' * 100000, '`' * 100000, '@' * 100000, '\U0001f680' * 100000):
            body = self.comment(text)
            self.assertLess(len(body), 65536)
            self.assertLess(len(body.encode('utf-8')), 65536)
            self.assertIn('summary truncated', body)

    def test_invalid_metadata_refuses(self):
        cases = [dict(sha='bad'), dict(run_url='https://evil.invalid'),
                 dict(status='unknown'), dict(mode='qualified')]
        for changes in cases:
            values = dict(summary='test', sha=self.sha, run_url=self.url, status='pass', mode='dev')
            values.update(changes)
            with self.assertRaises(ValueError):
                runner.format_comment(**values)

    def test_cli_formats_without_host_access(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'summary.txt').write_text('smoke FAIL 1s\n')
            (root / 'result.json').write_text(json.dumps(dict(sha=self.sha, mode='dev', status='fail')))
            result = subprocess.run([sys.executable, str(HERE / 'host_runner.py'), 'comment',
                '--summary', str(root / 'summary.txt'), '--result', str(root / 'result.json'),
                '--run-url', self.url], capture_output=True, text=True, check=True)
            self.assertIn('**FAIL**', result.stdout)
            self.assertEqual(result.stderr, '')


class WrapperTests(unittest.TestCase):
    def test_module_installs_job_filter_and_only_lease_wrapper_sudo(self):
        module = (HERE.parent / 'nix/host-runner.nix').read_text()
        self.assertIn('${pkgs.bash}/bin/bash ${./host-job-started.sh}', module)
        self.assertIn('ACTIONS_RUNNER_HOOK_JOB_STARTED = "${jobStarted}";', module)
        rules = module.split('security.sudo.extraRules =', 1)[1].split('systemd.services =', 1)[0]
        self.assertEqual(rules.count('command ='), 1)
        self.assertIn('command = "${wrapper}/bin/seedbox-host-cpu-lease"', rules)
        self.assertNotIn('cfg.pinnedEngine', rules)

    def test_builder_storage_and_runtime_are_owned_by_the_job_unit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            runtime = root / 'runtime'
            runtime.mkdir()
            env = adapter.builder_environment(root, runtime)
            storage = tomllib.loads(Path(env['CONTAINERS_STORAGE_CONF']).read_text())['storage']
            engine = tomllib.loads(Path(env['CONTAINERS_CONF']).read_text())['engine']
            self.assertEqual(storage['graphroot'], str(root / 'containers'))
            self.assertEqual(storage['runroot'], str(runtime))
            self.assertEqual(storage['driver'], 'vfs')
            self.assertEqual(engine['tmp_dir'], str(runtime / 'libpod'))
            self.assertEqual(engine['cgroup_manager'], 'cgroupfs')
            self.assertEqual(env['BUILDAH_ISOLATION'], 'chroot')
            self.assertTrue(Path(env['TMPDIR']).is_relative_to(root))

    def test_job_cleanup_paths_cannot_escape_root(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            job = root / 'job-abc123'
            job.mkdir()
            self.assertEqual(wrapper.job_path(str(job), [root]), job.resolve())
            link = root / 'job-link'
            link.symlink_to(job, target_is_directory=True)
            for path in (root, root / '../job-escape', link, root / 'not-a-job'):
                with self.assertRaises(ValueError):
                    wrapper.job_path(str(path), [root])
            work = job / 'host-check-abcdef0-cpu15-aBc123'
            work.mkdir()
            self.assertEqual(wrapper.work_path(str(work), job), work.resolve())
            for path in (job / 'host-check-abcdef0-cpu0-aBc123', root / work.name):
                with self.assertRaises(ValueError):
                    wrapper.work_path(str(path), job)

    def test_runner_owned_pinned_artifacts_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'engine'
            path.write_bytes(b'engine')
            with self.assertRaisesRegex(ValueError, 'not root-owned'):
                wrapper.owned_input(path)


class JobHookTests(unittest.TestCase):
    allowed = dict(
        GITHUB_REPOSITORY='gardnervickers/seedbox',
        GITHUB_WORKFLOW_REF='gardnervickers/seedbox/.github/workflows/host-check.yml@refs/heads/main',
        GITHUB_JOB='host', GITHUB_EVENT_NAME='pull_request_target')

    def hook(self, environment):
        return subprocess.run(['bash', str(HERE.parent / 'nix/host-job-started.sh')],
                              env=environment, capture_output=True, text=True)

    def test_only_main_host_job_for_both_request_events_is_allowed(self):
        for event in ('pull_request_target', 'issue_comment'):
            result = self.hook(dict(self.allowed, GITHUB_EVENT_NAME=event))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout + result.stderr, '')

    def test_other_repositories_workflows_refs_jobs_and_events_refuse(self):
        cases = {
            'GITHUB_REPOSITORY': ['other/seedbox'],
            'GITHUB_WORKFLOW_REF': [
                'gardnervickers/seedbox/.github/workflows/x.yml@refs/heads/main',
                'gardnervickers/seedbox/.github/workflows/host-check.yml@refs/heads/attack',
                'gardnervickers/seedbox/.github/workflows/host-check.yml@refs/pull/112/merge',
                'other/seedbox/.github/workflows/host-check.yml@refs/heads/main'],
            'GITHUB_JOB': ['gate', 'report', 'host-extra'],
            'GITHUB_EVENT_NAME': ['push', 'pull_request', 'workflow_dispatch'],
        }
        for key, values in cases.items():
            for value in values:
                with self.subTest(key=key, value=value):
                    result = self.hook(dict(self.allowed, **{key: value}))
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn('host runner refused job:', result.stderr)
                    self.assertEqual(result.stdout, '')
                    self.assertNotIn(value, result.stderr)

    def test_missing_or_empty_identity_fails_closed(self):
        for key in self.allowed:
            for value in (None, ''):
                environment = self.allowed.copy()
                if value is None:
                    del environment[key]
                else:
                    environment[key] = value
                with self.subTest(key=key, value=value):
                    self.assertNotEqual(self.hook(environment).returncode, 0)
        self.assertNotEqual(self.hook({}).returncode, 0)

    def test_installed_hook_rejection_stops_worker_before_an_always_step(self):
        module = (HERE.parent / 'nix/host-runner.nix').read_text()
        launcher = module.split('jobStarted = pkgs.writeShellScript "seedbox-host-job-started.sh" \'\'\n', 1)[1]
        launcher = launcher.split("  '';", 1)[0]
        launcher = launcher.replace('${pkgs.bash}/bin/bash', shlex.quote(shutil.which('bash')))
        launcher = launcher.replace('${./host-job-started.sh}', shlex.quote(str(HERE.parent / 'nix/host-job-started.sh')))
        # A disposable process stands in for Runner.Worker. It tries to run an
        # unconditional step after the hook, even if the hook failed.
        worker = '''
import subprocess, sys
subprocess.run(['bash', '-c', sys.argv[1]])
print('always step reached', flush=True)
'''
        for allowed in (True, False):
            environment = dict(os.environ, **self.allowed)
            if not allowed:
                environment['GITHUB_WORKFLOW_REF'] = 'gardnervickers/seedbox/.github/workflows/x.yml@refs/heads/attack'
            result = subprocess.run([sys.executable, '-c', worker, launcher], env=environment,
                                    capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0 if allowed else -9)
            self.assertEqual('always step reached' in result.stdout, allowed)
            self.assertEqual('host runner refused job:' in result.stderr, not allowed)


class WorkflowCheckoutTests(unittest.TestCase):
    def test_missing_commit_stops_before_candidate_execution_without_fetching(self):
        workflow = (HERE.parent / '.github/workflows/host-check.yml').read_text()
        host = workflow.split('\n  host:\n', 1)[1].split('\n  report:\n', 1)[0]
        check = next(line.strip() for line in host.splitlines() if line.strip().startswith('git cat-file'))
        with tempfile.TemporaryDirectory() as directory:
            subprocess.run(['git', 'init', '-q', directory], check=True)
            subprocess.run(['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                            '-c', 'commit.gpgsign=false', '-c', 'core.hooksPath=/dev/null',
                            'commit', '-q', '--allow-empty', '-m', 'fixture'], cwd=directory, check=True)
            sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=directory, text=True).strip()
            for candidate, present in ((sha, True), ('0' * 40, False)):
                result = subprocess.run(['bash', '-e', '-c', check + '\nprintf "coordinator reached\\n"'],
                                        cwd=directory, env=dict(os.environ, SHA=candidate),
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, present)
                self.assertEqual('coordinator reached' in result.stdout, present)
        self.assertNotIn('git fetch', host)
        self.assertIn('fetch-depth: 0', host)
        self.assertIn('persist-credentials: false', host)


if __name__ == '__main__':
    unittest.main()
