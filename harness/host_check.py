#!/usr/bin/env python3
"""Plan change-scoped host checks and dispatch them across free SMT pairs."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

from host_plan import format_plan, load_map, plan_revisions
from host_pairs import allocate_pairs, read_pairs
from host_worker import Commands

HERE = Path(__file__).resolve().parent


def jobs_for(checks, mapping=None):
    mapping = mapping or load_map()
    smoke = [check for check in checks if mapping['checks'][check]['kind'] == 'smoke']
    jobs = []
    if smoke:
        if not {'fixed', 'default-replay'} <= set(smoke):
            raise ValueError('smoke replays require the fixed baseline and default replay')
        jobs.append(dict(name='core-smoke', checks=smoke, spec=dict(kind='smoke')))
    jobs.extend(dict(name=check, checks=[check], spec=mapping['checks'][check])
                for check in checks if check not in smoke)
    return jobs


def run_jobs(jobs, pairs, launch, cancelled=lambda: False):
    """One active worker per pair, no retry; join owned workers before return."""
    pending, active, finished = list(jobs), {}, []
    failed = False
    try:
        while pending or active:
            if cancelled():
                failed = True
            # Observe failures before assigning another lane.
            for pair, (job, child, started) in list(active.items()):
                code = child.poll()
                if code is not None:
                    finished.append(dict(name=job['name'], checks=job['checks'], pair=list(pair),
                                         exit=code, wall_seconds=time.monotonic() - started))
                    del active[pair]
                    failed = failed or code != 0
            if failed:
                break
            for pair in pairs:
                if pending and pair not in active:
                    job = pending.pop(0)
                    started = time.monotonic()
                    active[pair] = (job, launch(job, pair), started)
            if active:
                time.sleep(.05)
    finally:
        # Never kill a privileged supervisor or release a reservation during cleanup.
        # A wrapper stuck in cleanup keeps this wait and the reservation outstanding.
        for pair, (job, child, started) in active.items():
            if child.poll() is None:
                child.terminate()  # Forward TERM once, then let the existing wrapper clean up.
            code = child.wait()
            finished.append(dict(name=job['name'], checks=job['checks'], pair=list(pair),
                                 exit=code, wall_seconds=time.monotonic() - started))
    return dict(jobs=finished, undispatched=[job['name'] for job in pending],
                status='FAIL' if failed or pending or any(job['exit'] for job in finished) else 'PASS')


def existing(value, name):
    path = Path(value).expanduser().resolve(strict=True)
    if not path.is_file():
        raise ValueError(name + ' must name an existing file')
    return str(path)


def execute(plan, args):
    if sys.platform != 'linux':
        raise RuntimeError('host execution requires the qualified Linux KVM host; use --plan on this Mac')
    kernel = existing(os.environ['SEEDBOX_KERNEL'], 'SEEDBOX_KERNEL')
    busybox = existing(os.environ['SEEDBOX_BUSYBOX'], 'SEEDBOX_BUSYBOX')
    pinned = os.environ.get('PINNED_ENGINE')
    stored = os.environ.get('SEEDBOX_VMM')
    if not pinned and not stored:
        raise ValueError('set PINNED_ENGINE for a rebuilt development engine, or SEEDBOX_VMM for a stored engine')
    if pinned:
        pinned = existing(pinned, 'PINNED_ENGINE')
    if stored:
        stored = existing(stored, 'SEEDBOX_VMM')
    variants = {}
    for item in args.pv_kernel:
        label, sep, path = item.partition('=')
        if not sep or label not in ('abi1', 'abi2') or label in variants:
            raise ValueError('--pv-kernel must be abi1=FILE or abi2=FILE, once per label')
        variants[label] = existing(path, '--pv-kernel')
    selected = jobs_for(plan['checks'])
    required = {job['spec']['kernel_variant'] for job in selected if 'kernel_variant' in job['spec']}
    if not required <= variants.keys():
        raise ValueError('timebase checks require --pv-kernel abi1=FILE and --pv-kernel abi2=FILE; no kernel is built')
    if required:
        import hashlib
        if len({hashlib.sha256(Path(variants[label]).read_bytes()).digest() for label in required}) != len(required):
            raise ValueError('PV variants must have distinct frozen kernel bytes')
    # Import the existing smoke readiness/lock wrapper only after host admission.
    sys.path.insert(0, str(HERE / 'smoke'))
    from amd import lock_free, pinned_workloads
    def idle(pair):
        try:
            pinned_workloads(pair)
        except RuntimeError as error:
            if str(error) == 'userspace workload pinned to the selected pair':
                raise BlockingIOError(str(error)) from error
            raise
    with allocate_pairs(read_pairs(), args.pairs, probe=lock_free, idle=idle) as pairs:
        work = Path(tempfile.mkdtemp(prefix='host-check-' + plan['target'][:7] + '-',
                                     dir=os.environ.get('HOST_CHECK_DIR', tempfile.gettempdir())))
        (work / 'plan.json').write_text(json.dumps(plan, indent=2) + '\n')
        src = work / 'src'
        env = dict(os.environ)
        env.pop('CARGO_TARGET_DIR', None)
        source = Commands(work, args.repo, env)
        old_handlers = {sig: signal.signal(sig, source.interrupt) for sig in (signal.SIGINT, signal.SIGTERM)}
        summary = dict(status='FAIL', target=plan['target'], pairs=pairs, jobs=[], undispatched=[])
        try:
            source.run('worktree', ['git', 'worktree', 'add', '--detach', '--quiet', str(src), plan['target']])
            source.src = src
            print('evidence: ' + str(work), flush=True)
            for name, command in [
                    ('build', ['nice', 'cargo', 'build', '--release', '--locked', '-p', 'seedbox']),
                    ('fmt', ['cargo', 'fmt', '-p', 'seedbox', '--check']),
                    ('clippy', ['nice', 'cargo', 'clippy', '-p', 'seedbox', '--all-targets', '--locked', '--', '-D', 'warnings']),
                    ('workspace', ['nice', 'cargo', 'test', '--workspace', '--locked']),
                    ('portable', ['nice', sys.executable, '-m', 'unittest', 'discover', '-s', str(HERE / 'tests'), '-q'])]:
                source.run(name, command)
                print(f'phase {name}: {json.loads((work / (name + ".wall.json")).read_text())["wall_seconds"]:.1f}s', flush=True)
            engine = stored or str(src / 'crates/seedbox-vmm/target/release/seedbox-vmm')
            if not stored:
                source.run('vmm-build', ['nice', 'cargo', 'build-vmm'])
            init = work / 'compose-init'
            source.run('guest-init', ['bash', 'guest/build-compose.sh', str(init)])
            common = dict(src=str(src), cli=str(src / 'target/release/seedbox'), engine=engine,
                          kernel=kernel, busybox=busybox, pinned_engine=pinned, guest_init=str(init))
            # Raw batch fixtures must not race a core smoke admission refusal.
            # Doctor admits every assigned lane before any guest is dispatched.
            for pair in pairs:
                doctor = ['sudo', '-n', common['cli'], 'doctor', '--kernel', kernel,
                          '--vmm', engine, '--cpu', str(pair[0])]
                if pinned:
                    doctor += ['--allow-dev-engine']
                source.run(f'doctor-cpu{pair[0]}', doctor)

            def launch(job, pair):
                data = dict(common, **job, pair=list(pair), output=str(work / job['name']))
                if 'kernel_variant' in job['spec']:
                    data['kernel'] = variants[job['spec']['kernel_variant']]
                path = work / (job['name'] + '.job.json')
                path.write_text(json.dumps(data, indent=2) + '\n')
                with (work / (job['name'] + '.log')).open('x') as log:
                    return subprocess.Popen([sys.executable, str(HERE / 'host_worker.py'), str(path)],
                                            env=env, stdout=log, stderr=subprocess.STDOUT)

            summary.update(run_jobs(selected, pairs, launch, lambda: source.cancelled))
        except BaseException as error:
            summary['error'] = repr(error)
            raise
        finally:
            for sig, handler in old_handlers.items():
                signal.signal(sig, handler)
            summary['source_wall_seconds'] = {p.name.removesuffix('.wall.json'): json.loads(p.read_text())['wall_seconds']
                                               for p in work.glob('*.wall.json')}
            summary['checks'] = {check: dict(status='NOT_RUN', wall_seconds=None, reasons=reasons)
                                 for check, reasons in plan['checks'].items()}
            for job in summary['jobs']:
                receipt = work / job['name'] / 'result.json'
                if receipt.exists():
                    job['receipt'] = json.loads(receipt.read_text())
                for check in job['checks']:
                    phase = {'default-replay': 'default'}.get(check, check)
                    phases = job.get('receipt', {}).get('phase_wall_seconds', {})
                    seconds = phases.get(phase) if job['name'] == 'core-smoke' else job['wall_seconds']
                    status = ('PASS' if job['exit'] == 0 else 'FAIL') if seconds is not None else 'NOT_RUN'
                    summary['checks'][check].update(status=status, wall_seconds=seconds, pair=job['pair'])
                    elapsed = 'not run' if seconds is None else f'{seconds:.1f}s'
                    print(f"check {check}: {status} {elapsed} cpu {job['pair']}", flush=True)
            (work / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
            print(summary['status'] + ': ' + str(work / 'summary.json'), flush=True)
    return 0 if summary['status'] == 'PASS' else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('revisions', nargs='*', help='SHAs/revisions in train order; execute the union at the last revision.')
    parser.add_argument('--train', help='Train branch; plan its merge-base diff and check its tip.')
    parser.add_argument('--base', default='origin/main', help='Local base ref for merge-base diffs [default: origin/main].')
    parser.add_argument('--plan', action='store_true', help='Print selected checks and matching paths; no host work.')
    parser.add_argument('--pairs', type=int, default=1, choices=range(1, 16), metavar='N', help='Number of free SMT pairs (1..15) [default: 1].')
    parser.add_argument('--pv-kernel', action='append', default=[], metavar='ABI=FILE', help='Frozen PV variant kernel: abi1=FILE and abi2=FILE (timebase checks only).')
    args = parser.parse_args(argv)
    if args.train and args.revisions:
        parser.error('use a revision list or --train, not both')
    revisions = [args.train] if args.train else (args.revisions or ['HEAD'])
    try:
        args.repo = Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], text=True).strip())
        plan = plan_revisions(args.repo, revisions, args.base)
        print(format_plan(plan), flush=True)
        if args.plan or not plan['checks']:
            return 0
        return execute(plan, args)
    except (OSError, ValueError, RuntimeError, KeyError, subprocess.CalledProcessError) as error:
        print('error: ' + str(error), file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
