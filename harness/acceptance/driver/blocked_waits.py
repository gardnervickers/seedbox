"""Threads blocked in kernel waits across a checkpoint, released exactly once.

The waits machine parks one thread in each wait and reports the phase only
after /proc shows every waiter sleeping:

* futex: a condition variable with no timeout;
* futex-timeout: a condition variable that must time out at a fixed deadline;
* pipe: a blocking read;
* timerfd: a read of a one-shot absolute CLOCK_MONOTONIC timer;
* epoll: epoll_wait on a socketpair and an eventfd;
* accept: a blocking accept on port 7101;
* nanosleep: a sleep until a fixed absolute deadline;
* wait4: waitpid() on a child process that is itself blocked reading a pipe.

The oracle's nominal-speed throttle marks the capture point. Afterwards the
oracle releases the waiters one by one with numbered tokens; deadline waiters
complete on their own. Each waiter then checks that nothing else is pending
(no second wakeup, byte, event, connection or expiration). The oracle checks
each completion happened once, with its token, not before its deadline, and
prints the completion ledger, including wake times, as continuation evidence.
"""
import hashlib
import json
import os
import select
import socket
import sys
import threading
import time

CONTROL, ACCEPT = 7100, 7101
DEADLINE_NS = 1_500_000_000      # Deadline waiters wake this long after parking.
KINDS = ("futex", "futex-timeout", "pipe", "timerfd", "epoll", "accept", "nanosleep", "wait4")
RELEASED = ("futex", "pipe", "epoll", "accept", "wait4")


def thread_state(tid):
    with open(f"/proc/self/task/{tid}/stat") as f:
        return f.read().rsplit(")", 1)[1].split()[0]


class Waits:
    def __init__(self):
        self.lock = threading.Lock()
        self.done = {}
        self.tids = {}
        self.cond = threading.Condition()
        self.token = None
        self.idle = threading.Condition()
        self.pipe_r, self.pipe_w = os.pipe()
        self.pair = socket.socketpair()
        self.event = os.eventfd(0)
        self.acceptor = socket.socket()
        self.acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.acceptor.bind(("0.0.0.0", ACCEPT))
        self.acceptor.listen()
        self.child_r, self.child_w = os.pipe()
        self.child = None
        self.deadline = None

    def finish(self, kind, woke_ns, **result):
        with self.lock:
            if kind in self.done:
                raise RuntimeError(f"{kind} completed twice")
            self.done[kind] = dict(result, woke_ns=woke_ns)

    def waiter(self, kind):
        self.tids[kind] = threading.get_native_id()
        if kind == "futex":
            with self.cond:
                while self.token is None:
                    self.cond.wait()
                token = self.token
            self.finish(kind, time.monotonic_ns(), token=token)
        elif kind == "futex-timeout":
            with self.idle:
                notified = self.idle.wait(timeout=(self.deadline - time.monotonic_ns()) / 1e9)
            self.finish(kind, time.monotonic_ns(), notified=notified)
        elif kind == "pipe":
            token = int(os.read(self.pipe_r, 64))
            woke = time.monotonic_ns()
            more, _, _ = select.select([self.pipe_r], [], [], 0.2)
            self.finish(kind, woke, token=token, extra=bool(more))
        elif kind == "timerfd":
            fd = os.timerfd_create(time.CLOCK_MONOTONIC)
            os.timerfd_settime_ns(fd, flags=os.TFD_TIMER_ABSTIME, initial=self.deadline)
            expirations = int.from_bytes(os.read(fd, 8), "little")
            woke = time.monotonic_ns()
            os.set_blocking(fd, False)
            try:
                os.read(fd, 8)
                extra = True
            except BlockingIOError:
                extra = False
            self.finish(kind, woke, expirations=expirations, extra=extra)
        elif kind == "epoll":
            poller = select.epoll()
            poller.register(self.pair[1].fileno(), select.EPOLLIN)
            poller.register(self.event, select.EPOLLIN)
            events = poller.poll()
            woke = time.monotonic_ns()
            token = int(self.pair[1].recv(64))
            later = poller.poll(0.2)
            self.finish(kind, woke, token=token, events=len(events), extra=len(later))
        elif kind == "accept":
            channel, _ = self.acceptor.accept()
            woke = time.monotonic_ns()
            with channel:
                token = int(channel.makefile("rb").readline())
            self.acceptor.setblocking(False)
            try:
                self.acceptor.accept()[0].close()
                extra = True
            except BlockingIOError:
                extra = False
            self.finish(kind, woke, token=token, extra=extra)
        elif kind == "nanosleep":
            time.sleep((self.deadline - time.monotonic_ns()) / 1e9)
            self.finish(kind, time.monotonic_ns())
        elif kind == "wait4":
            _, status = os.waitpid(self.child, 0)
            self.finish(kind, time.monotonic_ns(), token=os.waitstatus_to_exitcode(status))
        else:
            raise RuntimeError(kind)

    def park(self):
        self.child = os.fork()
        if self.child == 0:
            # The child blocks in its own pipe read; its exit status is the token.
            os._exit(int(os.read(self.child_r, 64)) % 256)
        self.deadline = time.monotonic_ns() + DEADLINE_NS
        for kind in KINDS:
            threading.Thread(target=self.waiter, args=(kind,), daemon=True).start()
        # Report the phase only once every waiter (and the child) sleeps in the kernel.
        while True:
            states = {kind: thread_state(tid) for kind, tid in list(self.tids.items())}
            with open(f"/proc/{self.child}/stat") as f:
                states["child"] = f.read().rsplit(")", 1)[1].split()[0]
            if len(states) == len(KINDS) + 1 and set(states.values()) == {"S"}:
                return {"parked": sorted(states), "deadline_ns": self.deadline}
            time.sleep(0.01)

    def release(self, kind, token):
        if kind == "futex":
            with self.cond:
                self.token = token
                self.cond.notify()
        elif kind == "pipe":
            os.write(self.pipe_w, b"%d" % token)
        elif kind == "epoll":
            self.pair[0].sendall(b"%d" % token)
        elif kind == "accept":
            pass  # The oracle connects and sends the token itself.
        elif kind == "wait4":
            os.write(self.child_w, b"%d" % token)
        else:
            raise RuntimeError(kind)
        return {"released": kind}

    def ledger(self):
        while True:
            with self.lock:
                if len(self.done) == len(KINDS):
                    return {"done": self.done, "deadline_ns": self.deadline}
            time.sleep(0.05)


