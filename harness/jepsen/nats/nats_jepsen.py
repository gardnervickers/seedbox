"""Jepsen's NATS JetStream 2.12.1 queue workload as a Seedbox oracle.

Five NATS servers n1..n5 hold one file-backed stream `jepsen-stream` (subjects
`jepsen.*`, R5 by default, discard new). Publisher threads, two per server and
each bound to its server, publish the unique values `P-C` (publisher, counter)
to `jepsen.0` and wait for the JetStream publish ack. Every ack is recorded
with its stream sequence. A timeout or error is indeterminate and never
retried. After the fault schedule, every fault is healed, every machine runs,
any server whose process has exited is started again (as Jepsen's final
generator does), and the oracle waits for the cluster to settle. It then reads the whole
stream from each server separately: an ephemeral R1 consumer is created until
its leader is that server, so it reads that server's own replica. The checker
reports, as Jepsen's queue checker does:

* lost: acknowledged values that no server holds (classified lost-prefix,
  lost-middle and lost-postfix per publisher, relative to its read values);
* divergent: acknowledged values that some servers hold and others lack, and
  stream sequences that hold different values on different servers;
* acknowledged sequence reuse: one stream sequence acknowledged for two values;
* stream-missing: a server, or the meta layer, that no longer knows the stream;
* unexpected: values that were never published.

Fault modes (machine names are fixed; `--mode` selects one):

* `coordinated` (nats-server#7564): crash every machine at once (a machine
  crash drops page cache and every unflushed disk sector), reboot them all.
* `pause-crash` (#7567, Jepsen's pause-kill nemesis): pause n1 n2, wait,
  crash n3, pause n4 n5 and resume n1 n2, reboot n3, wait, resume n4 n5.
* `part-crash` (#7567, Jepsen's part-kill nemesis): partition n1 n2 from
  the rest, wait, crash n3, heal, partition n4 n5 from the rest, reboot n3,
  wait, heal.
* `kill` (#6888): rounds of SIGKILL (Seedbox `stop`) and restart of a random
  one, minority, majority or all of the servers, from the oracle's seeded RNG.
* `kill-all` (#6888, as nats-server#5946 reproduced it): SIGKILL every server
  shortly after the stream is created, then restart them all.
* `none`: no faults.

Exit status: 0 when every check holds, 1 on a violation, 3 when the run is
inconclusive (for example no value was ever acknowledged). Each evidence line
starts with `EVIDENCE ` and is one JSON document; the last line is `VERDICT`.
`--native` replaces the Seedbox control API with a Docker-compatible engine
API socket so the same oracle runs against plain containers.
"""
import argparse
import collections
import http.client
import json
import os
import random
import socket
import sys
import threading
import time

NODES = ["n1", "n2", "n3", "n4", "n5"]
STREAM = "jepsen-stream"
SUBJECT = "jepsen.0"
CLIENT_PORT, HTTP_PORT = 4222, 8222
START = time.monotonic()


def now():
    return round(time.monotonic() - START, 3)


def evidence(kind, **fields):
    print("EVIDENCE " + json.dumps(dict(t=now(), kind=kind, **fields), sort_keys=True), flush=True)


# ------------------------------------------------------------ NATS protocol

class NatsError(RuntimeError):
    pass


class Timeout(NatsError):
    pass


