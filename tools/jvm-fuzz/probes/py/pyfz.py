"""CPython probes: the language of every harness driver, under translation.

signals: interval timers (ITIMER_REAL, ITIMER_VIRTUAL, ITIMER_PROF) during fixed CPU loops;
signal.set_wakeup_fd with paced SIGUSR1 from a thread and from a child, through a raw selector
and through asyncio; multiprocessing pools with the fork and forkserver start methods and
sem_open-backed queues; faulthandler's watchdog dump; subprocess pipelines carrying seeded bytes.

Only normalized predicates and seeded checksums are printed (a timer fired at least once, every
signal produced one wakeup byte and one handler run), never raw counts or tracebacks.

Usage: pyfz signals SEED SCALE
"""

import asyncio
import faulthandler
import hashlib
import multiprocessing as mp
import os
import selectors
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

_digest = hashlib.sha256()


def emit(key, value):
    line = f"{key} {value}"
    print(line, flush=True)
    _digest.update((line + "\n").encode())


def fail(msg):
    print(f"FAIL {msg}", flush=True)
    os._exit(1)


def check(ok, what):
    if not ok:
        fail(what)


def xorshift(x):
    x ^= (x << 13) & 0xFFFFFFFFFFFFFFFF
    x ^= x >> 7
    x ^= (x << 17) & 0xFFFFFFFFFFFFFFFF
    return x


def cpu_loop(n, x=1):
    for i in range(n):
        x = xorshift(x) ^ i
    return x


# ---------------------------------------------------------------------------------------------
# (a) interval timers


def timers(seed, scale):
    counts = {"real": 0, "virtual": 0, "prof": 0}
    kinds = [("real", signal.ITIMER_REAL, signal.SIGALRM),
             ("virtual", signal.ITIMER_VIRTUAL, signal.SIGVTALRM),
             ("prof", signal.ITIMER_PROF, signal.SIGPROF)]
    for name, which, sig in kinds:
        def handler(signum, frame, name=name):
            counts[name] += 1
        old = signal.signal(sig, handler)
        signal.setitimer(which, 0.002, 0.002)
        # A fixed CPU loop of well over one interval of process time (about 0.3 s natively).
        x = cpu_loop(300_000 * scale, seed | 1)
        signal.setitimer(which, 0, 0)
        signal.signal(sig, old)
        check(x != 0, "cpu loop")
        emit(f"itimer.{name}", counts[name] >= 1)
    # ITIMER_REAL during sleeps: one-shot timers each fire exactly once.
    fired = []
    old = signal.signal(signal.SIGALRM, lambda s, f: fired.append(s))
    for k in range(10):
        signal.setitimer(signal.ITIMER_REAL, 0.001 + 0.001 * (k % 3))
        deadline = time.monotonic() + 30
        while len(fired) <= k and time.monotonic() < deadline:
            time.sleep(0.0005)
    signal.signal(signal.SIGALRM, old)
    emit("itimer.oneshot", f"{len(fired)} of 10")


# ---------------------------------------------------------------------------------------------
# (b) wakeup fd


