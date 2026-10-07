"""One allocated host check, invoked only by host_check.py on Linux.

Public CLI smoke and the existing batch supervisor own execution, restoration
and canonical leases. This module does not write MSRs or operate KVM directly.
"""
import json
import os
from pathlib import Path
import signal
import re
import subprocess
import sys
import time

from host_pairs import probe_canonical, read_pairs

HERE = Path(__file__).resolve().parent


class CheckFailure(RuntimeError):
    """A dispatched check failed (host gate exit 1)."""


class Commands:
    def __init__(self, root, src, env):
        self.root, self.src, self.env = root, src, env
        self.child = None
        self.cancelled = False

    def interrupt(self, sig, frame):
        self.cancelled = True
        if self.child is not None:
            self.child.send_signal(sig)

    def run(self, name, command, env=None, owned=False):
        if self.cancelled:
            raise RuntimeError('check cancelled; no later command dispatched')
        began = time.monotonic()
        status = 'FAIL'
        (self.root / (name + '.command.json')).write_text(json.dumps(command) + '\n')
        try:
            with (self.root / (name + '.log')).open('x') as out:
                if owned:
                    # cargo can spawn a test process: reap the entire owned session
                    # before returning to the canonical-lease context manager.
                    sys.path.insert(0, str(HERE / 'smoke'))
                    from owned import run_owned_replay
                    deadline = time.monotonic() + 1800
                    life = run_owned_replay(command, env or self.env, out, out, deadline - 30, deadline,
                        lambda child: setattr(self, 'child', child), lambda: None,
                        lambda value: (self.root / (name + '.lifecycle.json')).write_text(json.dumps(value) + '\n'),
                        cwd=self.src)
                    self.cancelled = self.cancelled or bool(life['signals'])
                    if life['failure'] or life['manager_handoff'] or not life['leader_reaped'] or not life['owned_session_gone']:
                        raise RuntimeError('unit-test command ownership/cleanup failed or uncertain')
                    code = life['exit']
                else:
                    self.child = subprocess.Popen(command, cwd=self.src, env=env or self.env,
                                                  stdout=out, stderr=subprocess.STDOUT)
                    # A catchable signal never releases the lane before the wrapper returns.
                    code = self.child.wait()
            if code or self.cancelled:
                raise CheckFailure(f'{name}: exit {code}; see {self.root / (name + ".log")}')
            status = 'PASS'
        finally:
            if not owned and self.child is not None and self.child.returncode is None:
                self.child.wait()
            self.child = None
            (self.root / (name + '.wall.json')).write_text(json.dumps({'status': status, 'wall_seconds': time.monotonic() - began}) + '\n')


def fixture_evidence(report, world):
    """Reject repeated errors; compare guest/world witnesses, never host counters."""
    if report['outcome'] != 'end_machine_exited' or report.get('fault_error'):
        raise RuntimeError('fixture did not reach its expected terminal machine')
    machines = report['machines']
    terminal = next(machine for machine in machines if machine['name'] == world['end_on_exit'])
    if terminal['guest_exit_code'] != 0:
        raise RuntimeError('fixture guest failed')
    if any(machine['guest_init_error'] or machine['infrastructure_error'] for machine in machines):
        raise RuntimeError('fixture bootstrap/infrastructure failure')
    witness = report['world_witness']
    digests = {m['name']: m['state_digest'] for m in machines}
    if not witness or not digests or not all(digests.values()):
        raise RuntimeError('fixture witnesses/full state absent')
    return witness, digests


