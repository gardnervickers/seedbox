"""A checkpoint while shaped traffic is queued under partition and drop layers.

The src machine sends numbered UDP datagrams to dst in bursts and keeps one TCP
connection to dst with a line every 50 ms. Before the capture the oracle shapes
src->dst to 400 kbit/s with a 3 s queue bound, throttles src, skews dst's wall
clock, drifts src's clock, queues a 100-datagram burst, drops the TCP
connection, partitions src from dst and sends a second burst, which the
partition loses.
A nominal-speed throttle on dst marks the capture with the shaped queue still
full. Afterwards the oracle heals every handle in reverse order and checks:

* the queued burst arrives whole and in order (routed frames keep their
  delivery decision), the burst sent under the partition never arrives;
* with the partition and drop healed, the shape still paces a burst; with the
  shape healed, a burst arrives unpaced;
* each heal reports its own counters: the drop, alone once the partition above
  it is healed, dropped packets; the shape
  matched every datagram it paced; the skew is undone on dst's wall clock;
* TCP lines arrive exactly once, in order, after the heals.

The deliveries, spreads, counters and clock offsets are printed as continuation
evidence, so a branch from the capture must reproduce them.
"""
import hashlib
import json
import socket
import sys
import threading
import time

UDP, TCP, CONTROL = 9000, 9100, 7001
DATAGRAM = 1000
BANDWIDTH = 400_000
SKEW_NS = 7_000_000_000
BURSTS = (("queued", 100), ("partitioned", 20), ("shaped", 40), ("unshaped", 40))


def connect(host, port, timeout=60):
    deadline = time.monotonic() + timeout
    while True:
        try:
            return socket.create_connection((host, port), timeout=10)
        except OSError:
            if time.monotonic() >= deadline:
                raise RuntimeError(f"{host}:{port} readiness timed out")
            time.sleep(0.1)


def serve(port, handle):
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", port))
    acceptor.listen()

    def run(channel):
        with channel:
            for line in channel.makefile("rb"):
                channel.sendall(json.dumps(handle(json.loads(line))).encode() + b"\n")

    while True:
        threading.Thread(target=run, args=(acceptor.accept()[0],), daemon=True).start()


def clocks():
    return {"realtime": time.clock_gettime_ns(time.CLOCK_REALTIME),
            "monotonic": time.clock_gettime_ns(time.CLOCK_MONOTONIC)}


# ------------------------------------------------------------------ dst side

def dst():
    lock = threading.Lock()
    datagrams, lines = [], []

    def udp():
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as receiver:
            receiver.bind(("0.0.0.0", UDP))
            while True:
                data = receiver.recv(2048)
                with lock:
                    datagrams.append((int(data[:8]), time.monotonic_ns()))

    def tcp():
        acceptor = socket.socket()
        acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        acceptor.bind(("0.0.0.0", TCP))
        acceptor.listen()
        channel, _ = acceptor.accept()
        for line in channel.makefile("rb"):
            with lock:
                lines.append(int(line.split()[1]))

    threading.Thread(target=udp, daemon=True).start()
    threading.Thread(target=tcp, daemon=True).start()

    def handle(request):
        with lock:
            if request["op"] == "clock":
                return clocks()
            return {"datagrams": list(datagrams), "lines": list(lines)}

    serve(CONTROL, handle)


# ------------------------------------------------------------------ src side

def src():
    sent = [0]
    sender = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    def ticker(channel):
        n = 0
        while True:
            channel.sendall(f"T {n}\n".encode())
            n += 1
            time.sleep(0.05)

    def handle(request):
        if request["op"] == "tcp":
            threading.Thread(target=ticker, args=(connect("dst", TCP),), daemon=True).start()
            return {"tcp": True}
        first = sent[0]
        for seq in range(first, first + request["count"]):
            sender.sendto(b"%08d" % seq + bytes(DATAGRAM - 8), ("dst", UDP))
        sent[0] += request["count"]
        return {"first": first, "count": request["count"]}

    serve(CONTROL, handle)


# --------------------------------------------------------------- oracle side

class Control:
    def __init__(self, host):
        self.channel = connect(host, CONTROL)
        self.channel.settimeout(60)
        self.reader = self.channel.makefile("rb")

    def __call__(self, **request):
        self.channel.sendall(json.dumps(request).encode() + b"\n")
        return json.loads(self.reader.readline())


def deliveries(datagrams, first, count):
    """Sequence numbers of one burst as delivered, and their arrival spread."""
    got = [(seq, at) for seq, at in datagrams if first <= seq < first + count]
    times = [at for _, at in got]
    return [seq for seq, _ in got], (max(times) - min(times) if times else 0)