def wakeup_thread(seed, n):
    """n SIGUSR1 sent by a thread, each after the previous one was seen: one byte, one handler."""
    rsock, wsock = socket.socketpair()
    rsock.setblocking(False)
    wsock.setblocking(False)
    handled = [0]
    ack = threading.Semaphore(0)
    old_fd = signal.set_wakeup_fd(wsock.fileno(), warn_on_full_buffer=True)
    old = signal.signal(signal.SIGUSR1, lambda s, f: handled.__setitem__(0, handled[0] + 1))

    def sender():
        for _ in range(n):
            os.kill(os.getpid(), signal.SIGUSR1)
            ack.acquire()

    t = threading.Thread(target=sender)
    sel = selectors.DefaultSelector()
    sel.register(rsock, selectors.EVENT_READ)
    t.start()
    got = bytearray()
    while len(got) < n:
        if not sel.select(timeout=60):
            fail(f"wakeup fd: no byte after {len(got)} of {n}")
        while True:
            try:
                b = rsock.recv(64)
            except BlockingIOError:
                break
            got += b
            for _ in b:
                ack.release()
    t.join()
    # Handlers run in the main thread between bytecodes; give the last one a chance.
    for _ in range(1000):
        if handled[0] >= n:
            break
        time.sleep(0.001)
    signal.signal(signal.SIGUSR1, old)
    signal.set_wakeup_fd(old_fd)
    sel.close()
    rsock.close()
    wsock.close()
    emit("wakeup.thread", f"bytes {len(got)} usr1 {got.count(signal.SIGUSR1)} handled {handled[0]} of {n}")
    check(len(got) == n and got.count(signal.SIGUSR1) == n and handled[0] == n, "wakeup thread")


CHILD_SIGNALER = r"""
import os, sys, signal
n = int(sys.argv[1])
for _ in range(n):
    os.kill(os.getppid(), signal.SIGUSR1)
    if sys.stdin.buffer.read(1) != b"a":
        sys.exit(3)
print("sent", n)
"""


async def wakeup_asyncio(n):
    """asyncio's own wakeup fd: paced SIGUSR1 from a child process, one callback each."""
    loop = asyncio.get_running_loop()
    got = asyncio.Queue()
    loop.add_signal_handler(signal.SIGUSR1, lambda: got.put_nowait(1))
    proc = await asyncio.create_subprocess_exec(sys.executable, "-I", "-c", CHILD_SIGNALER, str(n),
                                                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE)
    for k in range(n):
        try:
            await asyncio.wait_for(got.get(), 60)
        except asyncio.TimeoutError:
            fail(f"asyncio: signal {k} not delivered")
        proc.stdin.write(b"a")
        await proc.stdin.drain()
    out = await proc.stdout.read()
    code = await proc.wait()
    loop.remove_signal_handler(signal.SIGUSR1)
    extra = got.qsize()
    emit("wakeup.asyncio", f"handled {n} extra {extra} child {code} {out.decode().strip()}")
    check(code == 0 and extra == 0, "asyncio signals")


# ---------------------------------------------------------------------------------------------
# (c) multiprocessing


def pool_work(args):
    seed, i = args
    return hashlib.sha256(f"{seed}:{i}".encode() * 50).hexdigest()[:16] + f":{cpu_loop(2000, seed + i) & 0xffff}"


def queue_producer(q, seed, start, count):
    for i in range(start, start + count):
        q.put((i, pool_work((seed, i))))
    q.put(None)


def pools(seed, scale):
    items = [(seed, i) for i in range(40 * scale)]
    want = [pool_work(x) for x in items]
    for method in ("fork", "forkserver"):
        ctx = mp.get_context(method)
        with ctx.Pool(2) as p:
            got = p.map(pool_work, items, chunksize=3)
        check(got == want, f"pool {method}")
        emit(f"pool.{method}", hashlib.sha256("".join(got).encode()).hexdigest()[:16])
        # A Queue (a pipe plus a sem_open semaphore and lock) from two producers.
        q = ctx.Queue(maxsize=8)
        procs = [ctx.Process(target=queue_producer, args=(q, seed, k * 20, 20)) for k in range(2)]
        for pr in procs:
            pr.start()
        results, done = {}, 0
        while done < len(procs):
            item = q.get(timeout=120)
            if item is None:
                done += 1
            else:
                results[item[0]] = item[1]
        for pr in procs:
            pr.join()
        check(sorted(results) == list(range(40)), f"queue {method} items")
        check(all(results[i] == pool_work((seed, i)) for i in results), f"queue {method} values")
        emit(f"queue.{method}", f"{len(results)} exits {[pr.exitcode for pr in procs]}")
        # A shared semaphore and value across processes.
        sem = ctx.Semaphore(0)
        val = ctx.Value("q", 0)
        workers = [ctx.Process(target=sem_worker, args=(sem, val, k)) for k in range(3)]
        for w in workers:
            w.start()
        for _ in workers:
            check(sem.acquire(timeout=120), f"semaphore {method}")
        for w in workers:
            w.join()
        emit(f"semaphore.{method}", val.value)


