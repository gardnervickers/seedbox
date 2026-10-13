"""Open TCP streams across a checkpoint: queued data, a partial read, frames in transit.

Two machines hold three connections. At the capture point (the oracle's
nominal-speed throttle, a no-op fault that marks the phase) the stream state is:

* main: the client has read only part of one framed 512 KiB reply, so reply
  data is queued in the client's receive buffer, the server's send buffer and
  the link; the server has stopped reading, so client data fills its receive
  buffer and the client's sender is blocked in sendall();
* ticker: the client sends a 64-byte frame every 5 ms and the server echoes it,
  so with 20 ms link latency several frames are in transit in each direction.

The run then continues bidirectionally, half-closes main (the server answers
EOF with a summary and closes), and reconnects. Every frame carries a sequence
number and a payload derived from it; both ends hash their streams, so the
oracle checks each record arrived exactly once, in order and byte-exact. The
continuation evidence includes echo times and TCP retransmission counters: a
frame lost or repeated by a restore shows up there even though TCP repairs the
stream. A checkpoint branch must print the same evidence as its source.
"""
import hashlib
import json
import socket
import struct
import sys
import threading
import time

HEADER = struct.Struct(">IIB")  # payload length, sequence, kind
DATA, REQUEST, SUMMARY, HELLO = 1, 2, 3, 4
WARMUP, FLOOD = 64, 512           # Client data frames before and during the hold.
DATA_BYTES = 1024
REPLY_FRAMES, REPLY_BYTES = 256, 2048
READ_BEFORE_HOLD = 100            # Whole reply frames read before the client stops.
TICKS, TICK = 200, 0.005
# Fixed buffers turn autotuning off, so the held stream really backs up.
CLIENT_BUFFERS = {socket.SO_SNDBUF: 32768, socket.SO_RCVBUF: 65536}
SERVER_BUFFERS = {socket.SO_SNDBUF: 65536, socket.SO_RCVBUF: 65536}
TICK_BYTES = 64
CONTROL = {"server": 7300, "client": 7200}
DATA_PORT = 8000
BASE = bytes((i * 131 + 17) % 251 for i in range(16384))


def payload(direction, seq, size):
    start = (seq * 97 + direction * 1009) % 8192
    return BASE[start:start + size]


def frame(kind, seq, body):
    return HEADER.pack(len(body), seq, kind) + body


def read_exact(channel, count):
    data = bytearray()
    while len(data) < count:
        chunk = channel.recv(count - len(data))
        if not chunk:
            raise EOFError(f"EOF after {len(data)} of {count} bytes")
        data += chunk
    return bytes(data)


def read_frame(channel):
    length, seq, kind = HEADER.unpack(read_exact(channel, HEADER.size))
    return kind, seq, read_exact(channel, length)


def tcp_counters():
    with open("/proc/net/snmp") as f:
        lines = [line.split() for line in f if line.startswith("Tcp:")]
    counters = dict(zip(lines[0][1:], map(int, lines[1][1:])))
    return {key: counters[key] for key in ("ActiveOpens", "PassiveOpens", "RetransSegs",
                                            "InSegs", "OutSegs", "InErrs", "OutRsts")}


def control_server(port, handle):
    """One oracle connection of JSON requests and replies."""
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", port))
    acceptor.listen()
    print(f"CONTROL_READY port={port}", flush=True)
    channel, _ = acceptor.accept()
    with channel:
        for line in channel.makefile("rb"):
            reply = handle(json.loads(line))
            channel.sendall(json.dumps(reply, sort_keys=True).encode() + b"\n")


# ------------------------------------------------------------------- server

