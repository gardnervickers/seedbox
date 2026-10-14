"""Jepsen's Kafka queue workload under Seedbox: KAFKA-17754 and acks=all durability.

Machines: brokers n1..nN (KRaft, combined broker+controller) and one client
machine c1 running `image/JepsenQueue.java` (Kafka's own Java client, the
client Jepsen used). The client streams every invocation and completion to
this oracle as JSON lines on port 7100; the oracle owns the history, the
fault schedule and the checker, and runs on one thread: it serves the client
connection whenever it waits.

Workloads (`--workload`):

* `txn`: Jepsen's transactional queue workload (sends and polls inside
  transactions, sendOffsetsToTransaction, intentional aborts). Fault modes
  (`--nemesis`):
  - `none`: no faults.
  - `end-partition` (KAFKA-17754): arm a console trigger on the client's
    `@@END` marker, printed just before each commitTransaction/abortTransaction,
    that cuts c1 off from every broker. The EndTxn request is lost in flight;
    the client times out (request.timeout.ms 3 s), closes that connection and,
    after the heal, retries EndTxn on a new connection, which succeeds. The
    first copy is still in c1's TCP send queue and is retransmitted later, when
    it can decide the fate of the producer's next transaction. Each cut lasts
    `--cut` s (uniform in the given range), then `--gap` s of quiet.
  - `jepsen`: Jepsen's mixed nemesis without targeting: every `--interval` s,
    either pause a random broker's process or cut c1 off from a random broker,
    for 2..10 s.
* `produce` with `--nemesis crash-all`: an idempotent acks=all producer
  writes continuously; after `--crash-after` acknowledgements the oracle
  power-fails every broker machine at once, reboots them all, and the final
  read must still hold every acknowledged record.

Checker (after all faults are healed, the client stops and reads every
partition from offset 0 at read_committed):

* lost: a send acknowledged in a committed (ok) transaction, or an
  acknowledged produce, missing from the final read;
* aborted read (G1a): a send of a failed (definitely aborted) transaction that
  appears in the final read or in any poll;
* torn transaction: an ok or failed transaction whose acknowledged sends are
  partly visible and partly not;
* inconsistent offsets (two values at one offset) and duplicates (one value at
  two offsets);
* Elle list-append cycles over the version order of the final read (G0, G1c);
  Kafka transactions routinely interleave writes (Jepsen's G0 finding), so
  cycles are reported but are not part of the verdict.

Exit status: 0 when the checks hold, 1 on a violation, 3 when inconclusive.
Evidence lines start with `EVIDENCE `, history lines with `H `; the last line
is `VERDICT`. `--check-log FILE` reruns the checker on a saved oracle log.
"""
import argparse
import collections
import json
import os
import random
import selectors
import socket
import sys
import time

START = time.monotonic()
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.dirname(HERE))  # ../elle in the repository


def now():
    return round(time.monotonic() - START, 3)


def evidence(kind, **fields):
    print("EVIDENCE " + json.dumps(dict(t=now(), kind=kind, **fields), sort_keys=True), flush=True)


def log(message):
    print(f"[{now():9.3f}] {message}", flush=True)


# ------------------------------------------------------------ client connection and history

class History:
    def __init__(self):
        self.events = []
        self.hello = None
        self.topic_ready = None
        self.started = False
        self.final = None
        self.done = False
        self.acks = 0
        self.ends = []  # (t, op, kind)

    def handle(self, event):
        event["t"] = now()
        kind = event.get("type")
        self.events.append(event)
        print("H " + json.dumps(event, sort_keys=True), flush=True)
        if kind == "hello":
            self.hello = event
        elif kind == "topic-ready":
            self.topic_ready = event
        elif kind == "final":
            self.final = event
        elif kind == "done":
            self.done = True
        elif kind == "ok" and event.get("f") == "send":
            self.acks += 1
        elif kind == "end":
            self.ends.append((event["t"], event.get("op"), event.get("kind")))