def app():
    waits = Waits()
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", CONTROL))
    acceptor.listen()
    print("WAITS_READY", flush=True)
    channel, _ = acceptor.accept()
    with channel:
        for line in channel.makefile("rb"):
            request = json.loads(line)
            if request["op"] == "park":
                reply = waits.park()
            elif request["op"] == "release":
                reply = waits.release(request["kind"], request["token"])
            elif request["op"] == "ledger":
                reply = waits.ledger()
            else:
                raise RuntimeError(request["op"])
            channel.sendall(json.dumps(reply, sort_keys=True).encode() + b"\n")


# ------------------------------------------------------------------- oracle

def tokens():
    return {kind: 100 + index for index, kind in enumerate(RELEASED)}


def verify(ledger, expected):
    """Every waiter completed once, with its own token, never before its deadline."""
    done, deadline = ledger["done"], ledger["deadline_ns"]
    errors = []
    if sorted(done) != sorted(KINDS):
        errors.append(f"completions {sorted(done)}")
    for kind, token in expected.items():
        if done.get(kind, {}).get("token") != token:
            errors.append(f"{kind} token {done.get(kind)} != {token}")
    for kind in ("pipe", "timerfd", "accept"):
        if done.get(kind, {}).get("extra"):
            errors.append(f"{kind} delivered twice")
    epoll = done.get("epoll", {})
    if epoll.get("events") != 1 or epoll.get("extra") != 0:
        errors.append(f"epoll events {epoll}")
    if done.get("timerfd", {}).get("expirations") != 1:
        errors.append(f"timerfd expirations {done.get('timerfd')}")
    if done.get("futex-timeout", {}).get("notified") is not False:
        errors.append(f"futex timeout {done.get('futex-timeout')}")
    for kind in ("futex-timeout", "timerfd", "nanosleep"):
        if done.get(kind, {}).get("woke_ns", 0) < deadline:
            errors.append(f"{kind} woke before its deadline: {done.get(kind)} < {deadline}")
    return errors


def oracle():
    from seedbox import World
    deadline = time.monotonic() + 30
    while True:
        try:
            channel = socket.create_connection(("waits", CONTROL), timeout=30)
            break
        except OSError:
            if time.monotonic() >= deadline:
                raise RuntimeError("waits readiness timed out")
            time.sleep(0.1)
    reader = channel.makefile("rb")

    def call(**request):
        channel.sendall(json.dumps(request).encode() + b"\n")
        return json.loads(reader.readline())

    expected = tokens()
    with World.connect() as world:
        parked = call(op="park")
        # Capture point: a nominal-speed throttle changes nothing but marks the phase.
        marker = world.throttle("waits", "1GHz")
        print(f"CAPTURE_PHASE at_ns={marker.effect.effect_at_ns} parked={','.join(parked['parked'])}",
              flush=True)
        for kind in RELEASED:
            world.sleep(0.1)
            if kind == "accept":
                with socket.create_connection(("waits", 7101), timeout=10) as peer:
                    peer.sendall(b"%d\n" % expected[kind])
            else:
                call(op="release", kind=kind, token=expected[kind])
        ledger = call(op="ledger")
        world.heal(marker)
    errors = verify(ledger, expected)
    for error in errors:
        print("WAIT_ERROR", error, flush=True)
    evidence = json.dumps(ledger, sort_keys=True)
    print("EVIDENCE " + evidence, flush=True)
    print("CONTINUATION " + hashlib.sha256(evidence.encode()).hexdigest(), flush=True)
    if errors:
        return 1
    print(f"RESULT pass waits={len(KINDS)} released={len(RELEASED)}", flush=True)
    return 0


def main():
    role = sys.argv[1]
    if role == "app":
        app()
    elif role == "oracle":
        return oracle()
    else:
        raise RuntimeError(f"unknown role {role}")


if __name__ == "__main__":
    raise SystemExit(main())