class Server:
    def __init__(self):
        self.reading = threading.Event()
        self.reading.set()
        self.state = {"data_frames": 0, "data_digest": None, "reply_sent": 0, "ticks_echoed": 0,
                      "verify_echoed": 0, "eof_at": None, "held_at": None, "resumed_at": None}
        self.lock = threading.Lock()

    def main(self, channel):
        digest, expected = hashlib.sha256(), 0
        while True:
            self.reading.wait()
            try:
                kind, seq, body = read_frame(channel)
            except EOFError:
                break
            if kind == DATA:
                if seq != expected or body != payload(0, seq, DATA_BYTES):
                    raise RuntimeError(f"data frame {seq} out of order or corrupt (expected {expected})")
                expected += 1
                digest.update(body)
                with self.lock:
                    self.state["data_frames"] = expected
            elif kind == REQUEST:
                threading.Thread(target=self.reply, args=(channel,), daemon=True).start()
                self.reading.clear()  # Hold: client data now queues in our receive buffer.
                self.state["held_at"] = time.monotonic_ns()
            else:
                raise RuntimeError(f"unexpected kind {kind}")
        self.state["eof_at"] = time.monotonic_ns()
        self.state["data_digest"] = digest.hexdigest()
        summary = json.dumps({"frames": expected, "digest": digest.hexdigest()}).encode()
        channel.sendall(frame(SUMMARY, 0, summary))
        channel.close()

    def reply(self, channel):
        for seq in range(REPLY_FRAMES):
            channel.sendall(frame(DATA, seq, payload(1, seq, REPLY_BYTES)))
            self.state["reply_sent"] = seq + 1

    def echo(self, channel, key):
        with channel:
            while True:
                try:
                    kind, seq, body = read_frame(channel)
                except EOFError:
                    return
                channel.sendall(frame(kind, seq, body))
                self.state[key] += 1

    def accept(self):
        acceptor = socket.socket()
        acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        for option, size in SERVER_BUFFERS.items():
            acceptor.setsockopt(socket.SOL_SOCKET, option, size)
        acceptor.bind(("0.0.0.0", DATA_PORT))
        acceptor.listen()
        while True:
            channel, _ = acceptor.accept()
            _, _, role = read_frame(channel)
            target = {b"main": lambda: self.main(channel),
                      b"ticker": lambda: self.echo(channel, "ticks_echoed"),
                      b"verify": lambda: self.echo(channel, "verify_echoed")}[role]
            threading.Thread(target=target, daemon=True).start()

    def handle(self, request):
        if request["op"] == "resume":
            self.state["resumed_at"] = time.monotonic_ns()
            self.reading.set()
        with self.lock:
            return dict(self.state, tcp=tcp_counters())


# ------------------------------------------------------------------- client

