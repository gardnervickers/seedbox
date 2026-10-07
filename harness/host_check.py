#!/usr/bin/env python3
"""Plan change-scoped host checks and dispatch them across free SMT pairs."""
import argparse
import json
import os
from pathlib import Path
import signal
import shlex
import subprocess
import sys
import tempfile
import time

from host_plan import format_plan, load_map, plan_revision, git
from host_pairs import allocate_pairs, read_pairs
from host_worker import CheckFailure, Commands

HERE = Path(__file__).resolve().parent


def jobs_for(checks, mapping=None):
    mapping = mapping or load_map()
    smoke = [check for check in checks if mapping['checks'][check]['kind'] == 'smoke']
    jobs = []
    if smoke:
        if 'core' not in smoke:
            raise ValueError('performance replay requires the core baseline')
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


def kernels_for(jobs):
    """Resolve every selected kernel before reservations, builds or state."""
    kernels = {}
    if any(job['spec'].get('kernel') == 'disk' for job in jobs):
        value = os.environ.get('SEEDBOX_DISK_KERNEL')
        if not value:
            raise ValueError('disk checks planned; set SEEDBOX_DISK_KERNEL to a kernel with VIRTIO_BLK and EXT4_FS')
        try:
            kernels['disk'] = existing(value, 'SEEDBOX_DISK_KERNEL')
        except (OSError, ValueError) as error:
            raise ValueError(f'SEEDBOX_DISK_KERNEL {value}: must name an existing kernel with VIRTIO_BLK and EXT4_FS') from error
    kernels['default'] = existing(os.environ['SEEDBOX_KERNEL'], 'SEEDBOX_KERNEL')
    return kernels