class Nats:
    """One synchronous NATS connection bound to one server (no discovery or reconnect)."""

    def __init__(self, host, timeout=3.0):
        self.host = host
        self.sock = socket.create_connection((host, CLIENT_PORT), timeout=timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = bytearray()
        self.token = 0
        self.inbox = "_INBOX.%016x" % random.getrandbits(64)
        self.queue = collections.deque()  # messages that arrived while awaiting a reply
        deadline = time.monotonic() + timeout
        op = self._next(deadline)
        if op[0] != "INFO":
            raise NatsError(f"{host}: expected INFO, got {op[0]}")
        self.info = op[1]
        connect = {"verbose": False, "pedantic": False, "headers": True, "no_responders": True,
                   "protocol": 1, "lang": "python-raw", "version": "1", "name": "jepsen-oracle"}
        self._send(b"CONNECT " + json.dumps(connect).encode() + b"\r\nPING\r\n")
        while True:
            op = self._next(deadline)
            if op[0] == "PONG":
                break
        self._send(f"SUB {self.inbox}.> 1\r\n".encode())

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass

    def _send(self, data):
        try:
            self.sock.sendall(data)
        except OSError as error:
            raise NatsError(f"{self.host}: send failed: {error}") from error

    def _fill(self, deadline):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise Timeout(f"{self.host}: timeout")
        self.sock.settimeout(remaining)
        try:
            chunk = self.sock.recv(1 << 16)
        except socket.timeout as error:
            raise Timeout(f"{self.host}: timeout") from error
        except OSError as error:
            raise NatsError(f"{self.host}: recv failed: {error}") from error
        if not chunk:
            raise NatsError(f"{self.host}: connection closed")
        self.buf += chunk

    def _next(self, deadline):
        """Return the next protocol operation; payload frames are consumed whole."""
        while True:
            end = self.buf.find(b"\r\n")
            if end < 0:
                self._fill(deadline)
                continue
            line = bytes(self.buf[:end])
            word = line.split(b" ", 1)[0].upper()
            if word in (b"MSG", b"HMSG"):
                parts = line.decode().split(" ")
                subject, sid = parts[1], parts[2]
                if word == b"MSG":
                    reply = parts[3] if len(parts) == 5 else None
                    header_len, total = 0, int(parts[-1])
                else:
                    reply = parts[3] if len(parts) == 6 else None
                    header_len, total = int(parts[-2]), int(parts[-1])
                if len(self.buf) < end + 2 + total + 2:
                    self._fill(deadline)
                    continue
                body = bytes(self.buf[end + 2:end + 2 + total])
                del self.buf[:end + 2 + total + 2]
                return ("MSG", subject, sid, reply, body[:header_len], body[header_len:])
            del self.buf[:end + 2]
            if word == b"PING":
                self._send(b"PONG\r\n")
            elif word == b"INFO":
                return ("INFO", json.loads(line[5:]))
            elif word == b"PONG":
                return ("PONG",)
            elif word == b"-ERR":
                raise NatsError(f"{self.host}: {line.decode(errors='replace')}")

    def next_message(self, deadline):
        if self.queue:
            return self.queue.popleft()
        while True:
            op = self._next(deadline)
            if op[0] == "MSG":
                return op[1:]

    def request(self, subject, payload=b"", timeout=2.0):
        """Return (status, body); other messages meanwhile wait in `queue`."""
        self.token += 1
        reply = f"{self.inbox}.{self.token}"
        if isinstance(payload, str):
            payload = payload.encode()
        self._send(f"PUB {subject} {reply} {len(payload)}\r\n".encode() + payload + b"\r\n")
        deadline = time.monotonic() + timeout
        while True:
            op = self._next(deadline)
            if op[0] != "MSG":
                continue
            if op[1] == reply:
                return status(op[4]), op[5]
            self.queue.append(op[1:])

    def js(self, api, body=None, timeout=3.0):
        status_code, reply = self.request("$JS.API." + api, json.dumps(body) if body is not None else b"", timeout)
        if status_code == 503:
            raise NatsError(f"{self.host}: no responders for {api}")
        return json.loads(reply) if reply else {}


def status(headers):
    if not headers:
        return None
    line = headers.split(b"\r\n", 1)[0].split()
    return int(line[1]) if len(line) > 1 and line[1].isdigit() else None


def http_get(host, path, timeout=3.0):
    connection = http.client.HTTPConnection(host, HTTP_PORT, timeout=timeout)
    try:
        connection.request("GET", path)
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def healthz(host):
    try:
        code, body = http_get(host, "/healthz")
        return code == 200, body.decode(errors="replace")[:200]
    except (OSError, http.client.HTTPException) as error:
        return False, str(error)[:200]


def unreachable(host):
    try:
        socket.create_connection((host, CLIENT_PORT), timeout=3.0).close()
        return False
    except OSError as error:
        return getattr(error, "errno", None) in (111, 113)  # ECONNREFUSED, EHOSTUNREACH


def jsz(host):
    """This server's own view of the meta layer and its stream replica."""
    try:
        code, body = http_get(host, "/jsz?accounts=true&streams=true")
        document = json.loads(body)
    except (OSError, ValueError, http.client.HTTPException) as error:
        return {"error": str(error)[:200]}
    view = {"messages": document.get("messages"),
            "meta_leader": (document.get("meta_cluster") or {}).get("leader"),
            "streams": document.get("streams")}
    for account in document.get("account_details") or []:
        for stream in account.get("stream_detail") or []:
            if stream.get("name") == STREAM:
                state = stream.get("state") or {}
                view["stream"] = {key: state.get(key) for key in ("messages", "first_seq", "last_seq", "num_deleted")}
                view["stream_leader"] = (stream.get("cluster") or {}).get("leader")
    return view


# ------------------------------------------------------------ worlds

class NativeWorld:
    """The Seedbox World calls this oracle uses, over a Docker-compatible engine API socket."""

    def __init__(self, socket_path, network):
        self.path, self.network = socket_path, network
        self.partitions = {}

    def _post(self, path, body=None):
        channel = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        channel.connect(self.path)
        connection = http.client.HTTPConnection("localhost", timeout=60)
        connection.sock = channel
        data = json.dumps(body) if body is not None else None
        connection.request("POST", path, body=data, headers={"Content-Type": "application/json"})
        response = connection.getresponse()
        text = response.read()
        connection.close()
        if response.status >= 300 and response.status != 304:
            raise RuntimeError(f"engine {path}: {response.status} {text[:200]}")

    def sleep(self, seconds):
        time.sleep(seconds)

    def crash(self, target):
        self._post(f"/containers/{target}/kill?signal=KILL")

    def stop(self, target):
        self.crash(target)

    def reboot(self, target):
        self._post(f"/containers/{target}/start")

    def restart(self, target):
        self.reboot(target)

    def pause_process(self, target):
        self._post(f"/containers/{target}/pause")

    def resume_process(self, target):
        self._post(f"/containers/{target}/unpause")

    def partition(self, left, right):
        raise RuntimeError("native mode has no partitions")

    def heal(self, handle):
        raise RuntimeError("native mode has no partitions")


def connect_world(args):
    if args.native:
        return NativeWorld(args.native, args.native_network)
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from seedbox import World
    return World.connect()


# ------------------------------------------------------------ workload

class Publisher(threading.Thread):
    def __init__(self, index, node, interval, history, stop):
        super().__init__(daemon=True)
        self.index, self.node, self.interval = index, node, interval
        self.history, self.stop = history, stop
        self.counter = 0
        self.timed_out = {}

    def run(self):
        connection = None
        while not self.stop.is_set():
            if connection is None:
                try:
                    connection = Nats(self.node, timeout=2.0)
                except (OSError, NatsError):
                    connection = None
                    time.sleep(1.0)
                    continue
            value = f"{self.index}-{self.counter}"
            self.counter += 1
            self.history.invoke(value, self.index, self.node)
            try:
                code, body = connection.request(SUBJECT, value, timeout=2.0)
                self._stale(connection)
                self.history.complete(value, code, body)
            except Timeout:
                self.history.info(value, "timeout")
                self.timed_out[f"{connection.inbox}.{connection.token}"] = value
                self._stale(connection)
            except (OSError, NatsError) as error:
                self.history.info(value, "conn: " + str(error)[:80])
                connection.close()
                connection = None
                time.sleep(1.0)
                continue
            time.sleep(self.interval)
        if connection is not None:
            connection.close()

    def _stale(self, connection):
        while connection.queue:
            subject, _, _, _, body = connection.queue.popleft()
            if subject in self.timed_out:
                self.history.late(self.timed_out.pop(subject), body)


class History:
    def __init__(self):
        self.lock = threading.Lock()
        self.attempted = {}
        self.acked = {}       # value -> (stream seq, ack time)
        self.failed = {}
        self.indeterminate = {}
        self.late_acks = []
        self.errors = collections.Counter()

    def invoke(self, value, publisher, node):
        with self.lock:
            self.attempted[value] = (publisher, node, now())

    def complete(self, value, code, body):
        with self.lock:
            if code == 503:
                self.failed[value] = "no-responders"
                self.errors["no-responders"] += 1
                return
            try:
                ack = json.loads(body)
            except ValueError:
                self.indeterminate[value] = "bad-ack"
                return
            if "error" in ack:
                description = (ack["error"].get("description") or "")[:60]
                self.failed[value] = description
                self.errors[description] += 1
            elif ack.get("stream") == STREAM and isinstance(ack.get("seq"), int):
                self.acked[value] = (ack["seq"], now())
            else:
                self.indeterminate[value] = "odd-ack"

    def info(self, value, reason):
        with self.lock:
            self.indeterminate[value] = reason
            self.errors[reason.split(":")[0]] += 1

    def late(self, value, body):
        """A reply that arrived after its request timed out is still an ack."""
        with self.lock:
            try:
                ack = json.loads(body)
            except ValueError:
                return
            if ack.get("stream") == STREAM and isinstance(ack.get("seq"), int):
                self.acked[value] = (ack["seq"], now())
                self.indeterminate.pop(value, None)
                self.late_acks.append(value)

    def summary(self):
        with self.lock:
            return {"attempted": len(self.attempted), "acked": len(self.acked),
                    "failed": len(self.failed), "indeterminate": len(self.indeterminate),
                    "errors": dict(self.errors.most_common(6))}


# ------------------------------------------------------------ cluster operations

def any_node(nodes, call, attempts=30, pause=1.0):
    last = None
    for attempt in range(attempts):
        node = nodes[attempt % len(nodes)]
        try:
            connection = Nats(node)
        except (OSError, NatsError) as error:
            last = error
            time.sleep(pause)
            continue
        try:
            return node, call(connection)
        except (OSError, NatsError, ValueError) as error:
            last = error
        finally:
            connection.close()
        time.sleep(pause)
    raise NatsError(f"no server answered: {last}")


def wait_healthy(nodes, deadline_s, label):
    deadline = time.monotonic() + deadline_s
    status_by_node = {}
    while time.monotonic() < deadline:
        status_by_node = {node: healthz(node) for node in nodes}
        if all(ok for ok, _ in status_by_node.values()):
            evidence("healthy", label=label, nodes=nodes)
            return True
        time.sleep(1.0)
    evidence("unhealthy", label=label, status={n: s[1] for n, s in status_by_node.items() if not s[0]})
    return False


def create_stream(replicas):
    config = {"name": STREAM, "subjects": ["jepsen.*"], "storage": "file", "retention": "limits",
              "discard": "new", "num_replicas": replicas}

    def call(connection):
        reply = connection.js(f"STREAM.CREATE.{STREAM}", config, timeout=10.0)
        if "error" in reply and reply["error"].get("err_code") != 10058:  # 10058: already in use
            raise NatsError(json.dumps(reply["error"]))
        return reply

    node, reply = any_node(NODES, call, attempts=60)
    evidence("stream-created", via=node, cluster=reply.get("cluster"))


def stream_info(connection):
    reply = connection.js(f"STREAM.INFO.{STREAM}", timeout=3.0)
    return reply


def wait_stream_current(replicas, deadline_s):
    deadline = time.monotonic() + deadline_s
    while time.monotonic() < deadline:
        try:
            _, info = any_node(NODES, stream_info, attempts=1)
            cluster = info.get("cluster") or {}
            replicas_info = cluster.get("replicas") or []
            if cluster.get("leader") and len(replicas_info) == replicas - 1 and all(r.get("current") for r in replicas_info):
                evidence("stream-current", leader=cluster.get("leader"))
                return True
        except NatsError:
            pass
        time.sleep(1.0)
    return False


# ------------------------------------------------------------ fault schedules

def step(world, action, *targets):
    for target in targets:
        getattr(world, action)(target)
    evidence("fault", action=action, targets=list(targets))


def schedule_coordinated(world, args, rng):
    world.sleep(args.warmup)
    step(world, "crash", *NODES)
    world.sleep(1)
    step(world, "reboot", *NODES)
    wait_healthy(NODES, 120, "after-reboot")
    world.sleep(args.after)


def schedule_pause_crash(world, args, rng):
    world.sleep(args.warmup)
    step(world, "pause_process", "n1", "n2")
    world.sleep(args.hold)
    step(world, "crash", "n3")
    step(world, "pause_process", "n4", "n5")
    step(world, "resume_process", "n1", "n2")
    step(world, "reboot", "n3")
    world.sleep(args.hold)
    step(world, "resume_process", "n4", "n5")
    world.sleep(args.after)


def schedule_part_crash(world, args, rng):
    world.sleep(args.warmup)
    first = world.partition(["n1", "n2"], ["n3", "n4", "n5"])
    evidence("fault", action="partition", targets=[["n1", "n2"], ["n3", "n4", "n5"]])
    world.sleep(args.hold)
    step(world, "crash", "n3")
    world.heal(first)
    second = world.partition(["n4", "n5"], ["n1", "n2", "n3"])
    evidence("fault", action="heal+partition", targets=[["n4", "n5"], ["n1", "n2", "n3"]])
    step(world, "reboot", "n3")
    world.sleep(args.hold)
    world.heal(second)
    evidence("fault", action="heal", targets=[])
    world.sleep(args.after)


def schedule_kill(world, args, rng):
    """Jepsen's kill nemesis: SIGKILL a random target set, wait, restart it."""
    world.sleep(args.warmup)
    end = time.monotonic() + args.hold
    while time.monotonic() < end:
        kind = rng.choice(["one", "minority", "majority", "all"])
        count = {"one": 1, "minority": 2, "majority": 3, "all": 5}[kind]
        victims = sorted(rng.sample(NODES, count))
        evidence("meta-leader", views={node: jsz(node).get("meta_leader") for node in NODES})
        step(world, "stop", *victims)
        world.sleep(round(rng.uniform(1, 5), 3))
        step(world, "restart", *victims)
        world.sleep(round(rng.uniform(5, 10), 3))
    world.sleep(args.after)


def schedule_kill_all(world, args, rng):
    """nats-server#5946's reproduction of #6888: SIGKILL every server soon after the stream is added."""
    world.sleep(args.warmup)
    evidence("meta-leader", views={node: jsz(node).get("meta_leader") for node in NODES})
    step(world, "stop", *NODES)
    world.sleep(2)
    step(world, "restart", *NODES)
    world.sleep(args.hold + args.after)


SCHEDULES = {"coordinated": schedule_coordinated, "kill-all": schedule_kill_all, "pause-crash": schedule_pause_crash,
             "part-crash": schedule_part_crash, "kill": schedule_kill,
             "none": lambda world, args, rng: world.sleep(args.warmup + args.hold + args.after)}


# ------------------------------------------------------------ final reads

def read_node(node, deadline_s=60):
    """Read this server's own replica through an R1 consumer led by it."""
    deadline = time.monotonic() + deadline_s
    attempts = []
    while time.monotonic() < deadline:
        try:
            connection = Nats(node, timeout=5.0)
        except (OSError, NatsError) as error:
            attempts.append(f"connect: {error}"[:100])
            time.sleep(1.0)
            continue
        try:
            names = connection.js("STREAM.NAMES", {}, timeout=5.0)
            if "error" in names:
                attempts.append(json.dumps(names["error"])[:100])
                time.sleep(1.0)
                continue
            if STREAM not in (names.get("streams") or []):
                return {"stream_missing": True, "names": names.get("streams"), "error": names.get("error")}
            deliver = "_DELIVER.%016x" % random.getrandbits(64)
            connection._send(f"SUB {deliver} 2\r\n".encode())
            config = {"deliver_policy": "all", "ack_policy": "none", "replay_policy": "instant",
                      "num_replicas": 1, "mem_storage": True, "deliver_subject": deliver,
                      "inactive_threshold": 120_000_000_000}
            reply = connection.js(f"CONSUMER.CREATE.{STREAM}", {"stream_name": STREAM, "config": config}, timeout=10.0)
            if "error" in reply:
                if reply["error"].get("err_code") == 10059:
                    return {"stream_missing": True, "error": reply["error"]}
                attempts.append(json.dumps(reply["error"])[:100])
                time.sleep(1.0)
                continue
            name, leader = reply["name"], (reply.get("cluster") or {}).get("leader")
            if leader != node:
                # Placement is random among the stream's peers; try again.
                connection.js(f"CONSUMER.DELETE.{STREAM}.{name}", timeout=5.0)
                attempts.append(f"placed on {leader}")
                continue
            return collect(connection, name, deliver, node, attempts, deadline)
        except (OSError, NatsError, ValueError, KeyError) as error:
            attempts.append(str(error)[:100])
            time.sleep(1.0)
        finally:
            connection.close()
    return {"error": "no read", "attempts": attempts[-5:]}


def collect(connection, name, deliver, node, attempts, deadline):
    messages = {}
    quiet = time.monotonic() + 3.0
    while time.monotonic() < deadline:
        try:
            subject, sid, reply, headers, body = connection.next_message(min(quiet, deadline))
        except Timeout:
            info = connection.js(f"CONSUMER.INFO.{STREAM}.{name}", timeout=5.0)
            if info.get("num_pending") == 0:
                break
            quiet = time.monotonic() + 3.0
            continue
        if sid != "2" or not reply or not reply.startswith("$JS.ACK."):
            continue
        tokens = reply.split(".")
        # v1: $JS.ACK.<stream>.<consumer>.<delivered>.<sseq>.<cseq>.<ts>.<pending>
        # v2 adds domain and account hash after ACK and a trailing token.
        sseq = int(tokens[5] if len(tokens) == 9 else tokens[7])
        messages[sseq] = body.decode(errors="replace")
        quiet = time.monotonic() + 3.0
    try:
        connection.js(f"CONSUMER.DELETE.{STREAM}.{name}", timeout=5.0)
    except NatsError:
        pass
    return {"messages": messages, "placement_attempts": len(attempts)}


# ------------------------------------------------------------ checker

def ranges(numbers):
    numbers = sorted(set(numbers))
    out = []
    for number in numbers:
        if out and number == out[-1][1] + 1:
            out[-1][1] = number
        else:
            out.append([number, number])
    return [a if a == b else f"{a}-{b}" for a, b in out]


def check(attempted, acked, reads):
    """Pure checker; `reads` maps node -> {"messages": {seq: value}} or a failure."""
    read_ok = {node: r["messages"] for node, r in reads.items() if "messages" in r}
    missing_stream = sorted(node for node, r in reads.items() if r.get("stream_missing"))
    unread = sorted(node for node, r in reads.items() if "messages" not in r and not r.get("stream_missing"))
    held = {node: set(messages.values()) for node, messages in read_ok.items()}
    union = set().union(*held.values()) if held else set()
    acked_values = set(acked)
    lost = acked_values - union
    unexpected = union - set(attempted)
    per_node_missing = {node: sorted(acked_values - values, key=value_key) for node, values in held.items()}
    divergent = sorted({v for node, values in per_node_missing.items() for v in values} - lost, key=value_key)
    conflicts = {}
    for node, messages in read_ok.items():
        for seq, value in messages.items():
            conflicts.setdefault(seq, {}).setdefault(value, []).append(node)
    seq_conflicts = {seq: by_value for seq, by_value in conflicts.items() if len(by_value) > 1}
    duplicates = {node: len(messages) - len(set(messages.values())) for node, messages in read_ok.items()}
    acked_seq_moved = sorted(v for v in acked_values & union
                             if any(messages.get(acked[v][0]) not in (None, v) for messages in read_ok.values()))
    by_seq = collections.defaultdict(list)
    for value, (seq, _) in acked.items():
        by_seq[seq].append(value)
    seq_reused = {seq: sorted(values, key=value_key) for seq, values in by_seq.items() if len(values) > 1}
    classes = classify(acked_values, union, lost)
    result = {
        "acked": len(acked_values), "read_union": len(union), "lost": len(lost),
        "lost_classes": classes, "divergent": len(divergent), "unexpected": len(unexpected),
        "seq_conflicts": len(seq_conflicts), "acked_seq_moved": len(acked_seq_moved),
        "acked_seq_reused": len(seq_reused),
        "stream_missing": missing_stream, "unread": unread,
        "per_node": {node: {"messages": len(read_ok[node]), "missing_acked": len(per_node_missing[node]),
                            "duplicates": duplicates[node],
                            "last_seq": max(read_ok[node]) if read_ok[node] else 0}
                     for node in sorted(read_ok)},
    }
    details = {
        "lost_seqs": ranges(acked[v][0] for v in lost),
        "lost_values": sorted(lost, key=value_key)[:40],
        "lost_ack_time": [min(acked[v][1] for v in lost), max(acked[v][1] for v in lost)] if lost else None,
        "missing_by_node": {node: ranges(acked[v][0] for v in values) for node, values in per_node_missing.items() if values},
        "seq_conflict_ranges": ranges(seq_conflicts),
        "seq_conflict_examples": {seq: seq_conflicts[seq] for seq in sorted(seq_conflicts)[:5]},
        "unexpected_values": sorted(unexpected, key=value_key)[:20],
        "acked_seq_reused_ranges": ranges(seq_reused),
        "acked_seq_reused_examples": {seq: seq_reused[seq] for seq in sorted(seq_reused)[:5]},
    }
    violation = bool(lost or divergent or unexpected or seq_conflicts or missing_stream or acked_seq_moved or seq_reused)
    return result, details, violation


def value_key(value):
    publisher, counter = value.split("-")
    return int(publisher), int(counter)


def classify(acked_values, union, lost):
    by_publisher = collections.defaultdict(list)
    for value in acked_values:
        publisher, counter = value_key(value)
        by_publisher[publisher].append(counter)
    classes = collections.Counter()
    for publisher, counters in by_publisher.items():
        ok = [c for c in counters if f"{publisher}-{c}" in union]
        for counter in counters:
            if f"{publisher}-{counter}" not in lost:
                continue
            if not ok or counter > max(ok):
                classes["lost-postfix"] += 1
            elif counter < min(ok):
                classes["lost-prefix"] += 1
            else:
                classes["lost-middle"] += 1
    return dict(classes)


# ------------------------------------------------------------ main

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=sorted(SCHEDULES), required=True)
    parser.add_argument("--config", default="", help="Label for the server configuration in use.")
    parser.add_argument("--replicas", type=int, default=5)
    parser.add_argument("--publishers", type=int, default=10)
    parser.add_argument("--interval", type=float, default=0.05, help="Seconds between one publisher's publishes.")
    parser.add_argument("--warmup", type=float, default=30)
    parser.add_argument("--hold", type=float, default=30)
    parser.add_argument("--after", type=float, default=10)
    parser.add_argument("--settle", type=float, default=30, help="Seconds to wait for replicas to agree before reading.")
    parser.add_argument("--native", default="", help="Docker-compatible engine socket; no Seedbox.")
    parser.add_argument("--native-network", default="natsnet")
    args = parser.parse_args()
    rng_seed = random.getrandbits(32)
    rng = random.Random(rng_seed)
    evidence("start", mode=args.mode, rng_seed=rng_seed, config=args.config, replicas=args.replicas, publishers=args.publishers,
             interval=args.interval, warmup=args.warmup, hold=args.hold, after=args.after)
    world = connect_world(args)
    if not wait_healthy(NODES, 180, "boot"):
        print("VERDICT inconclusive: cluster never became healthy", flush=True)
        return 3
    _, js_info = any_node(NODES, lambda c: c.js("INFO", timeout=5.0), attempts=60)
    evidence("jetstream", meta=js_info.get("meta_cluster") or js_info.get("cluster"))
    create_stream(args.replicas)
    wait_stream_current(args.replicas, 60)
    history, stop = History(), threading.Event()
    publishers = [Publisher(i, NODES[i % len(NODES)], args.interval, history, stop) for i in range(args.publishers)]
    for publisher in publishers:
        publisher.start()
    progress_stop = threading.Event()

    def progress():
        while not progress_stop.wait(5.0):
            evidence("progress", **history.summary())

    threading.Thread(target=progress, daemon=True).start()
    SCHEDULES[args.mode](world, args, rng)
    stop.set()
    for publisher in publishers:
        publisher.join(10)
    progress_stop.set()
    summary = history.summary()
    evidence("history", **summary, late_acks=len(history.late_acks))
    if hasattr(world, "inspect"):
        evidence("inspect", world=world.inspect())
    healthy = wait_healthy(NODES, 120, "final")
    if not healthy:
        # Jepsen's final generator starts every node. Every fault is healed now,
        # so a server that refuses or cannot route connections has exited
        # (for example, killed by the guest's OOM killer): start it again.
        exited = [node for node in NODES if unreachable(node)]
        for node in exited:
            try:
                step(world, "restart", node)
            except Exception as error:  # Keep the evidence; the reads still run.
                evidence("restart-failed", node=node, error=str(error)[:200])
        if exited:
            healthy = wait_healthy(NODES, 120, "final-after-restart")
    # Let replicas converge: read once every server reports the same stream
    # state for five seconds, or after --settle seconds regardless.
    deadline, agreed_since, views = time.monotonic() + args.settle, None, {}
    while time.monotonic() < deadline:
        views = {node: jsz(node) for node in NODES}
        if len({json.dumps(v.get("stream"), sort_keys=True) for v in views.values()}) == 1:
            agreed_since = agreed_since or time.monotonic()
            if time.monotonic() - agreed_since >= 5:
                break
        else:
            agreed_since = None
        time.sleep(1.0)
    evidence("jsz", healthy=healthy, views=views)
    reads = {}
    for node in NODES:
        reads[node] = read_node(node)
        result = reads[node]
        evidence("read", node=node, messages=len(result.get("messages", {})),
                 **{k: v for k, v in result.items() if k != "messages"})
    with history.lock:
        attempted, acked = dict(history.attempted), dict(history.acked)
    if not acked:
        print("VERDICT inconclusive: no publish was acknowledged", flush=True)
        return 3
    result, details, violation = check(attempted, acked, reads)
    evidence("check", **result)
    evidence("details", **details)
    if violation:
        problems = [name for name in ("lost", "divergent", "unexpected", "seq_conflicts", "acked_seq_moved", "acked_seq_reused")
                    if result[name]] + (["stream_missing"] if result["stream_missing"] else [])
        print("VERDICT violation: " + ", ".join(f"{p}={result[p] if not isinstance(result[p], list) else result[p]}"
                                               for p in problems), flush=True)
        return 1
    if result["unread"]:
        print(f"VERDICT inconclusive: unread nodes {result['unread']}", flush=True)
        return 3
    print(f"VERDICT ok: {result['acked']} acked values present on every node", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
