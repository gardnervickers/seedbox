"""Peers detect a crashed server within their TCP timeouts, then meet its successor.

The peer holds 50 connections to the server's port 8000, each with
TCP_USER_TIMEOUT 10 s and keepalive (idle 5 s, interval 1 s, 3 probes), plus two
dormant connections with neither. Right after one ping on every connection the
oracle crashes the server (a crash sends nothing), and the peer then sends 1 KiB
on 25 of them, 100 ms apart, which can never be acknowledged. Meanwhile the
oracle partitions the peer from a third machine, the witness, for a seeded
1-30 s while the witness sends heartbeats to the peer.

Checked against Linux's timer rules (TCP_USER_TIMEOUT bounds both the
retransmission and the keepalive abort):

* every connection fails with ETIMEDOUT, the idle ones 10 s after the ping
  (within one keepalive interval), the ones with data 10 s after their send
  and in the order they sent, each allowing the timer wheel's deferral;
* after the reboot, data on a dormant connection draws a RST (ECONNRESET), a
  SYN to the closed port 8001 draws a RST (ECONNREFUSED), and a new connection
  to port 8000 works;
* every heartbeat is echoed once, in order, with the largest gap at least
  the partition.

Failure offsets, errors and /proc/net/snmp counters are printed as evidence,
which a replay must reproduce.
"""
import errno
import json
import os
import random
import socket
import struct
import sys
import threading
import time

SERVICE, CLOSED, CONTROL, HEARTBEAT = 8000, 8001, 7001, 7002
CONNECTIONS, DATA = 50, 25
USER_TIMEOUT_MS = 10_000
KEEPIDLE, KEEPINTVL, KEEPCNT = 5, 1, 3
STAGGER_NS = 100_000_000
SECOND = 1_000_000_000


def mono():
    return time.monotonic_ns()


def snmp():
    counters = {}
    with open("/proc/net/snmp") as f:
        lines = [line.split() for line in f if line.startswith("Tcp:")]
    for key, value in zip(lines[0][1:], lines[1][1:]):
        if key in ("ActiveOpens", "AttemptFails", "EstabResets", "RetransSegs", "InErrs", "OutRsts"):
            counters[key] = int(value)
    return counters


