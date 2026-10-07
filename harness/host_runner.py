#!/usr/bin/env python3
"""Runner coordination and bounded PR comments; portable controls use fake leases."""
import argparse
from contextlib import ExitStack, contextmanager
import fcntl
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import time


class CapacityBusy(RuntimeError):
    pass


def cpulist(text):
    result = set()
    for part in text.strip().split(','):
        bounds = list(map(int, part.split('-')))
        result.update(range(bounds[0], bounds[-1] + 1))
    return result


def eligible_pairs(sysfs, allowed):
    """Fail closed on topology drift; exclude both threads of the boot core."""
    online = cpulist((sysfs / 'online').read_text())
    result = []
    for cpu in range(1, 16):
        pair = {cpu, cpu + 16}
        if not pair <= online & set(allowed):
            continue
        if all(cpulist((sysfs / f'cpu{n}/topology/thread_siblings_list').read_text()) == pair
               for n in pair):
            result.append(cpu)
    return result


def unpinned_pairs(pairs, proc=Path('/proc'), affinity=None):
    """Exclude userspace threads whose allowed affinity fits within a pair."""
    affinity = affinity or os.sched_getaffinity
    busy = set()
    for process in proc.iterdir():
        if not process.name.isdecimal():
            continue
        try:
            for thread in (process / 'task').iterdir():
                fields = (thread / 'stat').read_text().rsplit(')', 1)[1].split()
                if int(fields[6]) & 0x00200000:  # PF_KTHREAD, as in the smoke readback.
                    continue
                allowed = affinity(int(thread.name))
                busy.update(cpu for cpu in pairs if allowed and allowed <= {cpu, cpu + 16})
        except (FileNotFoundError, ProcessLookupError):
            continue
    return [cpu for cpu in pairs if cpu not in busy]


@contextmanager
def locked(path, create=False):
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
    if create:
        flags = (flags & ~os.O_RDONLY) | os.O_RDWR | os.O_CREAT
    fd = os.open(path, flags, 0o600)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise RuntimeError(f'nonregular lease: {path}')
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise CapacityBusy(str(path)) from error
        yield fd
    finally:
        os.close(fd)


@contextmanager
def reserve(directory, cap, pairs, canonical=Path('/tmp')):
    """Hold a capacity slot and coordinator pair lock, not Seedbox's own locks."""
    if not 1 <= cap <= 15:
        raise ValueError('cap must be between 1 and 15')
    directory.mkdir(parents=True, exist_ok=True)
    with ExitStack() as slot:
        for index in range(cap):
            try:
                slot.enter_context(locked(directory / f'slot-{index}', True))
                break
            except CapacityBusy:
                continue
        else:
            raise CapacityBusy('all capacity slots are occupied')
        for cpu in pairs:
            if not 1 <= cpu <= 15:
                raise ValueError('pair must be N/N+16 with N between 1 and 15')
            with ExitStack() as pair:
                try:
                    pair.enter_context(locked(directory / f'pair-{cpu}', True))
                    # Probe canonical locks together; CLI acquires them later.
                    with ExitStack() as probe:
                        # The installed smoke driver keeps this guard for its
                        # lifetime even if its unprivileged coordinator dies.
                        probe.enter_context(locked(directory / f'active-{cpu}'))
                        for sibling in (cpu, cpu + 16):
                            probe.enter_context(locked(canonical / f'seedbox-msr-cpu-{sibling}.lock'))
                except (CapacityBusy, FileNotFoundError):
                    continue
                yield cpu
                return
        raise CapacityBusy('no eligible pair has free canonical leases')


def format_comment(summary, sha, run_url, status, mode):
    if not re.fullmatch(r'[0-9a-f]{40}', sha):
        raise ValueError('expected a full commit SHA')
    if not re.fullmatch(r'https://github.com/gardnervickers/seedbox/actions/runs/[0-9]+', run_url):
        raise ValueError('unexpected workflow URL')
    if status not in ('pass', 'fail', 'cancelled') or mode not in ('dev', 'pinned'):
        raise ValueError('unexpected result or engine mode')
    # Strip control bytes and neutralize HTML/mentions. Choose a fence longer
    # than every supplied backtick sequence so guest text cannot escape it.
    text = ''.join(c for c in summary if c in '\n\t' or (ord(c) >= 32 and ord(c) != 127))
    text = text.replace('<', '&lt;').replace('>', '&gt;').replace('@', '@\u200b')
    raw = text.encode('utf-8', errors='replace')
    if len(raw) > 48000:
        text = raw[:48000].decode('utf-8', errors='ignore') + '\n[summary truncated; see linked workflow log]\n'
    fence = '`' * max(3, 1 + max((len(m[0]) for m in re.finditer(r'`+', text)), default=0))
    # Avoid pathological fences consuming the GitHub comment size limit.
    if len(fence) > 100:
        text = text.replace('`', "'")
        fence = '```'
    qualification = 'dev-unpinned (unqualified)' if mode == 'dev' else 'stored pinned engine'
    return (f'<!-- seedbox-host-check {sha} -->\n'
            f'Host check **{status.upper()}** for `{sha}` — {qualification}.\n\n'
            f'[Workflow logs]({run_url}) (summary quoted below; cleanup errors, if any, appear in the receipt).\n\n'
            f'{fence}text\n{text.rstrip()}\n{fence}\n')


