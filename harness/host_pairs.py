"""SMT allocation with coordinator reservations and existing canonical leases.

Canonical locks are probed, then released for the CLI/batch child to own. Separate
coordinator flocks prevent two host-check invocations assigning the same lane
between child commands. Actual execution still takes the shared canonical locks.
"""
import contextlib
import fcntl
import os
from pathlib import Path
import stat

from batch import cpu_list


def eligible_pairs(topology, online, allowed):
    zero = set(topology.get(0, {0, 16})) | {0, 16}
    seen, pairs = set(), []
    for cpu in sorted(topology):
        pair = frozenset(topology[cpu])
        if pair in seen:
            continue
        seen.add(pair)
        if len(pair) != 2 or cpu not in pair or pair & zero:
            continue
        if not pair <= online or not pair <= allowed:
            continue
        if any(set(topology.get(sibling, ())) != set(pair) for sibling in pair):
            continue
        pairs.append(tuple(sorted(pair)))
    return pairs


def read_pairs(root=Path('/sys/devices/system/cpu'), allowed=None):
    online = cpu_list((root / 'online').read_text())
    topology = {cpu: cpu_list((root / f'cpu{cpu}/topology/thread_siblings_list').read_text()) for cpu in online}
    return eligible_pairs(topology, online, os.sched_getaffinity(0) if allowed is None else allowed)


def probe_canonical(pair, directory=Path('/tmp')):
    # The smoke wrapper requires existing qualification-owned regular leases.
    # Never create/unlink canonical leases merely to discover free capacity.
    with contextlib.ExitStack() as stack:
        for cpu in sorted(pair):
            fd = os.open(directory / f'seedbox-msr-cpu-{cpu}.lock', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            stream = stack.enter_context(os.fdopen(fd, 'r'))
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise RuntimeError('canonical lease is not regular')
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)


@contextlib.contextmanager
def coordinator_lock(pair, directory=Path('/tmp')):
    with contextlib.ExitStack() as stack:
        for cpu in sorted(pair):
            path = directory / f'seedbox-host-check-cpu-{cpu}.lock'
            try:
                fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            except FileNotFoundError:
                try:
                    fd = os.open(path, os.O_RDONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
                except FileExistsError:
                    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            stream = stack.enter_context(os.fdopen(fd, 'r'))
            if not stat.S_ISREG(os.fstat(fd).st_mode):
                raise RuntimeError('coordinator lease is not regular')
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield


@contextlib.contextmanager
def allocate_pairs(pairs, count, directory=Path('/tmp'), probe=None, idle=None):
    if not 1 <= count <= 15:
        raise ValueError('--lanes must request 1..15 pairs')
    probe = probe or (lambda pair: probe_canonical(pair, directory))
    # idle is smoke.pinned_workloads on the real host, injectable for portable tests.
    idle = idle or (lambda pair: None)
    with contextlib.ExitStack() as held:
        selected = []
        for pair in pairs:
            if set(pair) & {0, 16} or any(set(pair) & set(other) for other in selected):
                continue
            candidate = contextlib.ExitStack()
            try:
                candidate.enter_context(coordinator_lock(pair, directory))
                probe(pair)
                idle(pair)
            except BlockingIOError:
                candidate.close()
                continue
            except BaseException:
                candidate.close()
                raise
            held.enter_context(candidate)
            selected.append(pair)
            if len(selected) == count:
                yield selected
                return
        raise RuntimeError(f'--lanes ({count} requested pairs): only {len(selected)} free eligible SMT pairs; no checks dispatched')