def connect(host, port, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        try:
            return socket.create_connection((host, port), timeout=10)
        except OSError:
            if time.monotonic() >= deadline:
                raise RuntimeError(f"{host}:{port} readiness timed out")
            time.sleep(0.1)


def serve(port, handler):
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", port))
    acceptor.listen(128)

    def run(channel):
        with channel:
            handler(channel)

    while True:
        threading.Thread(target=run, args=(acceptor.accept()[0],), daemon=True).start()


def echo(channel):
    while data := channel.recv(65536):
        channel.sendall(data)


def json_handler(handle):
    def handler(channel):
        for line in channel.makefile("rb"):
            channel.sendall(json.dumps(handle(json.loads(line))).encode() + b"\n")
    return handler


# --------------------------------------------------------------- server side

def server():
    print("SERVER_READY", flush=True)
    serve(SERVICE, echo)


# ----------------------------------------------------------------- peer side

class Peer:
    def __init__(self):
        self.lock = threading.Lock()
        self.connections, self.dormant = [], []
        self.pinged = None
        self.sent, self.failed = {}, {}

    def open(self):
        for i in range(CONNECTIONS + 2):
            channel = connect("server", SERVICE)
            if i < CONNECTIONS:
                channel.setsockopt(socket.IPPROTO_TCP, socket.TCP_USER_TIMEOUT, USER_TIMEOUT_MS)
                channel.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)
                channel.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPIDLE, KEEPIDLE)
                channel.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPINTVL, KEEPINTVL)
                channel.setsockopt(socket.IPPROTO_TCP, socket.TCP_KEEPCNT, KEEPCNT)
                self.connections.append(channel)
            else:
                self.dormant.append(channel)
        # One ping on every connection; the last pong is the last data received.
        for channel in self.connections + self.dormant:
            channel.sendall(b"p")
        for channel in self.connections + self.dormant:
            if channel.recv(1) != b"p":
                raise RuntimeError("ping not echoed")
            channel.settimeout(None)
        self.pinged = mono()
        for i, channel in enumerate(self.connections):
            threading.Thread(target=self.watch, args=(i, channel), daemon=True).start()
        return {"opened": len(self.connections) + len(self.dormant)}

    def watch(self, i, channel):
        try:
            data = channel.recv(1)
            outcome = "eof" if not data else "data"
        except OSError as error:
            outcome = errno.errorcode.get(error.errno, str(error.errno))
        with self.lock:
            self.failed[i] = (mono(), outcome)

    def send(self):
        for i in range(DATA):
            with self.lock:
                self.sent[i] = mono()
            self.connections[i].sendall(bytes([i]) * 1024)
            time.sleep(STAGGER_NS / SECOND)
        return {"sent": DATA}

    def status(self):
        with self.lock:
            return {"failed": len(self.failed)}

    def outcomes(self):
        """Failure offsets in ms from the ping (idle) or the send (data)."""
        with self.lock:
            result = []
            for i in range(CONNECTIONS):
                at, outcome = self.failed.get(i, (None, "open"))
                start = self.sent[i] if i < DATA else self.pinged
                result.append({"i": i, "data": i < DATA, "error": outcome,
                               "ms": None if at is None else (at - start) // 1_000_000})
            return result

    def after_reboot(self):
        result = {}
        channel = self.dormant[0]
        channel.settimeout(10)
        try:
            channel.sendall(b"stale")
            data = channel.recv(16)
            result["dormant"] = "data" if data else "eof"
        except OSError as error:
            result["dormant"] = errno.errorcode.get(error.errno, str(error.errno))
        try:
            socket.create_connection(("server", CLOSED), timeout=10).close()
            result["closed_port"] = "connected"
        except OSError as error:
            result["closed_port"] = errno.errorcode.get(error.errno, str(error.errno))
        with connect("server", SERVICE) as fresh:
            fresh.sendall(b"fresh")
            result["fresh"] = fresh.recv(16).decode()
        result["snmp"] = snmp()
        return result


def peer():
    state = Peer()
    threading.Thread(target=serve, args=(HEARTBEAT, echo), daemon=True).start()
    serve(CONTROL, json_handler(lambda request: getattr(state, request["op"])()))


# -------------------------------------------------------------- witness side

def witness():
    lock = threading.Lock()
    echoed = []
    channel = connect("peer", HEARTBEAT)
    channel.settimeout(None)

    def receive():
        buffer = b""
        while data := channel.recv(4096):
            buffer += data
            while len(buffer) >= 8:
                (n,) = struct.unpack("!Q", buffer[:8])
                buffer = buffer[8:]
                with lock:
                    echoed.append((n, mono()))

    def send():
        n = 0
        while True:
            channel.sendall(struct.pack("!Q", n))
            n += 1
            time.sleep(0.2)

    threading.Thread(target=receive, daemon=True).start()
    threading.Thread(target=send, daemon=True).start()

    def report():
        with lock:
            numbers = [n for n, _ in echoed]
            gaps = [b - a for (_, a), (_, b) in zip(echoed, echoed[1:])]
            return {"echoed": len(numbers), "in_order": numbers == list(range(len(numbers))),
                    "max_gap_ms": max(gaps, default=0) // 1_000_000}

    serve(CONTROL, json_handler(lambda request: report()))


# --------------------------------------------------------------- oracle side

def check(outcomes, recovery, witness_report, partition_s):
    errors = []
    for o in outcomes:
        if o["error"] != "ETIMEDOUT":
            errors.append(f"connection {o['i']} ended with {o['error']}, not ETIMEDOUT")
            continue
        # The clamped final retransmission or keepalive timer runs seconds out,
        # where the kernel's timer wheel may defer it by up to 640 ms (HZ=100).
        low = USER_TIMEOUT_MS - 50
        high = USER_TIMEOUT_MS + 700 + (0 if o["data"] else KEEPINTVL * 1000)
        if not low <= o["ms"] <= high:
            errors.append(f"connection {o['i']} ({'data' if o['data'] else 'idle'}) failed after "
                          f"{o['ms']} ms, outside [{low}, {high}]")
    data = [o for o in outcomes if o["data"] and o["ms"] is not None]
    absolute = [o["i"] * STAGGER_NS // 1_000_000 + o["ms"] for o in data]
    if absolute != sorted(absolute):
        errors.append(f"connections with data did not fail in send order: {absolute}")
    if recovery["dormant"] != "ECONNRESET":
        errors.append(f"data on a pre-crash connection drew {recovery['dormant']}, not a RST")
    if recovery["closed_port"] != "ECONNREFUSED":
        errors.append(f"a SYN to the closed port drew {recovery['closed_port']}, not a RST")
    if recovery["fresh"] != "fresh":
        errors.append(f"a new connection echoed {recovery['fresh']!r}")
    if not witness_report["in_order"]:
        errors.append("witness heartbeats were lost, duplicated or reordered")
    if witness_report["max_gap_ms"] < partition_s * 1000 - 300:
        errors.append(f"witness heartbeats continued through a {partition_s} s partition "
                      f"(largest gap {witness_report['max_gap_ms']} ms)")
    return errors


def oracle():
    from seedbox import World

    class Control:
        def __init__(self, host):
            self.channel = connect(host, CONTROL)
            self.channel.settimeout(120)
            self.reader = self.channel.makefile("rb")

        def __call__(self, op):
            self.channel.sendall(json.dumps({"op": op}).encode() + b"\n")
            return json.loads(self.reader.readline())

    rng = random.Random(os.urandom(16))
    partition_s = round(rng.uniform(1, 30), 1)
    peer_control = Control("peer")
    with World.connect() as world:
        peer_control("open")
        crashed = world.crash("server")
        cut = world.partition(["peer"], ["witness"])
        peer_control("send")
        deadline = crashed.effect_at_ns + 20 * SECOND
        while peer_control("status")["failed"] < CONNECTIONS and world.now_ns() < deadline:
            world.sleep(0.5)
        outcomes = peer_control("outcomes")
        remaining = cut.effect.effect_at_ns + int(partition_s * SECOND) - world.now_ns()
        if remaining > 0:
            world.sleep(remaining / SECOND)
        world.heal(cut)
        world.reboot("server")
        connect("server", SERVICE).close()
        recovery = peer_control("after_reboot")
        # Let the witness's retransmissions recover from the partition.
        world.sleep(min(2 * partition_s, 40) + 2)
        witness_report = Control("witness")("report")
    errors = check(outcomes, recovery, witness_report, partition_s)
    for error in errors:
        print("PEER_ERROR", error, flush=True)
    evidence = {"partition_s": partition_s, "recovery": recovery, "witness": witness_report,
                "failures": [[o["i"], o["error"], o["ms"]] for o in outcomes]}
    print("EVIDENCE " + json.dumps(evidence, sort_keys=True), flush=True)
    if errors:
        return 1
    print(f"RESULT pass partition_s={partition_s}", flush=True)
    return 0


if __name__ == "__main__":
    role = sys.argv[1]
    roles = {"server": server, "peer": peer, "witness": witness}
    if role in roles:
        roles[role]()
    else:
        raise SystemExit(oracle())