class Loop:
    """A single-threaded line server for the client's event stream."""

    def __init__(self, history, port):
        self.history = history
        self.selector = selectors.DefaultSelector()
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("0.0.0.0", port))
        self.listener.listen(16)
        self.listener.setblocking(False)
        self.selector.register(self.listener, selectors.EVENT_READ, None)
        self.buffers = {}

    def wait(self, predicate=None, timeout=0.0):
        deadline = time.monotonic() + timeout
        while True:
            value = predicate() if predicate else None
            remaining = deadline - time.monotonic()
            if value or remaining <= 0:
                return value
            for key, _ in self.selector.select(min(remaining, 0.5)):
                if key.fileobj is self.listener:
                    connection, _ = self.listener.accept()
                    connection.setblocking(False)
                    self.buffers[connection] = bytearray()
                    self.selector.register(connection, selectors.EVENT_READ, None)
                else:
                    self.receive(key.fileobj)

    def receive(self, connection):
        try:
            chunk = connection.recv(1 << 20)
        except (BlockingIOError, InterruptedError):
            return
        except OSError:
            chunk = b""
        if not chunk:
            self.selector.unregister(connection)
            self.buffers.pop(connection, None)
            connection.close()
            return
        buffer = self.buffers[connection]
        buffer += chunk
        while True:
            end = buffer.find(b"\n")
            if end < 0:
                break
            line = bytes(buffer[:end])
            del buffer[:end + 1]
            if line.strip():
                self.history.handle(json.loads(line))

    def send(self, command):
        for connection in list(self.buffers):
            try:
                connection.setblocking(True)
                connection.sendall(command.encode() + b"\n")
                connection.setblocking(False)
            except OSError:
                pass


# ------------------------------------------------------------ worlds

def connect_world(args):
    if args.native:
        return None
    from seedbox import World
    return World.connect()


def trigger_status(world, trigger):
    return world.inspect()["triggers"][trigger.id - 1]


def nemesis_end_partition(world, loop, history, args, rng, end, brokers):
    from seedbox.control import FaultError
    faults = 0
    lo, hi = args.cut
    while time.monotonic() < end and faults < 60:
        window = round(max(1.0, end - time.monotonic()), 3)
        trigger = world.arm_console_partition(args.client, b"@@END ", [args.client], brokers, match_timeout=window)
        status = {"status": "armed"}
        while status["status"] == "armed" and time.monotonic() < end:
            loop.wait(timeout=0.1)
            status = trigger_status(world, trigger)
        if status["status"] == "armed":
            try:
                world.cancel_trigger(trigger)
            except FaultError:
                pass  # it matched meanwhile
            status = trigger_status(world, trigger)
        # A matched trigger applies its partition shortly afterwards.
        while status["status"] == "matched":
            loop.wait(timeout=0.01)
            status = trigger_status(world, trigger)
        if status["status"] != "fired":
            evidence("trigger", status=status["status"])
            break
        cut = rng.uniform(lo, hi)
        recent = history.ends[-1] if history.ends else None
        evidence("fault", action="partition", targets=[[args.client], brokers], cut=round(cut, 3),
                 matched_at_ns=status.get("matched_at_ns"), effect_at_ns=status.get("effect_at_ns"),
                 last_end=recent)
        loop.wait(timeout=cut)
        world.heal(trigger.partition)
        evidence("fault", action="heal", targets=[[args.client], brokers])
        faults += 1
        loop.wait(timeout=rng.uniform(*args.gap))
    return faults


