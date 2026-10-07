#!/usr/bin/env python3
"""Root supervisor: directly owns VMM children and locks through MSR restore."""
import ctypes
import contextlib
import fcntl
import math
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

import msr54


class Interrupted(Exception):
    def __init__(self, signum):
        self.signum = signum


def cpu_list(value):
    cpus = set()
    for item in value.strip().split(','):
        bounds = [int(v) for v in item.split('-')]
        if len(bounds) == 1:
            cpus.add(bounds[0])
        elif len(bounds) == 2 and bounds[0] <= bounds[1]:
            cpus.update(range(bounds[0], bounds[1] + 1))
        else:
            raise ValueError('invalid CPU list')
    return cpus


def siblings(cpu):
    return cpu_list(Path(f'/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list').read_text())


@contextlib.contextmanager
def cpu_locks(cpus, directory=Path('/tmp')):
    with contextlib.ExitStack() as stack:
        for cpu in sorted(cpus):
            # Never follow a user-created symlink when the supervisor runs as root.
            path = directory / f'seedbox-msr-cpu-{cpu}.lock'
            try:
                fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
            except FileNotFoundError:
                try:
                    fd = os.open(path, os.O_RDONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
                except FileExistsError:
                    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
            stream = stack.enter_context(os.fdopen(fd, 'r'))
            try:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise RuntimeError(f'cpu {cpu} is already owned') from error
        yield


def enable_subreaper():
    if sys.platform == 'linux':
        # Adopt orphaned descendants so cleanup can wait for the whole group.
        libc = ctypes.CDLL(None, use_errno=True)
        if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
            raise OSError(ctypes.get_errno(), 'cannot become child subreaper')


def exit_status(child):
    # WNOWAIT keeps the leader's PID reserved until the final group signal.
    observed = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    if observed is None:
        return None
    return observed.si_status if observed.si_code == os.CLD_EXITED else -observed.si_status


def stop_owned_group(child):
    if child.returncode is not None:
        # A completed cleanup must never signal a potentially reused PGID.
        return
    try:
        os.killpg(child.pid, signal.SIGTERM)
    except ProcessLookupError:
        child.wait()
        return
    deadline = time.monotonic() + 2
    while exit_status(child) is None and time.monotonic() < deadline:
        time.sleep(.01)
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    # Reap only after the last signal. Blocking is deliberate: never restore
    # while an owned child remains alive, even if SIGKILL cannot reap promptly.
    child.wait()
    while True:
        try:
            os.waitpid(-child.pid, 0)
        except InterruptedError:
            continue
        except ChildProcessError:
            break


def run_batch(root, world, prefix, seeds, extra, cpus, timeout, vmm, margin,
              lock_dir=Path('/tmp')):
    if not seeds or len(cpus) != len(seeds) or not math.isfinite(timeout) or timeout <= 0:
        raise ValueError('expected nonempty seeds and positive timeout')
    if not hasattr(os, "WNOWAIT"):
        raise RuntimeError("Linux waitid/WNOWAIT is required")
    enable_subreaper()
    protected = set().union(*(siblings(cpu) for cpu in cpus))
    outputs = [root / 'runs' / f'{prefix}-s{seed}-r{i}' for i, seed in enumerate(seeds)]
    for out in outputs:
        for path in (out, *(Path(str(out) + suffix) for suffix in ('.json', '.err', '.status'))):
            if path.exists():
                raise FileExistsError(f'refusing to overwrite {path}')
    children = []
    result = 0
    snapshot = None
    pending_signal = None
    def interrupted(signum, _frame):
        nonlocal pending_signal
        pending_signal = signum
    def check_signal():
        if pending_signal is not None:
            raise Interrupted(pending_signal)
    signals = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)
    previous = {sig: signal.signal(sig, interrupted) for sig in signals}
    try:
        with cpu_locks(protected, lock_dir):
            state_dir = Path(tempfile.mkdtemp(prefix='seedbox-msr-', dir=lock_dir))
            snapshot = state_dir / 'saved.json'
            try:
                check_signal()
                if msr54.required():
                    msr54.show(sorted(protected))
                    msr54.set_workaround(sorted(protected), snapshot)
                for cpu, seed, out in zip(cpus, seeds, outputs):
                    check_signal()
                    out.parent.mkdir(parents=True, exist_ok=True)
                    with Path(str(out)+'.json').open('x') as stdout, Path(str(out)+'.err').open('x') as stderr:
                        child = subprocess.Popen(
                            ['taskset', '-c', str(cpu), vmm, '--config', world,
                             '--seed', str(seed), '--out', str(out), *extra],
                            cwd=root, stdout=stdout, stderr=stderr,
                            env={**os.environ, 'SEEDBOX_MARGIN': margin},
                            start_new_session=True)
                    children.append((child, out, time.monotonic(), None))
                check_signal()
                while any(item[3] is None for item in children):
                    check_signal()
                    for i, (child, out, started, status) in enumerate(children):
                        if status is not None:
                            continue
                        status = exit_status(child)
                        if status is None and time.monotonic() - started >= timeout:
                            status = 124
                        if status is not None:
                            # Also remove any descendant before recording completion.
                            stop_owned_group(child)
                            children[i] = (child, out, started, status)
                            Path(str(out)+'.status').write_text(f'{status}\n')
                    time.sleep(.01)
            except Interrupted as error:
                result = 128 + error.signum
            except Exception as error:
                print(f'batch: {error}', file=sys.stderr)
                result = 1
            finally:
                # Cleanup cannot itself be interrupted between reap and restore.
                for sig in signals:
                    signal.signal(sig, signal.SIG_IGN)
                for child, out, _, status in children:
                    if status is None:
                        stop_owned_group(child)
                        Path(str(out)+'.status').write_text(f'{result or 1}\n')
                if snapshot.exists():
                    try:
                        msr54.show(sorted(protected))
                    except OSError as error:
                        print(f'batch: pre-restore readback: {error}', file=sys.stderr)
                        result = 1
                    try:
                        msr54.restore_snapshot(sorted(protected), snapshot)
                    except (OSError, ValueError) as error:
                        print(f'batch: restoration failed; retained {snapshot}: {error}', file=sys.stderr)
                        result = 1
                if not snapshot.exists():
                    state_dir.rmdir()
    except Interrupted as error:
        result = 128 + error.signum
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)
    return result


def main():
    if os.geteuid() != 0:
        raise ValueError('run through batch.sh (root supervisor required)')
    world, prefix, seed_arg, *extra = sys.argv[1:]
    seeds = [int(seed) for seed in seed_arg.split()]
    cpu0 = int(os.environ.get('CPU0', '2'))
    return run_batch(Path(os.environ['SEEDBOX_ROOT']), world, prefix, seeds,
                     extra, list(range(cpu0, cpu0 + len(seeds))),
                     float(os.environ.get('SEEDBOX_TIMEOUT', '3600')),
                     os.environ.get('VMM', './crates/seedbox-vmm/target/release/seedbox-vmm'),
                     os.environ.get('SEEDBOX_MARGIN', 'adaptive'))


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError) as error:
        sys.exit(f'batch: {error}')
