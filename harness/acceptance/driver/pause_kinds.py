"""Process pause (SIGSTOP) and whole-machine pause, each for 20 s, compared.

The app machine runs a 100 ms periodic timerfd, a TCP echo server and a
control port for the oracle. The peer machine sends a numbered frame every 50 ms and records when
each echo returns. The oracle pauses the app with `pause_process`, sends a
request to the app's control port while it is stopped, waits 20 s and resumes
it; then the same with `pause` (the whole machine).

Each pause must follow its documented behaviour:

* both: the app makes no progress during the pause; its next timer read
  reports one burst of the missed expirations; CLOCK_MONOTONIC and
  CLOCK_BOOTTIME both advance by the whole pause; the oracle's pending request
  is answered only after the resume; every frame is echoed exactly once and in
  order;
* pause_process: the app's kernel keeps running, so it keeps ACKing the peer's
  frames and the peer never retransmits;
* pause: the machine's kernel is frozen too, so the peer retransmits.

Measured deltas, bursts, retransmissions and the pause and resume RPC receipt
timings are printed as evidence, which a replay must reproduce.
"""
import json
import os
import socket
import struct
import sys
import threading
import time

ECHO, CONTROL = 7000, 7001
PAUSE_S = 20
TICK_NS = 100_000_000
FRAME = struct.Struct("!QQ16x")  # sequence, sender monotonic ns


def clock(kind):
    return time.clock_gettime_ns(kind)


def retransmissions():
    with open("/proc/net/snmp") as f:
        lines = [line.split() for line in f if line.startswith("Tcp:")]
    return int(lines[1][lines[0].index("RetransSegs")])