def run_fixture(job, commands):
    src, root = Path(job['src']), commands.root
    spec = job['spec']
    world = json.loads((src / 'harness/worlds' / (spec['world'] + '.json')).read_text())
    artifacts = root / 'artifacts'
    artifacts.mkdir()
    if spec['world'] in ('disk-crash', 'disk-ext4-crash'):
        build = ['nice', 'bash', 'guest/build-disk.sh', str(artifacts / 'disk.cpio')]
        if any(machine.get('disk', {}).get('image') for machine in world['machines']):
            build.append(str(artifacts / 'ext4.img'))
        commands.run('guest-build', build)
        initrd = artifacts / 'disk.cpio'
    elif spec['world'] == 'cluster-fixed':
        client = artifacts / 'client'
        commands.run('client-build', ['nice', 'bash', 'guest/build-client.sh', str(client)])
        initrd = artifacts / 'cluster.cpio'
        commands.run('guest-build', ['nice', 'bash', 'guest/build-initramfs.sh', str(initrd),
                                    'guest/cluster.init', job['etcd'] + ':bin/etcd', str(client) + ':bin/client'])
    else:
        initrd = artifacts / 'net.cpio'
        commands.run('guest-build', ['nice', 'bash', 'guest/build-initramfs.sh', str(initrd), 'guest/net.init'])
    world['kernel'] = job['kernel']
    for machine in world['machines']:
        machine['initrd'] = str(initrd)
        if machine.get('disk', {}).get('image'):
            machine['disk']['image'] = str(artifacts / 'ext4.img')
    config = root / 'world.json'
    config.write_text(json.dumps(world, indent=2) + '\n')
    env = dict(commands.env, SEEDBOX_ROOT=str(root), CPU0=str(job['pair'][0]),
               VMM=job['engine'], SEEDBOX_TIMEOUT='120', SEEDBOX_UNDO='256', SEEDBOX_MARGIN='adaptive')

    def run(name, seed, extra=()):
        commands.run(name, ['bash', str(HERE / 'batch.sh'), str(config), name, str(seed), '--digest', *extra], env)
        output = root / 'runs' / f'{name}-s{seed}-r0'
        if Path(str(output) + '.status').read_text().strip() != '0':
            raise RuntimeError('fixture timed out or engine failed')
        return json.loads(Path(str(output) + '.json').read_text())

    first = run('fixed', 1009)
    if spec['world'] == 'cluster-fixed':
        env['VMM'] = job['main_engine']
        main = run('main', 1009)
        # Engine versions can legitimately differ. Retain and report the comparison.
        def evidence(report):
            return {key: report.get(key) for key in ('outcome', 'fault_error', 'world_witness')} | {
                'machines': [{key: machine.get(key) for key in
                    ('name', 'guest_exit_code', 'guest_init_error', 'infrastructure_error', 'state_digest', 'witness')}
                    for machine in report['machines']]}
        candidate, baseline = evidence(first), evidence(main)
        result = dict(status='MATCH' if candidate == baseline else 'DIFFERENT', seed=1009,
                      main_revision=job['main_revision'], candidate=candidate, main=baseline)
        (root / 'cluster-comparison.json').write_text(json.dumps(result, indent=2) + '\n')
        print('cluster-fixed: ' + result['status'], flush=True)
        fixture_evidence(main, world)
    fixture_evidence(first, world)


def run_smoke(job, commands):
    command = ['sudo', '-n', sys.executable, str(HERE / 'smoke/amd.py'),
               '--cli', job['cli'], '--engine', job['engine'], '--kernel', job['kernel'],
               '--guest-init', job['guest_init'], '--busybox', job['busybox'],
               '--cpu', str(job['pair'][0]), '--tool-path', commands.env['PATH'],
               '--output', str(commands.root / 'smoke')]
    if job.get('pinned_engine'):
        command += ['--allow-dev-engine', '--pinned-engine', job['pinned_engine']]
    if 'perf-replay' in job['checks']:
        command += ['--perf-replay']
    project = commands.root / 'project'
    commands.run('project-generate', [sys.executable, 'examples/faults/generate.py', str(project)])
    project /= 'fixed'
    command += ['--project', str(project)]
    commands.run('smoke', command)


def main():
    if sys.platform != 'linux':
        raise RuntimeError('host checks require Linux; use --plan on this host')
    job = json.loads(Path(sys.argv[1]).read_text())
    if tuple(job['pair']) not in read_pairs():
        raise RuntimeError('worker pair is no longer eligible; CPU 0/16 are never used')
    sys.path.insert(0, str(HERE / 'smoke'))
    from amd import pinned_workloads
    pinned_workloads(job['pair'])
    root = Path(job['output'])
    root.mkdir()
    env = dict(os.environ)
    env.pop('CARGO_TARGET_DIR', None)
    env.pop('SEEDBOX_CPU', None)
    env.update(SEEDBOX_UNDO='256', SEEDBOX_MARGIN='adaptive')
    commands = Commands(root, Path(job['src']), env)
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, commands.interrupt)
    began = time.monotonic()
    result = dict(checks=job['checks'], pair=job['pair'], status='FAIL')
    try:
        probe_canonical(job['pair'])
        if job['spec']['kind'] == 'fixture':
            run_fixture(job, commands)
        elif job['spec']['kind'] == 'compose':
            commands.run('compose', ['sudo', '-n', sys.executable, str(HERE / 'host_compose.py'), sys.argv[1]])
        else:
            run_smoke(job, commands)
        result['status'] = 'PASS'
    except BaseException as error:
        result['error'] = repr(error)
    finally:
        result['wall_seconds'] = time.monotonic() - began
        result['phase_wall_seconds'] = {p.name.removesuffix('.wall.json'): json.loads(p.read_text())['wall_seconds']
                                       for p in root.glob('*.wall.json')}
        # The privileged smoke directory is private; phases are also printed to
        # our user-owned log on both success and failure.
        for name in ('smoke', 'compose'):
            log = root / (name + '.log')
            if log.exists():
                result['phase_wall_seconds'].update({phase: float(seconds) for phase, seconds in
                    re.findall(r'^phase (.+): ([0-9.]+)s$', log.read_text(), re.M)})
        comparison = root / 'cluster-comparison.json'
        if comparison.exists():
            result['cluster_comparison'] = json.loads(comparison.read_text())
        (root / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    return 0 if result['status'] == 'PASS' else 1


if __name__ == '__main__':
    sys.exit(main())