def nemesis_jepsen(world, loop, history, args, rng, end, brokers):
    faults = 0
    while time.monotonic() < end:
        loop.wait(timeout=args.interval)
        if time.monotonic() >= end:
            break
        length = rng.uniform(2, 10)
        target = rng.choice(brokers)
        if rng.random() < 0.5:
            world.pause_process(target)
            evidence("fault", action="pause_process", targets=[target], length=round(length, 3))
            loop.wait(timeout=length)
            world.resume_process(target)
            evidence("fault", action="resume_process", targets=[target])
        else:
            handle = world.partition([args.client], [target])
            evidence("fault", action="partition", targets=[[args.client], [target]], length=round(length, 3))
            loop.wait(timeout=length)
            world.heal(handle)
            evidence("fault", action="heal", targets=[[args.client], [target]])
        faults += 1
    return faults


def nemesis_crash_all(world, loop, history, args, rng, brokers):
    if not loop.wait(lambda: history.acks >= args.crash_after, args.duration):
        evidence("crash", skipped=True, acks=history.acks)
        return 0
    acked = history.acks
    for broker in brokers:
        world.crash(broker)
    evidence("fault", action="crash", targets=brokers, acked_before=acked)
    loop.wait(timeout=2.0)
    for broker in brokers:
        world.reboot(broker)
    evidence("fault", action="reboot", targets=brokers)
    before = history.acks
    loop.wait(lambda: history.acks >= before + 100, 300)
    evidence("recovered", acks_after_reboot=history.acks - before)
    return 1


# ------------------------------------------------------------ checker