def execute(plan, args):
    if sys.platform != 'linux':
        raise RuntimeError('host execution requires the qualified Linux KVM host; use --plan on this Mac')
    selected = jobs_for(plan['checks'])
    kernels = kernels_for(selected)
    kernel = kernels['default']
    for job in selected:
        job['kernel'] = kernels[job['spec'].get('kernel', 'default')]
        if job['name'] == 'volume-crash':
            value = os.environ.get('SEEDBOX_MKE2FS')
            if not value:
                raise ValueError('volume-crash planned; set SEEDBOX_MKE2FS from the project development shell')
            try:
                job['mke2fs'] = existing(value, 'SEEDBOX_MKE2FS')
            except (OSError, ValueError) as error:
                raise ValueError(f'SEEDBOX_MKE2FS {value}: must name an existing static mke2fs from the project development shell') from error
    busybox = existing(os.environ['SEEDBOX_BUSYBOX'], 'SEEDBOX_BUSYBOX')
    pinned = os.environ.get('PINNED_ENGINE')
    stored = os.environ.get('SEEDBOX_VMM')
    if not pinned and not stored:
        raise ValueError('set PINNED_ENGINE for a rebuilt development engine, or SEEDBOX_VMM for a stored engine')
    if pinned:
        pinned = existing(pinned, 'PINNED_ENGINE')
    if stored:
        stored = existing(stored, 'SEEDBOX_VMM')
    cluster = 'cluster-fixed' in plan['checks']
    etcd = existing(os.environ['SEEDBOX_ETCD'], 'SEEDBOX_ETCD') if cluster else None
    main_sha = (plan.get('main') or git(args.repo, 'rev-parse', '--verify', 'origin/main^{commit}').decode().strip()) if cluster else None
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
    available = read_pairs()
    requested = [(cpu, cpu + 16) for cpu in args.lanes]
    if any(pair not in available for pair in requested):
        raise ValueError('--lanes: every requested N,N+16 pair must be online, allowed and reciprocal')
    with allocate_pairs(requested, len(requested), probe=lock_free, idle=idle) as pairs:
        work = Path(tempfile.mkdtemp(prefix='host-check-' + plan['target'][:7] + '-',
                                     dir=os.environ.get('HOST_CHECK_DIR', tempfile.gettempdir())))
        (work / 'plan.json').write_text(json.dumps(plan, indent=2) + '\n')
        src = work / 'src'
        env = dict(os.environ)
        env.pop('CARGO_TARGET_DIR', None)
        source = Commands(work, args.repo, env)
        old_handlers = {sig: signal.signal(sig, source.interrupt) for sig in (signal.SIGINT, signal.SIGTERM)}
        summary = dict(status='FAIL', target=plan['target'], pairs=pairs, jobs=[], undispatched=[],
                       reviewer_notes=plan.get('reviewer_notes', []), kernels=kernels)
        try:
            source.run('worktree', ['git', 'worktree', 'add', '--detach', '--quiet', str(src), plan['target']])
            source.src = src
            print('evidence: ' + str(work), flush=True)
            source.run('build', ['nice', 'cargo', 'build', '--release', '--locked', '-p', 'seedbox'])
            engine = stored or str(src / 'crates/seedbox-vmm/target/release/seedbox-vmm')
            if not stored:
                source.run('vmm-build', ['nice', 'cargo', 'build-vmm'])
            if args.skip_portable:
                source.run('vmm-test', ['nice', 'cargo', 'test-vmm'])
            else:
                source.run('fmt', ['cargo', 'fmt', '-p', 'seedbox', '--check'])
                source.run('clippy', ['nice', 'cargo', 'clippy', '-p', 'seedbox',
                                      '--all-targets', '--locked', '--', '-D', 'warnings'])
                # On Linux the portable suite includes the standalone VMM tests.
                source.run('suite', ['nice', 'bash', 'harness/acceptance.sh', 'portable'])
            init = work / 'compose-init'
            source.run('guest-init', ['nice', 'bash', 'guest/build-compose.sh', str(init)])
            main_engine = None
            if cluster:
                main_src = work / 'main'
                source.run('main-worktree', ['git', 'worktree', 'add', '--detach', '--quiet', str(main_src), main_sha])
                source.src = main_src
                source.run('main-vmm-build', ['nice', 'cargo', 'build-vmm'])
                main_engine = str(main_src / 'crates/seedbox-vmm/target/release/seedbox-vmm')
                source.src = src
            common = dict(src=str(src), cli=str(src / 'target/release/seedbox'), engine=engine,
                          kernel=kernel, busybox=busybox, pinned_engine=pinned, guest_init=str(init),
                          main_engine=main_engine, main_revision=main_sha, etcd=etcd, tool_path=env['PATH'])
            # Raw batch fixtures must not race a core smoke admission refusal.
            # Doctor admits every assigned lane before any guest is dispatched.
            for pair in pairs:
                for kind, selected_kernel in kernels.items():
                    doctor = ['sudo', '-n', common['cli'], 'doctor', '--kernel', selected_kernel,
                              '--vmm', engine, '--cpu', str(pair[0])]
                    if pinned:
                        doctor += ['--allow-dev-engine']
                    name = f'doctor-cpu{pair[0]}' if kind == 'default' else f'doctor-{kind}-cpu{pair[0]}'
                    source.run(name, doctor)

            def launch(job, pair):
                data = dict(common, **job, pair=list(pair), output=str(work / job['name']))
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
            summary['source_steps'] = {p.name.removesuffix('.wall.json'): json.loads(p.read_text())
                                       for p in work.glob('*.wall.json')}
            summary['checks'] = {check: dict(status='NOT_RUN', wall_seconds=None, reasons=reasons)
                                 for check, reasons in plan['checks'].items()}
            for job in summary['jobs']:
                receipt = work / job['name'] / 'result.json'
                if receipt.exists():
                    job['receipt'] = json.loads(receipt.read_text())
                    if 'cluster_comparison' in job['receipt']:
                        print('cluster-fixed: ' + job['receipt']['cluster_comparison']['status'], flush=True)
                for check in job['checks']:
                    phase = {'core': 'fixed', 'perf-replay': 'undo-margin'}.get(check, check)
                    phases = job.get('receipt', {}).get('phase_wall_seconds', {})
                    seconds = job['wall_seconds'] if check == 'core' else (phases.get(phase) if job['name'] == 'core-smoke' else job['wall_seconds'])
                    status = ('PASS' if job['exit'] == 0 else 'FAIL') if seconds is not None else 'NOT_RUN'
                    summary['checks'][check].update(status=status, wall_seconds=seconds, pair=job['pair'])
                    elapsed = 'not run' if seconds is None else f'{seconds:.1f}s'
                    print(f"check {check}: {status} {elapsed} cpu {job['pair']}", flush=True)
            finish(work, args.repo, summary, source)
    return 0 if summary['status'] == 'PASS' else 1


def parse_lanes(text):
    try:
        lanes = [int(item) for item in text.split(',')]
    except ValueError as error:
        raise argparse.ArgumentTypeError('--lanes must be comma-separated pair numbers 1..15') from error
    if not lanes or len(set(lanes)) != len(lanes) or any(not 1 <= cpu <= 15 for cpu in lanes):
        raise argparse.ArgumentTypeError('--lanes must name distinct pairs 1..15; CPU 0 and 16 are reserved')
    return lanes