class Client:
    def __init__(self):
        self.main = self.ticker = self.partial = None
        self.sent = 0
        self.reply_digest = hashlib.sha256()
        self.reply_frames = 0
        self.reading = threading.Event()
        self.echo_times = []
        self.threads = []

    def connect(self, role):
        channel = socket.socket()
        for option, size in CLIENT_BUFFERS.items():
            channel.setsockopt(socket.SOL_SOCKET, option, size)
        channel.settimeout(30)
        channel.connect(("server", DATA_PORT))
        channel.settimeout(None)
        channel.sendall(frame(HELLO, 0, role))
        return channel

    def send_data(self, first, count):
        for seq in range(first, first + count):
            self.main.sendall(frame(DATA, seq, payload(0, seq, DATA_BYTES)))
            self.sent = seq + 1

    def read_reply(self, frames, partial=0):
        for _ in range(frames):
            kind, seq, body = read_frame(self.main)
            if kind != DATA or seq != self.reply_frames or body != payload(1, seq, REPLY_BYTES):
                raise RuntimeError(f"reply frame {seq} out of order or corrupt")
            self.reply_digest.update(body)
            self.reply_frames += 1
        if partial:
            # Stop inside a frame: its header and part of its payload are consumed.
            length, seq, kind = HEADER.unpack(read_exact(self.main, HEADER.size))
            self.partial = (length, seq, kind, read_exact(self.main, partial))

    def finish_partial(self):
        length, seq, kind, head = self.partial
        body = head + read_exact(self.main, length - len(head))
        if kind != DATA or seq != self.reply_frames or body != payload(1, seq, REPLY_BYTES):
            raise RuntimeError("partially read reply frame corrupt")
        self.reply_digest.update(body)
        self.reply_frames += 1
        self.read_reply(REPLY_FRAMES - self.reply_frames)

    def tick(self):
        start = time.monotonic_ns()
        for seq in range(TICKS):
            deadline = start + seq * int(TICK * 1e9)
            delay = deadline - time.monotonic_ns()
            if delay > 0:
                time.sleep(delay / 1e9)
            self.ticker.sendall(frame(DATA, seq, payload(2, seq, TICK_BYTES)))

    def echoes(self):
        for seq in range(TICKS):
            kind, got, body = read_frame(self.ticker)
            if got != seq or body != payload(2, seq, TICK_BYTES):
                raise RuntimeError(f"echo {got} out of order or corrupt (expected {seq})")
            self.echo_times.append(time.monotonic_ns())

    def start(self, target, *args):
        thread = threading.Thread(target=target, args=args, daemon=True)
        thread.start()
        self.threads.append(thread)

    def handle(self, request):
        op = request["op"]
        if op == "open":
            self.main, self.ticker = self.connect(b"main"), self.connect(b"ticker")
            return {"opened": 2}
        if op == "warmup":
            self.send_data(0, WARMUP)
            return {"sent": self.sent}
        if op == "request":
            self.main.sendall(frame(REQUEST, 0, b"hold"))
            self.read_reply(READ_BEFORE_HOLD, partial=REPLY_BYTES // 2)
            return {"reply_frames_read": self.reply_frames, "partial_bytes": len(self.partial[3])}
        if op == "flood":
            self.start(self.send_data, WARMUP, FLOOD)
            self.start(self.tick)
            self.start(self.echoes)
            # Wait until the held server's buffers stop accepting client data.
            last, stable = -1, 0
            while stable < 4:
                time.sleep(0.05)
                stable = stable + 1 if self.sent == last else 0
                last = self.sent
            return {"sent_before_block": self.sent, "echoes": len(self.echo_times),
                    "partial_reply_pending": True}
        if op == "continue":
            self.finish_partial()
            for thread in self.threads:
                thread.join()
            return {"sent": self.sent, "reply_frames": self.reply_frames,
                    "reply_digest": self.reply_digest.hexdigest(), "echoes": len(self.echo_times)}
        if op == "close":
            self.main.shutdown(socket.SHUT_WR)
            kind, _, body = read_frame(self.main)
            if kind != SUMMARY:
                raise RuntimeError(f"expected summary, got kind {kind}")
            trailing = self.main.recv(1)
            self.main.close()
            self.ticker.close()
            digest = hashlib.sha256(b"".join(payload(0, seq, DATA_BYTES)
                                              for seq in range(self.sent))).hexdigest()
            return {"summary": json.loads(body), "expected_digest": digest,
                    "eof_after_summary": trailing == b""}
        if op == "reconnect":
            channel = self.connect(b"verify")
            with channel:
                for seq in range(10):
                    channel.sendall(frame(DATA, seq, payload(3, seq, 512)))
                    if read_frame(channel) != (DATA, seq, payload(3, seq, 512)):
                        raise RuntimeError("verify echo mismatch")
            return {"verified": 10}
        if op == "report":
            gaps = [b - a for a, b in zip(self.echo_times, self.echo_times[1:])]
            return {"echo_times": self.echo_times, "echo_gaps_max": max(gaps), "tcp": tcp_counters()}
        raise RuntimeError(f"unknown op {op}")


# ------------------------------------------------------------------- oracle

class Peer:
    def __init__(self, name):
        deadline = time.monotonic() + 30
        while True:
            try:
                self.channel = socket.create_connection((name, CONTROL[name]), timeout=60)
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"{name} control readiness timed out")
                time.sleep(0.1)
        self.reader = self.channel.makefile("rb")

    def __call__(self, op):
        self.channel.sendall(json.dumps({"op": op}).encode() + b"\n")
        line = self.reader.readline()
        if not line:
            raise RuntimeError(f"peer closed during {op}")
        return json.loads(line)