def check(events, workload):
    """Return (result, details, violation)."""
    from elle import list_append

    final = next((e for e in events if e.get("type") == "final"), None)
    logs = {}
    if final:
        for k, pairs in final["logs"].items():
            logs[int(k)] = [(int(o), int(v)) for o, v in pairs]
    complete = bool(final) and final.get("status") == "complete"
    position = {}   # (k, v) -> offset in the final read
    duplicates, inconsistent, nonmonotonic = [], [], []
    for k, pairs in logs.items():
        seen = {}
        last = -1
        for o, v in pairs:
            if o <= last:
                nonmonotonic.append({"key": k, "offset": o, "after": last})
            last = o
            if v in seen:
                duplicates.append({"key": k, "value": v, "offsets": [seen[v], o]})
            seen[v] = o
            position[(k, v)] = o
    by_offset = {(k, o): v for k, pairs in logs.items() for o, v in pairs}

    def observe(k, o, v, where):
        if o is None:
            return
        have = by_offset.get((k, o))
        if have is not None and have != v:
            inconsistent.append({"key": k, "offset": o, "value": v, "final": have, "where": where})

    lost, aborted_read, torn, aborted_poll, committed_fail, unseen = [], [], [], [], [], []
    lso = {int(k): int(v) for k, v in ((final or {}).get("end-offsets") or {}).items()}

    def beyond(k, o):
        return o is not None and k in lso and int(o) >= lso[k]
    ops = []  # completions of transactions
    stats = collections.Counter()
    txns = []
    failed_values = {}
    if workload == "produce":
        for e in events:
            if e.get("f") != "send" or e.get("type") == "invoke":
                continue
            k, v = int(e["k"]), int(e["v"])
            stats[e["type"]] += 1
            if e["type"] == "ok":
                observe(k, e.get("offset"), v, "ack")
                if (k, v) not in position and beyond(k, e.get("offset")):
                    unseen.append({"unseen": [(k, v)], "offset": e.get("offset")})
                elif (k, v) not in position:
                    lost.append({"key": k, "value": v, "offset": e.get("offset"), "t": e.get("t")})
            elif (k, v) in position:
                stats["info-visible"] += 1
    else:
        for e in events:
            if e.get("type") in ("ok", "fail", "info") and e.get("f") == "txn":
                ops.append(e)
        for e in ops:
            stats[e["type"]] += 1
            sends, attempted = [], []
            for m in e["value"]:
                if m[0] == "send":
                    k = int(m[1])
                    if isinstance(m[2], list):
                        o, v = m[2]
                        attempted.append((k, o, int(v)))
                        sends.append((k, int(v)))
                        observe(k, o, int(v), f"op {e['op']}")
                    else:
                        sends.append((k, int(m[2])))
                elif m[0] == "poll" and len(m) > 1:
                    for k, pairs in m[1].items():
                        for o, v in pairs:
                            observe(int(k), o, int(v), f"poll op {e['op']}")
            visible = [(k, v) for k, v in sends if (k, v) in position]
            missing = [(k, v) for k, o, v in attempted if (k, v) not in position]
            if e["type"] == "fail":
                for k, v in sends:
                    failed_values[(k, v)] = e["op"]
                if visible:
                    aborted_read.append({"op": e["op"], "txid": e.get("txid"), "visible": visible,
                                         "error": e.get("error")})
                    if missing:
                        torn.append({"op": e["op"], "type": "fail", "visible": visible, "missing": missing})
                    else:
                        committed_fail.append(e["op"])
            elif e["type"] == "ok" and complete:
                gone = [(k, v) for k, v in sends if (k, v) not in position]
                # A write at or above the partition's last stable offset is unseen, not lost:
                # an open transaction below it hides it from read_committed readers.
                hidden = [(k, v) for k, o, v in attempted if (k, v) not in position and beyond(k, o)]
                if hidden:
                    unseen.append({"op": e["op"], "txid": e.get("txid"), "unseen": hidden})
                    gone = [kv for kv in gone if kv not in hidden]
                if gone:
                    lost.append({"op": e["op"], "txid": e.get("txid"), "lost": gone, "visible": visible})
                    if visible:
                        torn.append({"op": e["op"], "type": "ok", "visible": visible, "missing": gone})
            elif e["type"] == "info":
                stats["info-visible" if visible else "info-invisible"] += 1
            txns.append(list_append.Txn(e["op"], e["type"],
                                        [["append", k, v] for k, v in sends], process=e.get("process")))
        for e in ops:
            for m in e["value"]:
                if m[0] == "poll" and len(m) > 1:
                    for k, pairs in m[1].items():
                        for o, v in pairs:
                            if (int(k), int(v)) in failed_values:
                                aborted_poll.append({"reader": e["op"], "writer": failed_values[(int(k), int(v))],
                                                     "key": int(k), "value": int(v), "offset": o})
    cycles = {}
    if txns and final:
        reader = list_append.Txn("final-read", "ok", [["r", k, [v for _, v in pairs]] for k, pairs in sorted(logs.items())])
        anomalies, _, _ = list_append.analyze(txns + [reader])
        cycles = {kind: len(items) for kind, items in anomalies.items()}
    result = dict(
        final=final.get("status") if final else None,
        records=sum(len(p) for p in logs.values()),
        ops=dict(stats),
        lost=len(lost), aborted_read=len(aborted_read), aborted_poll=len(aborted_poll), torn=len(torn),
        committed_fail=len(committed_fail), unseen=sum(len(u["unseen"]) for u in unseen),
        duplicates=len(duplicates), inconsistent=len(inconsistent),
        nonmonotonic=len(nonmonotonic), elle=cycles,
    )
    details = dict(lost=lost[:50], unseen=unseen[:50], aborted_read=aborted_read[:50], aborted_poll=aborted_poll[:50], torn=torn[:50],
                   duplicates=duplicates[:50], inconsistent=inconsistent[:50], nonmonotonic=nonmonotonic[:50])
    violation = any(result[k] for k in ("lost", "aborted_read", "aborted_poll", "torn", "duplicates",
                                        "inconsistent", "nonmonotonic"))
    return result, details, violation


def summarize(result, faults):
    return (f"{result['lost']} lost, {result['unseen']} unseen, {result['aborted_read']} aborted reads ({result['aborted_poll']} in polls), "
            f"{result['torn']} torn, {result['duplicates']} duplicates, {result['inconsistent']} inconsistent offsets; "
            f"ops {json.dumps(result['ops'], sort_keys=True)}; elle {json.dumps(result['elle'], sort_keys=True)}; "
            f"{result['records']} records; {faults} faults")