def sem_worker(sem, val, k):
    with val.get_lock():
        val.value += (k + 1) * 100
    sem.release()


# ---------------------------------------------------------------------------------------------
# (d) faulthandler


def watchdog():
    with tempfile.TemporaryFile("w+") as f:
        faulthandler.dump_traceback_later(0.05, repeat=False, file=f)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            cpu_loop(2000)
            time.sleep(0.01)
            f.flush()
            if os.fstat(f.fileno()).st_size > 0:
                break
        faulthandler.cancel_dump_traceback_later()
        time.sleep(0.12)  # a repeat would land here
        f.seek(0)
        text = f.read()
    # The dump's contents are timing-dependent; only that it fired, once.
    emit("faulthandler", f"fired {text.count('Timeout (')} thread {'Thread 0x' in text or 'Current thread' in text}")


# ---------------------------------------------------------------------------------------------
# (e) subprocess pipelines

STAGE = r"""
import sys
k = int(sys.argv[1])
inp, out = sys.stdin.buffer, sys.stdout.buffer
while True:
    b = inp.read(65536 - 1000 * k)
    if not b:
        break
    out.write(bytes((x + k * 37) & 255 for x in b))
out.flush()
"""


def pipelines(seed, scale):
    data = hashlib.shake_256(str(seed).encode()).digest(1 << 18) * (4 * scale)  # 1 MiB per scale
    stages = 3
    want = bytes((x + sum(k * 37 for k in range(1, stages + 1))) & 255 for x in data)
    for shape in range(2):
        procs = []
        prev = subprocess.PIPE
        for k in range(1, stages + 1):
            p = subprocess.Popen([sys.executable, "-I", "-c", STAGE, str(k)], stdin=prev, stdout=subprocess.PIPE)
            if procs:
                procs[-1].stdout.close()
            procs.append(p)
            prev = p.stdout
        first = procs[0]

        def feed():
            if shape == 0:
                first.stdin.write(data)
            else:
                for i in range(0, len(data), 4093):
                    first.stdin.write(data[i:i + 4093])
            first.stdin.close()

        t = threading.Thread(target=feed)
        t.start()
        out = procs[-1].stdout.read()
        t.join()
        codes = [p.wait() for p in procs]
        check(out == want, f"pipeline {shape}: {len(out)} bytes")
        emit(f"pipeline.{shape}", f"{len(out)} {hashlib.sha256(out).hexdigest()[:16]} {codes}")
    # A shell pipeline with coreutils between Python stages.
    sh = subprocess.run(["sh", "-c", f"{sys.executable} -I -c 'import sys; sys.stdout.buffer.write(bytes(range(256))*4096)' | "
                         "tr 'a-z' 'A-Z' | wc -c"], capture_output=True, check=True)
    emit("pipeline.shell", sh.stdout.decode().strip())


def signals_probe(seed, scale):
    timers(seed, scale)
    wakeup_thread(seed, 100)
    asyncio.run(wakeup_asyncio(100))
    pools(seed, scale)
    watchdog()
    pipelines(seed, scale)


def main():
    if len(sys.argv) != 4:
        fail("usage: pyfz signals SEED SCALE")
    test, seed, scale = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    if scale < 1:
        fail("bad SCALE")
    tests = {"signals": signals_probe}
    if test not in tests:
        fail(f"unknown test {test}")
    tests[test](seed, scale)
    print(f"OK Py.{test} {_digest.hexdigest()[:16]}", flush=True)


if __name__ == "__main__":
    main()