def check(condition, message, evidence=None):
    if not condition:
        raise RuntimeError(f"{message}: {json.dumps(evidence, sort_keys=True)}")


def verify(steps):
    """Exactly-once, in-order, byte-exact delivery in every direction."""
    held, flood, cont, closed, server_end = (steps[k] for k in ("held", "flood", "continue",
                                                                  "close", "server_end"))
    check(held["data_frames"] == WARMUP and held["held_at"] is not None,
          "server holds main after reading the warmup", held)
    check(flood["sent_before_block"] < WARMUP + FLOOD, "client sender blocked by the held server", flood)
    check(cont["sent"] == WARMUP + FLOOD, "every client frame was sent", cont)
    reply = hashlib.sha256(b"".join(payload(1, s, REPLY_BYTES) for s in range(REPLY_FRAMES))).hexdigest()
    check(cont["reply_frames"] == REPLY_FRAMES and cont["reply_digest"] == reply,
          "the partially read reply completed byte-exact", cont)
    check(cont["echoes"] == TICKS and server_end["ticks_echoed"] == TICKS, "every tick echoed once",
          [cont, server_end])
    check(closed["summary"] == {"frames": WARMUP + FLOOD, "digest": closed["expected_digest"]}
          and closed["eof_after_summary"], "server received every client frame once after half-close", closed)
    check(server_end["data_digest"] == closed["expected_digest"], "server stream digest", server_end)
    check(steps["reconnect"] == {"verified": 10}, "reconnected exchange", steps["reconnect"])


def oracle():
    from seedbox import World
    server, client = Peer("server"), Peer("client")
    steps = {}
    with World.connect() as world:
        steps["open"] = client("open")
        steps["warmup"] = client("warmup")
        steps["request"] = client("request")
        steps["flood"] = client("flood")
        steps["held"] = server("status")
        # The capture follows the first fault after this line (applications.rs).
        print("CAPTURE_NEXT", flush=True)
        world.now_ns()
        # Capture point: a nominal-speed throttle changes nothing but marks the phase.
        marker = world.throttle("client", "1GHz")
        print(f"CAPTURE_PHASE at_ns={marker.effect.effect_at_ns}", flush=True)
        steps["resumed"] = server("resume")
        steps["continue"] = client("continue")
        steps["close"] = client("close")
        steps["reconnect"] = client("reconnect")
        steps["server_end"] = server("status")
        steps["client_end"] = client("report")
        world.heal(marker)
    verify(steps)
    continuation = {k: steps[k] for k in ("continue", "close", "reconnect", "server_end", "client_end")}
    digest = hashlib.sha256(json.dumps(continuation, sort_keys=True).encode()).hexdigest()
    print("EVIDENCE " + json.dumps(continuation, sort_keys=True), flush=True)
    print(f"CONTINUATION {digest}", flush=True)
    print(f"RESULT pass frames={WARMUP + FLOOD} reply={REPLY_FRAMES} ticks={TICKS} "
          f"blocked_at={steps['flood']['sent_before_block']} "
          f"retrans={steps['client_end']['tcp']['RetransSegs']}/{steps['server_end']['tcp']['RetransSegs']}",
          flush=True)
    return 0


def main():
    role = sys.argv[1]
    if role == "server":
        server = Server()
        threading.Thread(target=server.accept, daemon=True).start()
        control_server(CONTROL["server"], server.handle)
    elif role == "client":
        control_server(CONTROL["client"], Client().handle)
    elif role == "oracle":
        return oracle()
    else:
        raise RuntimeError(f"unknown role {role}")


if __name__ == "__main__":
    raise SystemExit(main())