def parse_cpu(text):
    lanes = parse_lanes(text)
    if len(lanes) != 1:
        raise argparse.ArgumentTypeError('CPU must name one pair number 1..15')
    return lanes[0]


def finish(work, repo, summary, source):
    """Keep summaries; discard owned run state only after all workers passed."""
    summary_path = work / 'summary.json'
    summary_path.write_text(json.dumps(summary, indent=2) + '\n')
    notes = summary.get('reviewer_notes', [])
    suffix = '; not run: ' + '; '.join(notes) if notes else ''
    lines = [note for note in notes]
    for name, seconds in summary['source_wall_seconds'].items():
        status = summary.get('source_steps', {}).get(name, {}).get('status', 'UNKNOWN')
        lines.append(f'{name:<8} {status.lower() if status == "PASS" else status} {int(seconds):4d}s')
    for job in summary['jobs']:
        status = 'pass' if job['exit'] == 0 else 'FAIL'
        lines.append(f"{job['name']:<18} {status} {job['wall_seconds']:6.1f}s cpu {job['pair']}")
        for name, seconds in job.get('receipt', {}).get('phase_wall_seconds', {}).items():
            lines.append(f'phase {name}: {seconds:.1f}s')
    (work / 'summary.txt').write_text('\n'.join(lines) + '\n')
    for line in lines:
        print(line, flush=True)
    cleanup = []
    for name in ('src', 'main'):
        if (work / name).exists():
            cleanup.append(shlex.join(['git', '-C', str(repo), 'worktree', 'remove', '--force', str(work / name)]))
    cleanup.append(shlex.join(['sudo', '-n', 'rm', '-rf', '--', str(work)]))
    if summary['status'] == 'PASS':
        # Receipts survive deletion of the private worktrees and root-owned stores.
        import shutil
        kept = work.with_suffix('.json')
        shutil.copyfile(summary_path, kept)
        shutil.copyfile(work / 'summary.txt', work.with_suffix('.txt'))
        try:
            source.src = repo
            for name in ('src', 'main'):
                if (work / name).exists():
                    source.run('remove-' + name, ['git', '-C', str(repo), 'worktree', 'remove', '--force', str(work / name)])
            source.src = repo
            subprocess.run(['sudo', '-n', 'rm', '-rf', '--', str(work)], cwd=repo, check=True)
        except BaseException as error:
            summary.update(status='FAIL', cleanup_error=repr(error))
            kept.write_text(json.dumps(summary, indent=2) + '\n')
            print('cleanup: ' + ' && '.join(cleanup), flush=True)
        print(summary['status'] + ': ' + str(kept) + suffix, flush=True)
    else:
        print('FAIL: ' + str(summary_path) + suffix, flush=True)
        print('cleanup: ' + ' && '.join(cleanup), flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('revision', nargs='?', default='HEAD', metavar='REV', help='Revision to check [default: HEAD].')
    parser.add_argument('cpu', nargs='?', type=parse_cpu, metavar='CPU',
                        help='Use one SMT pair N,N+16 (shorthand for --lanes N).')
    parser.add_argument('--skip-portable', action='store_true',
                        help='Skip fmt, clippy and the portable suite after a Linux pre-check at the same SHA; keep VMM tests.')
    parser.add_argument('--base', default='origin/main', help='Base ref for BASE...REV [default: origin/main].')
    parser.add_argument('--plan', action='store_true', help='Print selected checks and matching paths, then exit.')
    parser.add_argument('--extra', action='append', default=[], metavar='CHECK', help='Add a reviewer check (repeatable).')
    parser.add_argument('--lanes', type=parse_lanes, metavar='1,2,3', help='Use exactly these SMT pairs N,N+16 [default: 1].')
    args = parser.parse_args(argv)
    if args.cpu is not None and args.lanes is not None:
        parser.error('CPU and --lanes cannot be used together')
    args.lanes = args.lanes if args.lanes is not None else [args.cpu if args.cpu is not None else 1]
    try:
        args.repo = Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], text=True).strip())
        plan = plan_revision(args.repo, args.base, args.revision, args.extra)
        print(format_plan(plan), flush=True)
        if args.plan:
            return 0
        if plan['warnings']:
            raise RuntimeError('kernel campaign needed; this plan does not cover kernel changes')
        if not plan['checks']:
            return 0
        return execute(plan, args)
    except CheckFailure as error:
        print('error: ' + str(error), file=sys.stderr)
        return 1
    except (OSError, ValueError, RuntimeError, KeyError, subprocess.CalledProcessError) as error:
        print('error: ' + str(error), file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