def check_log(path, workload):
    events = []
    with open(path) as source:
        for line in source:
            if line.startswith("H "):
                events.append(json.loads(line[2:]))
    result, details, violation = check(events, workload)
    print(json.dumps(dict(result=result, details=details, violation=violation), indent=1, sort_keys=True))
    return 1 if violation else 0


# ------------------------------------------------------------ main

def parse_range(text):
    lo, _, hi = text.partition(",")
    return float(lo), float(hi or lo)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--workload", choices=["txn", "produce"], default="txn")
    parser.add_argument("--nemesis", choices=["none", "end-partition", "jepsen", "crash-all"], default="none")
    parser.add_argument("--brokers", default="n1", help="Comma-separated broker machines.")
    parser.add_argument("--client", default="c1")
    parser.add_argument("--duration", type=float, default=60, help="Seconds of workload.")
    parser.add_argument("--warmup", type=float, default=5)
    parser.add_argument("--cut", type=parse_range, default=(3.5, 5.5), help="Partition length range, s.")
    parser.add_argument("--gap", type=parse_range, default=(2.0, 4.0), help="Quiet time range after a heal, s.")
    parser.add_argument("--interval", type=float, default=10, help="Jepsen nemesis interval, s.")
    parser.add_argument("--crash-after", type=int, default=1000, help="Acknowledgements before crash-all.")
    parser.add_argument("--port", type=int, default=7100)
    parser.add_argument("--native", action="store_true", help="No Seedbox: no faults.")
    parser.add_argument("--check-log", default="")
    args = parser.parse_args()
    if args.check_log:
        return check_log(args.check_log, args.workload)
    brokers = args.brokers.split(",")
    rng_seed = random.getrandbits(32)
    rng = random.Random(rng_seed)
    evidence("start", workload=args.workload, nemesis=args.nemesis, brokers=brokers, rng_seed=rng_seed,
             duration=args.duration, cut=args.cut, gap=args.gap)
    history = History()
    loop = Loop(history, args.port)
    world = connect_world(args)
    if not loop.wait(lambda: history.topic_ready, 600):
        print("VERDICT inconclusive: the client never created its topic", flush=True)
        return 3
    evidence("ready", hello=history.hello, topic=history.topic_ready)
    loop.send("start")
    loop.wait(timeout=args.warmup)
    end = time.monotonic() + args.duration
    faults = 0
    if args.nemesis == "end-partition":
        faults = nemesis_end_partition(world, loop, history, args, rng, end, brokers)
    elif args.nemesis == "jepsen":
        faults = nemesis_jepsen(world, loop, history, args, rng, end, brokers)
    elif args.nemesis == "crash-all":
        faults = nemesis_crash_all(world, loop, history, args, rng, brokers)
        end = time.monotonic()  # stop once the cluster has recovered
    loop.wait(timeout=max(0.0, end - time.monotonic()))
    evidence("nemesis", faults=faults)
    loop.wait(timeout=5)
    loop.send("stop")
    if not loop.wait(lambda: history.done, 400):
        evidence("client", done=False, final=bool(history.final))
    if world is not None:
        state = world.inspect()
        evidence("inspect", triggers=state.get("triggers"), machines=state.get("machines"))
    result, details, violation = check(history.events, args.workload)
    result["faults"] = faults
    evidence("check", **result)
    evidence("details", **details)
    summary = summarize(result, faults)
    if violation:
        print("VERDICT violation: " + summary, flush=True)
        return 1
    if result["final"] != "complete":
        print("VERDICT inconclusive: final read " + str(result["final"]) + "; " + summary, flush=True)
        return 3
    if result["unseen"]:
        print("VERDICT inconclusive: committed writes beyond an open transaction; " + summary, flush=True)
        return 3
    if args.nemesis != "none" and not faults:
        print("VERDICT inconclusive: no fault was applied; " + summary, flush=True)
        return 3
    print("VERDICT ok: " + summary, flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