def check(bursts, report, heals, offsets):
    errors, evidence = [], {}
    for name, (first, count) in bursts.items():
        seqs, spread = deliveries(report["datagrams"], first, count)
        evidence[name] = {"delivered": len(seqs), "spread_ms": spread // 1_000_000}
        expected = [] if name == "partitioned" else list(range(first, first + count))
        if seqs != expected:
            errors.append(f"{name} burst delivered {len(seqs)} of {count} "
                          f"({'in order' if seqs == sorted(seqs) else 'out of order'})")
    paced = (bursts["shaped"][1] - 1) * DATAGRAM * 8 * 1_000_000_000 // BANDWIDTH
    if evidence["shaped"]["spread_ms"] * 1_000_000 < paced * 9 // 10:
        errors.append(f"shaped burst spread {evidence['shaped']['spread_ms']} ms; the shape no longer paces")
    if evidence["unshaped"]["spread_ms"] * 1_000_000 > paced // 4:
        errors.append(f"unshaped burst spread {evidence['unshaped']['spread_ms']} ms; the shape survived its heal")
    lines = report["lines"]
    if lines != list(range(len(lines))) or len(lines) < 40:
        errors.append(f"TCP lines lost, duplicated or reordered ({len(lines)})")
    if heals["drop"].get("dropped", 0) <= 0:
        errors.append(f"connection drop reports {heals['drop']}")
    # The partition above the shape drops the partitioned burst first.
    shaped = bursts["queued"][1] + bursts["shaped"][1]
    if heals["shape"]["queue_counts"]["matched"] < shaped:
        errors.append(f"shape matched {heals['shape']['queue_counts']['matched']} of {shaped}")
    if abs(offsets["skewed"] - offsets["base"] - SKEW_NS) > 50_000_000:
        errors.append(f"dst wall clock moved {offsets['skewed'] - offsets['base']} ns, not {SKEW_NS}")
    if abs(offsets["healed"] - offsets["base"]) > 50_000_000:
        errors.append(f"dst wall clock is {offsets['healed'] - offsets['base']} ns off after the heal")
    evidence["lines"] = len(lines)
    evidence["heals"] = heals
    evidence["offsets_ms"] = {k: (v - offsets["base"]) // 1_000_000 for k, v in offsets.items()}
    return errors, evidence


def oracle():
    from seedbox import World
    source, sink = Control("src"), Control("dst")

    def offset():
        sample = sink(op="clock")
        return sample["realtime"] - sample["monotonic"]

    bursts, heals, offsets = {}, {}, {"base": offset()}

    def burst(name):
        reply = source(op="burst", count=dict(BURSTS)[name])
        bursts[name] = (reply["first"], reply["count"])

    with World.connect() as world:
        source(op="tcp")
        world.sleep(0.5)
        shape = world.shape(["src"], ["dst"], latency=1_000_000, bandwidth_bps=BANDWIDTH,
                            queue_bound=3_000_000_000)
        throttle = world.throttle("src", "500MHz")
        skew = world.clock_skew("dst", offset=SKEW_NS)
        drift = world.clock_drift("src", ppb=20_000)
        offsets["skewed"] = offset()
        burst("queued")
        found = world.connections("src", "dst")["connections"]
        drop = world.drop_connection(next(c for c in found if c["dst_port"] == TCP))
        partition = world.partition(["src"], ["dst"])
        burst("partitioned")
        # The capture follows the first fault after this line (applications.rs).
        print("CAPTURE_NEXT", flush=True)
        world.now_ns()
        # Capture point: a nominal-speed throttle changes nothing but marks the phase.
        marker = world.throttle("dst", "1GHz")
        print(f"CAPTURE_PHASE at_ns={marker.effect.effect_at_ns} handles={len(world.inspect()['active_handles'])}",
              flush=True)
        # Heal in reverse order of creation. The partition hides the drop until
        # it is healed; then the drop alone loses the TCP retransmissions.
        world.sleep(3)
        world.heal(partition)
        world.sleep(1)
        heals["drop"] = world.heal(drop)
        world.sleep(1)
        burst("shaped")
        world.sleep(2)
        world.heal(drift)
        world.heal(skew)
        world.heal(throttle)
        offsets["healed"] = offset()
        heals["shape"] = world.heal(shape)
        burst("unshaped")
        world.sleep(2)
        world.heal(marker)
        remaining = world.inspect()["active_handles"]
        report = sink(op="report")
    errors, evidence = check(bursts, report, heals, offsets)
    if remaining:
        errors.append(f"handles remain after every heal: {remaining}")
    for error in errors:
        print("LINK_ERROR", error, flush=True)
    text = json.dumps(evidence, sort_keys=True)
    print("EVIDENCE " + text, flush=True)
    print("CONTINUATION " + hashlib.sha256(text.encode()).hexdigest(), flush=True)
    if errors:
        return 1
    print("RESULT pass", flush=True)
    return 0


if __name__ == "__main__":
    role = sys.argv[1]
    if role == "src":
        src()
    elif role == "dst":
        dst()
    else:
        raise SystemExit(oracle())