def connect(host, port, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        try:
            return socket.create_connection((host, port), timeout=timeout)
        except OSError:
            if time.monotonic() >= deadline:
                raise RuntimeError(f"{host}:{port} readiness timed out")
            time.sleep(0.1)


def serve_json(port, handler):
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", port))
    acceptor.listen()
    def serve(channel):
        with channel:
            for line in channel.makefile("rb"):
                reply = handler(json.loads(line))
                channel.sendall(json.dumps(reply).encode() + b"\n")

    while True:
        threading.Thread(target=serve, args=(acceptor.accept()[0],), daemon=True).start()


# ------------------------------------------------------------------- app side

def app():
    lock = threading.Lock()
    ticks = []

    def ticker():
        fd = os.timerfd_create(time.CLOCK_MONOTONIC)
        os.timerfd_settime_ns(fd, initial=TICK_NS, interval=TICK_NS)
        while True:
            count = struct.unpack("Q", os.read(fd, 8))[0]
            with lock:
                ticks.append((count, clock(time.CLOCK_MONOTONIC), clock(time.CLOCK_BOOTTIME)))

    def echo():
        acceptor = socket.socket()
        acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        acceptor.bind(("0.0.0.0", ECHO))
        acceptor.listen()
        while True:
            channel, _ = acceptor.accept()
            threading.Thread(target=lambda c=channel: [c.sendall(d) for d in iter(lambda: c.recv(4096), b"")],
                             daemon=True).start()

    for target in (ticker, echo):
        threading.Thread(target=target, daemon=True).start()

    def handle(request):
        with lock:
            if request["op"] == "ping":
                return {"pong": request["n"], "mono": clock(time.CLOCK_MONOTONIC)}
            return {"ticks": list(ticks)}

    serve_json(CONTROL, handle)


# ------------------------------------------------------------------ peer side

def peer():
    lock = threading.Lock()
    sent, echoed = {}, []
    channel = connect("app", ECHO)
    channel.settimeout(None)

    def receive():
        buffer = b""
        while True:
            data = channel.recv(4096)
            if not data:
                return
            buffer += data
            while len(buffer) >= FRAME.size:
                seq, _ = FRAME.unpack(buffer[:FRAME.size])
                buffer = buffer[FRAME.size:]
                with lock:
                    echoed.append((seq, clock(time.CLOCK_MONOTONIC)))

    def send():
        seq = 0
        while True:
            now = clock(time.CLOCK_MONOTONIC)
            with lock:
                sent[seq] = now
            channel.sendall(FRAME.pack(seq, now))
            seq += 1
            time.sleep(0.05)

    threading.Thread(target=receive, daemon=True).start()
    threading.Thread(target=send, daemon=True).start()

    def handle(request):
        with lock:
            return {"retrans": retransmissions(), "sent": len(sent), "echoed": list(echoed),
                    "sent_at": sent}

    serve_json(CONTROL, handle)


# --------------------------------------------------------------- oracle side

class Control:
    def __init__(self, host):
        self.channel = connect(host, CONTROL)
        self.channel.settimeout(120)
        self.reader = self.channel.makefile("rb")

    def call(self, **request):
        self.channel.sendall(json.dumps(request).encode() + b"\n")
        return json.loads(self.reader.readline())


def analyse(kind, window, app_before, app_after, peer_before, peer_after, ping):
    """Errors and evidence for one pause; `window` is (paused, resumed) world ns."""
    errors = []
    paused, resumed = window
    span = resumed - paused
    # Timer: the largest gap between consecutive reads in this phase is the pause.
    ticks = app_after["ticks"][len(app_before["ticks"]) - 1:]
    gaps = [(b[1] - a[1], b[2] - a[2], b[0], i) for i, (a, b) in enumerate(zip(ticks, ticks[1:]))]
    mono_gap, boot_gap, burst, _ = max(gaps)
    if mono_gap < span - 2 * TICK_NS:
        errors.append(f"{kind}: largest monotonic gap {mono_gap} ns is shorter than the pause {span} ns")
    if abs(boot_gap - mono_gap) > 1_000_000:
        errors.append(f"{kind}: boottime advanced {boot_gap} ns but monotonic {mono_gap} ns")
    if burst < span // TICK_NS - 5:
        errors.append(f"{kind}: first read after the pause reported {burst} expirations")
    # Echoes: exactly once, in order.
    echoed = [seq for seq, _ in peer_after["echoed"]]
    if echoed != list(range(len(echoed))) or len(echoed) < peer_after["sent"] - 20:
        errors.append(f"{kind}: echoes are not each frame once in order ({len(echoed)} of {peer_after['sent']})")
    times = [t for _, t in peer_after["echoed"][len(peer_before["echoed"]) - 1:]]
    echo_gap = max(b - a for a, b in zip(times, times[1:]))
    if echo_gap < span - 500_000_000:
        errors.append(f"{kind}: echoes continued during the pause (largest gap {echo_gap} ns)")
    retrans = peer_after["retrans"] - peer_before["retrans"]
    if kind == "pause_process" and retrans:
        errors.append(f"pause_process: the peer retransmitted {retrans} segments")
    if kind == "pause" and not retrans:
        errors.append("pause: the peer never retransmitted to the frozen machine")
    if ping["elapsed_ns"] < span - 500_000_000:
        errors.append(f"{kind}: the pending control request was answered after {ping['elapsed_ns']} ns")
    evidence = {"span_ns": span, "mono_gap_ns": mono_gap, "boot_gap_ns": boot_gap, "burst": burst,
                "retrans": retrans, "echo_gap_ns": echo_gap, "ping_elapsed_ns": ping["elapsed_ns"],
                "pause_rpc": ping["pause_rpc"], "resume_rpc": ping["resume_rpc"]}
    return errors, evidence


def ping_during(world, pause, resume, n):
    """Pause, send a control request to the stopped app, wait, resume, then read it."""
    effect = pause("app")
    started = world.now_ns()
    result = {}

    def request():
        try:
            channel = connect("app", CONTROL, timeout=120)
            channel.settimeout(120)
            channel.sendall(json.dumps({"op": "ping", "n": n}).encode() + b"\n")
            result["reply"] = json.loads(channel.makefile("rb").readline())
        except Exception as error:  # Reported below.
            result["error"] = repr(error)

    thread = threading.Thread(target=request)
    thread.start()
    world.sleep(PAUSE_S)
    resumed = resume("app")
    thread.join(120)
    finished = world.now_ns()
    if "reply" not in result:
        raise RuntimeError(f"pending request failed: {result}")
    timing = lambda e: [e.admitted_at_ns - e.requested_at_ns, e.effect_at_ns - e.admitted_at_ns,
                        e.replied_at_ns - e.effect_at_ns]
    return (effect.effect_at_ns, resumed.effect_at_ns), {
        "elapsed_ns": finished - started, "pause_rpc": timing(effect), "resume_rpc": timing(resumed)}


def oracle():
    from seedbox import World
    app_control, peer_control = Control("app"), Control("peer")
    results, errors = {}, []
    with World.connect() as world:
        world.sleep(2)
        for n, (kind, pause, resume) in enumerate((
                ("pause_process", world.pause_process, world.resume_process),
                ("pause", world.pause, world.resume)), start=1):
            app_before, before = app_control.call(op="report"), peer_control.call(op="report")
            window, ping = ping_during(world, pause, resume, n)
            world.sleep(3)
            app_after, after = app_control.call(op="report"), peer_control.call(op="report")
            found, evidence = analyse(kind, window, app_before, app_after, before, after, ping)
            errors += found
            results[kind] = evidence
            print(f"PAUSE {kind} " + json.dumps(evidence, sort_keys=True), flush=True)
    for error in errors:
        print("PAUSE_ERROR", error, flush=True)
    if errors:
        return 1
    print("RESULT pass", flush=True)
    return 0


if __name__ == "__main__":
    role = sys.argv[1]
    if role == "app":
        app()
    elif role == "peer":
        peer()
    else:
        raise SystemExit(oracle())