def run(args):
    repo = Path.cwd()
    jobs = Path(os.environ['HOST_RUNNER_JOBS'])
    locks = Path(os.environ['HOST_RUNNER_LOCKS'])
    cap = int(os.environ['HOST_RUNNER_CAP'])
    wrapper = os.environ['HOST_CHECK_CPU_WRAPPER']
    summary = args.summary.resolve()
    summary.write_text(f'host-check {args.sha}\nsetup did not complete\n')
    result = args.result.resolve()
    value = dict(sha=args.sha, mode=args.mode, status='fail')
    job = None
    child = None
    def interrupted(sig, frame):
        raise KeyboardInterrupt(f'runner signal {sig}')
    old = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        # Wait for capacity without retrying a guest campaign. GitHub services
        # normally supply the cap, but shared locks also cover local invocations.
        while True:
            stack = ExitStack()
            try:
                pairs = eligible_pairs(Path('/sys/devices/system/cpu'), os.sched_getaffinity(0))
                cpu = stack.enter_context(reserve(locks, cap, unpinned_pairs(pairs)))
                break
            except CapacityBusy:
                stack.close()
                time.sleep(2)
            except BaseException:
                stack.close()
                raise
        with stack:
            import tempfile
            job = Path(tempfile.mkdtemp(prefix='job-', dir=jobs))
            env = os.environ.copy()
            env['HOST_CHECK_DIR'] = str(job)
            env['HOST_CHECK_SUMMARY'] = str(summary)
            env.pop('SEEDBOX_VMM', None)
            env.pop('PINNED_ENGINE', None)
            if args.mode == 'dev':
                env['PINNED_ENGINE'] = os.environ['HOST_RUNNER_PINNED_ENGINE']
            else:
                env['SEEDBOX_VMM'] = os.environ['HOST_RUNNER_PINNED_ENGINE']
            blocked = signal.pthread_sigmask(signal.SIG_BLOCK, old)
            try:
                child = subprocess.Popen(['bash', 'harness/host-check.sh', args.sha, str(cpu)],
                                         env=env, start_new_session=True)
                # Register the child before pending cancellation is delivered.
                signal.pthread_sigmask(signal.SIG_SETMASK, blocked)
                value['status'] = 'pass' if child.wait() == 0 else 'fail'
            finally:
                # Ignore repeated runner signals while cleaning. The privileged
                # wrapper stops its systemd cgroup before deleting any files.
                for sig in old:
                    signal.signal(sig, signal.SIG_IGN)
                signal.pthread_sigmask(signal.SIG_SETMASK, blocked)
                if child is not None and child.poll() is None:
                    os.killpg(child.pid, signal.SIGTERM)
                    try:
                        child.wait(timeout=45)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid, signal.SIGKILL)
                        child.wait()
                subprocess.run(['sudo', '-n', wrapper, 'clean', str(job)], check=True)
                subprocess.run(['git', '-C', str(repo), 'worktree', 'prune'], check=True)
                job = None
    except KeyboardInterrupt:
        value['status'] = 'cancelled'
    except Exception as error:
        with summary.open('a') as out:
            out.write(f'coordinator error: {error}\n')
        value['status'] = 'fail'
    finally:
        # Covers failure before child spawn too. Do not unlink coordinator or
        # canonical lock files: an open descriptor must always name one inode.
        for sig in old:
            signal.signal(sig, signal.SIG_IGN)
        if job is not None:
            clean = subprocess.run(['sudo', '-n', wrapper, 'clean', str(job)])
            prune = subprocess.run(['git', '-C', str(repo), 'worktree', 'prune'])
            if clean.returncode or prune.returncode:
                value['status'] = 'fail'
                with summary.open('a') as out:
                    out.write('cleanup failed; host operator must inspect job ' + str(job) + '\n')
        result.write_text(json.dumps(value) + '\n')
        for sig, handler in old.items():
            signal.signal(sig, handler)
    return 0 if value['status'] == 'pass' else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    check = sub.add_parser('run')
    check.add_argument('sha', type=lambda s: s if re.fullmatch(r'[0-9a-f]{40}', s) else parser.error('invalid SHA'))
    check.add_argument('--mode', choices=('dev', 'pinned'), required=True)
    check.add_argument('--summary', type=Path, required=True)
    check.add_argument('--result', type=Path, required=True)
    comment = sub.add_parser('comment')
    comment.add_argument('--summary', type=Path, required=True)
    comment.add_argument('--result', type=Path, required=True)
    comment.add_argument('--run-url', required=True)
    args = parser.parse_args()
    if args.command == 'run':
        return run(args)
    value = json.loads(args.result.read_text())
    print(format_comment(args.summary.read_text(errors='replace'), value['sha'], args.run_url,
                         value['status'], value['mode']), end='')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
