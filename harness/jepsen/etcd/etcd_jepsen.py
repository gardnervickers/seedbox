"""Jepsen's etcd 3.4.3 lock-set workload as a Seedbox oracle.

Five etcd members n1..n5 form one cluster. Five client machines c1..c5 run
`etcd_lock_client.py`, client ci bound to member ni. Each client operation
grants a lease (TTL 2 s by default) kept alive every TTL/3, takes etcd's lock
`lock` with it, reads a set, sleeps one second, writes the set plus a unique
element, then releases the lock. This oracle hosts the set (Jepsen keeps it in
the test process) in a small HTTP store on port 8000, and every client step is
reported to the store, so the store's log is one total order of the history.
The oracle and the clients each run on a single thread.

Fault modes (`--mode`; Seedbox faults, recorded as `fault` evidence):

* `none`: no faults.
* `pause-holder`: when the lock holder has read the set, SIGSTOP the holder's
  client process (a GC pause) for `--pause` s, longer than the TTL, then
  SIGCONT it; wait `--gap` s and repeat.
* `pause-waiter-member` (etcd#11456 through a pause): when the holder has read
  the set, pause the member of a client waiting for the lock (not the leader,
  not the holder's member) for `--pause` s, so the waiter's lease expires while
  its Lock RPC is pending on that member.
* `pause-random-member`: Jepsen's nemesis: pause a random member's process for
  `--pause` s every `--pause` + `--gap` s.
* `partition-waiter` (etcd#11456): while one client holds the lock, isolate
  the member of a client that is waiting for it (not the leader, not the
  holder's member) from the other members for `--pause` s, then heal.

`--guard` must match the clients' guard: `none` (plain reads and writes),
`token` (the store fences operations carrying a lower lock-key revision than
one it has seen) or `txn` (the set lives in etcd and the write is a
transaction on the lock key's existence).

After the schedule every fault is healed, clients stop, and the checker
reports, as Jepsen's lock-set checker does, the acknowledged elements missing
from the final set (lost updates). It also reports mutual-exclusion
violations: overlapping lock holds in the store's order (from a client's
`acquired` report to its `releasing` report); read-modify-writes that another
client's write interleaved; and stale grants, Lock RPC replies whose lock key
etcd had already deleted at or before the reply's revision (etcd#11456), from
the cluster's full revision history.

The run also records etcd#11496: a watch from revision 0 starts after the
current revision, while revision 1 replays the history.

Exit status: 0 when the checks hold, 1 on a violation (lost updates; or for
unguarded clients, overlapping holds), 3 when inconclusive. Evidence lines
start with `EVIDENCE `; the last line is `VERDICT`. `--native` replaces the
Seedbox control API with a Docker-compatible engine socket (no partitions).
"""
import argparse
import base64
import collections
import http.client
import json
import os
import random
import selectors
import socket
import sys
import time

MEMBERS = ["n1", "n2", "n3", "n4", "n5"]
CLIENTS = {f"c{i}": f"n{i}" for i in range(1, 6)}
START = time.monotonic()


def now():
    return round(time.monotonic() - START, 3)


def evidence(kind, **fields):
    print("EVIDENCE " + json.dumps(dict(t=now(), kind=kind, **fields), sort_keys=True), flush=True)


def log(message):
    print(f"[{now():9.3f}] {message}", flush=True)


def b64(text):
    return base64.b64encode(text.encode()).decode()


def unb64(text):
    return base64.b64decode(text).decode()



# ------------------------------------------------------------ the store and its event loop

class Store:
    """The protected set, the shared history and the client registry."""

    def __init__(self, guard):
        self.guard = guard
        self.history = []
        self.set = []
        self.max_token = 0
        self.hello, self.done = {}, {}
        self.start, self.stop = False, False
        self.holders = {}   # client -> {"key", "op", "read"}
        self.waiting = {}   # client -> op
        self.last_read = None  # (history index, client)

    def handle(self, body):
        kind, client = body.get("type"), body.get("client")
        if kind == "hello":
            self.hello[client] = body.get("member")
            return {"start": self.start}
        if kind == "next":
            return {"stop": self.stop}
        if kind == "done":
            self.done[client] = body.get("ops")
            return {}
        entry = dict(body, t=now(), i=len(self.history))
        reply = {"ok": True}
        if kind == "read" and self.guard != "txn":
            if not self.fence(entry):
                reply = {"ok": False}
            else:
                reply["set"] = list(self.set)
            entry["size"] = len(self.set)
        elif kind == "write" and self.guard != "txn":
            if self.fence(entry):
                self.set = list(body["set"])
            else:
                reply = {"ok": False}
            entry.pop("set", None)
        if kind in ("read", "write") and self.guard != "txn":
            entry["ok"] = reply["ok"]
        else:  # txn clients report their own transaction's outcome
            entry["ok"] = entry.get("ok")
        if kind == "invoke":
            self.waiting[client] = body.get("op")
        elif kind == "acquired":
            self.waiting.pop(client, None)
            self.holders[client] = {"key": body.get("key"), "op": body.get("op"), "read": False}
        elif kind == "read" and client in self.holders:
            self.holders[client]["read"] = True
            self.last_read = (entry["i"], client)
        elif kind in ("releasing", "fail", "info") and body.get("op") == self.waiting.get(client):
            self.waiting.pop(client, None)
        if kind == "releasing":
            self.holders.pop(client, None)
        self.history.append(entry)
        print("H " + json.dumps(entry, sort_keys=True), flush=True)
        return reply

    def fence(self, entry):
        if self.guard != "token":
            return True
        token = entry.get("token")
        if not isinstance(token, int) or token < self.max_token:
            entry["fenced"] = True
            return False
        self.max_token = token
        return True


class Loop:
    """A single-threaded HTTP/1.0 server for the store. The oracle runs on one
    thread: it serves requests whenever it waits (`wait`), and control RPCs and
    etcd queries run between waits. Clients send one short request per
    connection and retry, so a request queued meanwhile is served late, never lost."""

    def __init__(self, store, port):
        self.store = store
        self.selector = selectors.DefaultSelector()
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("0.0.0.0", port))
        self.listener.listen(128)
        self.listener.setblocking(False)
        self.selector.register(self.listener, selectors.EVENT_READ, None)
        self.buffers = {}

    def wait(self, predicate=None, timeout=0.0):
        """Serve requests until `predicate()` is true or `timeout` seconds pass; return its value."""
        deadline = time.monotonic() + timeout
        while True:
            value = predicate() if predicate else None
            remaining = deadline - time.monotonic()
            if value or remaining <= 0:
                return value
            for key, _ in self.selector.select(min(remaining, 0.5)):
                if key.fileobj is self.listener:
                    self.accept()
                else:
                    self.receive(key.fileobj)

    def accept(self):
        while True:
            try:
                connection, _ = self.listener.accept()
            except (BlockingIOError, InterruptedError):
                return
            connection.setblocking(False)
            self.buffers[connection] = (bytearray(), time.monotonic())
            self.selector.register(connection, selectors.EVENT_READ, None)

    def close(self, connection):
        self.selector.unregister(connection)
        self.buffers.pop(connection, None)
        connection.close()

    def receive(self, connection):
        buffer, _ = self.buffers[connection]
        try:
            chunk = connection.recv(65536)
        except (BlockingIOError, InterruptedError):
            return
        except OSError:
            self.close(connection)
            return
        if not chunk:
            self.close(connection)
            return
        buffer += chunk
        end = buffer.find(b"\r\n\r\n")
        if end < 0:
            return
        headers = bytes(buffer[:end]).decode("latin-1").split("\r\n")
        length = 0
        for line in headers[1:]:
            name, _, value = line.partition(":")
            if name.strip().lower() == "content-length":
                length = int(value.strip())
        if len(buffer) < end + 4 + length:
            return
        try:
            body = json.loads(bytes(buffer[end + 4:end + 4 + length]) or b"{}")
            data, code = json.dumps(self.store.handle(body)).encode(), b"200 OK"
        except (ValueError, KeyError, TypeError) as error:
            data, code = json.dumps({"error": str(error)}).encode(), b"400 Bad Request"
        response = (b"HTTP/1.0 " + code + b"\r\nContent-Type: application/json\r\nContent-Length: "
                    + str(len(data)).encode() + b"\r\n\r\n" + data)
        try:
            connection.setblocking(True)
            connection.settimeout(2.0)
            connection.sendall(response)
        except OSError:
            pass
        self.close(connection)


# ------------------------------------------------------------ etcd gateway

def post(member, path, body, timeout=5.0):
    connection = http.client.HTTPConnection(member, 2379, timeout=timeout)
    try:
        connection.request("POST", path, json.dumps(body), {"Content-Type": "application/json"})
        response = connection.getresponse()
        data = response.read()
        if response.status != 200:
            raise RuntimeError(f"{member}{path}: HTTP {response.status} {data[:200]!r}")
        return json.loads(data)
    finally:
        connection.close()


def status(member, timeout=2.0):
    try:
        return post(member, "/v3/maintenance/status", {}, timeout)
    except (OSError, RuntimeError, ValueError, http.client.HTTPException):
        return None


def leader_member(timeout=0.5):
    """The member name of the current leader, as reachable members report it."""
    ids, leader = {}, None
    for member in MEMBERS:
        reply = status(member, timeout)
        if reply:
            ids[reply["header"]["member_id"]] = member
            leader = leader or reply.get("leader")
    return ids.get(leader)


def wait_cluster(loop, deadline_s, label):
    deadline = time.monotonic() + deadline_s
    while time.monotonic() < deadline:
        replies = {m: status(m) for m in MEMBERS}
        leaders = {r.get("leader") for r in replies.values() if r}
        if all(replies.values()) and len(leaders) == 1 and not leaders & {None, "0"}:
            evidence("cluster", label=label, leader=leader_member(),
                     revision=int(next(iter(replies.values()))["header"]["revision"]))
            return True
        loop.wait(timeout=0.5)
    evidence("cluster-unhealthy", label=label, replies={m: bool(status(m)) for m in MEMBERS})
    return False


def any_member(path, body, timeout=5.0):
    last = None
    for _ in range(20):
        for member in MEMBERS:
            try:
                return post(member, path, body, timeout)
            except (OSError, RuntimeError, ValueError, http.client.HTTPException) as error:
                last = error
        time.sleep(0.5)
    raise RuntimeError(f"{path}: no member answered: {last}")


class Watch:
    """A gateway watch on [key, range_end) from `start_revision`, read on the calling thread."""

    def __init__(self, member, key, range_end, start_revision, timeout=30.0):
        self.events = []
        self.connection = http.client.HTTPConnection(member, 2379, timeout=timeout)
        request = {"key": b64(key), "range_end": b64(range_end)}
        if start_revision is not None:
            request["start_revision"] = str(start_revision)
        self.connection.request("POST", "/v3/watch", json.dumps({"create_request": request}),
                                {"Content-Type": "application/json"})
        self.response = self.connection.getresponse()
        self.created = False

    def read_until(self, until):
        """Read responses until `until(self)` holds; raise on a closed stream or a timeout."""
        while not until(self):
            line = self.response.readline()
            if not line:
                raise RuntimeError("watch stream ended")
            result = json.loads(line).get("result", {})
            self.created = self.created or bool(result.get("created"))
            for event in result.get("events", []):
                kv = event["kv"]
                self.events.append({"type": event.get("type", "PUT"), "key": unb64(kv["key"]),
                                    "mod_revision": int(kv.get("mod_revision", 0)),
                                    "create_revision": int(kv.get("create_revision", 0)),
                                    "lease": kv.get("lease")})
        return self.events

    def close(self):
        self.connection.close()


def wait_keepalives(loop, deadline_s):
    """Wait until every member renews a lease promptly: after boot, followers can take
    seconds to forward renewals to the leader, which by itself expires 2 s leases."""
    deadline, streak = time.monotonic() + deadline_s, 0
    lease = int(any_member("/v3/lease/grant", {"TTL": 10})["ID"])
    while time.monotonic() < deadline:
        try:
            for member in MEMBERS:
                post(member, "/v3/lease/keepalive", {"ID": str(lease)}, timeout=0.3)
            streak += 1
        except (OSError, RuntimeError, ValueError, http.client.HTTPException):
            streak = 0
        if streak >= 3:
            any_member("/v3/lease/revoke", {"ID": str(lease)})
            evidence("keepalives-ready")
            return True
        loop.wait(timeout=0.5)
    evidence("keepalives-not-ready")
    return False


def watch_revision_zero():
    """etcd#11496: watch(rev 0) streams only new events; watch(rev 1) replays all of them."""
    any_member("/v3/kv/put", {"key": b64("w/1"), "value": b64("1")})
    any_member("/v3/kv/put", {"key": b64("w/2"), "value": b64("2")})
    keys, errors, watches = {}, {}, {}
    try:
        for rev in (0, 1):
            watches[rev] = Watch("n1", "w/", "w0", rev, timeout=20)
            watches[rev].read_until(lambda w: w.created)
        any_member("/v3/kv/put", {"key": b64("w/3"), "value": b64("3")})
        for rev, watch in watches.items():
            watch.read_until(lambda w: any(e["key"] == "w/3" for e in w.events))
    except (OSError, RuntimeError, ValueError, http.client.HTTPException) as error:
        errors["watch"] = repr(error)
    for rev, watch in watches.items():
        keys[rev] = [e["key"] for e in watch.events]
        watch.close()
    evidence("watch", rev0_keys=keys.get(0), rev1_keys=keys.get(1), errors=errors,
             rev0_starts_after_current=keys.get(0) == ["w/3"], rev1_replays=keys.get(1) == ["w/1", "w/2", "w/3"])


def revision_history():
    """Every event in the cluster's history, through a watch from revision 1 up to a marker put."""
    marker = int(any_member("/v3/kv/put", {"key": b64("zz-end"), "value": b64("end")})["header"]["revision"])
    for member in MEMBERS:
        try:
            watch = Watch(member, "\0", "\0", 1, timeout=60)
            events = watch.read_until(lambda w: any(e["mod_revision"] >= marker for e in w.events))
            watch.close()
            return events, marker
        except (OSError, RuntimeError, ValueError, http.client.HTTPException) as error:
            evidence("history-watch-failed", member=member, error=repr(error))
    return None, marker


# ------------------------------------------------------------ worlds

class NativeWorld:
    """The Seedbox World calls this oracle uses, over a Docker-compatible engine API socket."""

    def __init__(self, socket_path):
        self.path = socket_path

    def _post(self, path):
        channel = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        channel.connect(self.path)
        connection = http.client.HTTPConnection("localhost", timeout=60)
        connection.sock = channel
        connection.request("POST", path)
        response = connection.getresponse()
        text = response.read()
        connection.close()
        if response.status >= 300 and response.status != 304:
            raise RuntimeError(f"engine {path}: {response.status} {text[:200]}")

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
        return NativeWorld(args.native)
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from seedbox import World
    return World.connect()


# ------------------------------------------------------------ nemesis

def holder_reading(store, since=0):
    """A client holding the lock that read the set at history index `since` or later, still holding."""
    if store.last_read and store.last_read[0] >= since and store.last_read[1] in store.holders:
        return store.last_read[1]
    return None


def pause(world, loop, target, seconds, **context):
    world.pause_process(target)
    evidence("fault", action="pause_process", targets=[target], **context)
    loop.wait(timeout=seconds)
    world.resume_process(target)
    evidence("fault", action="resume_process", targets=[target])


def nemesis(world, loop, store, args, rng, end):
    """Run the fault schedule until `end`; the store is served whenever the schedule waits."""
    count = 0
    while time.monotonic() < end:
        window = min(10.0, max(0.1, end - time.monotonic()))
        if args.mode == "none":
            loop.wait(timeout=max(0.0, end - time.monotonic()))
            break
        if args.mode == "pause-random-member":
            target = rng.choice(MEMBERS)
            pause(world, loop, target, args.pause, leader=leader_member())
            count += 1
            loop.wait(timeout=args.gap)
            continue
        since = len(store.history)
        if args.mode == "pause-holder":
            holder = loop.wait(lambda: holder_reading(store, since), window)
            if not holder:
                continue
            pause(world, loop, holder, args.pause, holder=holder, key=store.holders.get(holder, {}).get("key"))
            count += 1
            loop.wait(timeout=args.gap)
            continue
        leader = leader_member()

        def pick():
            holder = holder_reading(store, since)
            if not holder or not leader:
                return None
            waiters = sorted(c for c in store.waiting if c != holder and CLIENTS[c] not in (leader, CLIENTS[holder]))
            return (holder, waiters[0]) if waiters else None

        if args.mode == "pause-waiter-member":
            chosen = loop.wait(pick, window)
            if not chosen:
                continue
            holder, waiter = chosen
            pause(world, loop, CLIENTS[waiter], args.pause, holder=holder, waiter=waiter, leader=leader,
                  waiter_op=store.waiting.get(waiter))
            count += 1
            loop.wait(timeout=args.gap)
            continue
        if args.mode == "partition-waiter":
            chosen = loop.wait(pick, window)
            if not chosen:
                continue
            holder, waiter = chosen
            isolated = CLIENTS[waiter]
            others = [m for m in MEMBERS if m != isolated]
            handle = world.partition([isolated], others)
            evidence("fault", action="partition", targets=[[isolated], others], holder=holder, waiter=waiter,
                     leader=leader, waiter_op=store.waiting.get(waiter))
            loop.wait(timeout=args.pause)
            world.heal(handle)
            evidence("fault", action="heal", targets=[[isolated], others])
            count += 1
            loop.wait(timeout=args.gap)
            continue
        raise ValueError(args.mode)
    return count


# ------------------------------------------------------------ checker

def acquisitions(history):
    """Lock holds in store order: (client, op) -> acquired/read/write/releasing entries."""
    holds = collections.OrderedDict()
    for entry in history:
        key = (entry.get("client"), entry.get("op"))
        if entry["type"] == "acquired":
            holds[key] = {"client": key[0], "op": key[1], "start": entry["i"], "end": None, "entry": entry,
                          "read": None, "write": None}
        elif entry["type"] in ("read", "write"):
            for hold in reversed(holds.values()):
                if hold["client"] == entry["client"] and hold["entry"].get("key") == entry.get("key"):
                    hold[entry["type"]] = entry
                    break
        elif entry["type"] == "releasing" and key in holds:
            holds[key]["end"] = entry["i"]
    end = len(history)
    for hold in holds.values():
        if hold["end"] is None:
            hold["end"] = end
    return list(holds.values())


def check(history, final_set, guard, lock_events):
    acked = [e["element"] for e in history if e["type"] == "ok"]
    final = set(final_set or [])
    lost = [element for element in acked if element not in final]
    holds = acquisitions(history)
    overlaps = []
    for index, hold in enumerate(holds):
        for other in holds[index + 1:]:
            if other["start"] >= hold["end"]:
                continue
            if other["client"] != hold["client"]:
                overlaps.append([f"{hold['client']}/{hold['op']}", f"{other['client']}/{other['op']}"])
    in_overlap = sorted({name for pair in overlaps for name in pair})
    writes = [e for e in history if e["type"] == "write" and e.get("ok")]
    interleaved = []
    for hold in holds:
        read, write = hold["read"], hold["write"]
        if read and write and write.get("ok"):
            between = [w for w in writes if read["i"] < w["i"] < write["i"] and w["client"] != hold["client"]]
            if between:
                interleaved.append({"hold": f"{hold['client']}/{hold['op']}",
                                    "by": [f"{w['client']}/{w.get('element')}" for w in between]})
    deleted = {}
    created = {}
    for event in lock_events or []:
        if not event["key"].startswith("lock/"):
            continue
        if event["type"] == "DELETE":
            deleted.setdefault(event["key"], event["mod_revision"])
        else:
            created.setdefault(event["key"], event["create_revision"])
    stale = []
    for hold in holds:
        entry = hold["entry"]
        gone = deleted.get(entry.get("key"))
        if gone is not None and gone <= entry.get("granted_revision", 0):
            stale.append({"hold": f"{hold['client']}/{hold['op']}", "key": entry.get("key"),
                          "deleted_revision": gone, "granted_revision": entry.get("granted_revision"),
                          "create_revision": created.get(entry.get("key"))})
    fenced = [e for e in history if e.get("fenced") or (e["type"] == "fail" and "fenced" in str(e.get("stage")))]
    keepalive_expired = sum(1 for e in history if e["type"] == "releasing"
                            and (e.get("keepalives") or {}).get("expired"))
    result = {
        "guard": guard, "acked": len(acked), "lost": len(lost),
        "lost_pct": round(100.0 * len(lost) / len(acked), 2) if acked else None,
        "final_size": len(final), "holds": len(holds), "overlap_pairs": len(overlaps),
        "holds_in_overlap": len(in_overlap), "interleaved_rmw": len(interleaved),
        "stale_grants": len(stale), "key_missing_at_grant": sum(1 for h in holds if h["entry"].get("key_present") is False),
        "fenced": len(fenced), "failed": sum(1 for e in history if e["type"] == "fail"),
        "indeterminate": sum(1 for e in history if e["type"] == "info"),
        "holds_with_expired_keepalive": keepalive_expired,
        "lock_keys_created": len(created), "lock_keys_deleted": len(deleted),
    }
    details = {"lost_elements": lost, "overlaps": overlaps[:50], "interleaved": interleaved[:50], "stale": stale[:50]}
    violation = bool(lost) or (guard == "none" and bool(overlaps))
    return result, details, violation


# ------------------------------------------------------------ main

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=["none", "pause-holder", "pause-waiter-member", "pause-random-member",
                                           "partition-waiter"], required=True)
    parser.add_argument("--guard", choices=["none", "token", "txn"], default="none")
    parser.add_argument("--duration", type=float, default=120, help="Seconds of workload.")
    parser.add_argument("--pause", type=float, default=3.0, help="Seconds each fault lasts.")
    parser.add_argument("--gap", type=float, default=2.0, help="Seconds between faults.")
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--native", default="", help="Docker-compatible engine socket; no Seedbox.")
    args = parser.parse_args()
    rng_seed = random.getrandbits(32)
    rng = random.Random(rng_seed)
    evidence("start", mode=args.mode, guard=args.guard, rng_seed=rng_seed, duration=args.duration,
             pause=args.pause, gap=args.gap)
    store = Store(args.guard)
    loop = Loop(store, args.port)
    world = connect_world(args)
    if not wait_cluster(loop, 240, "boot"):
        print("VERDICT inconclusive: cluster never became healthy", flush=True)
        return 3
    wait_keepalives(loop, 120)
    watch_revision_zero()
    if args.guard == "txn":
        any_member("/v3/kv/put", {"key": b64("set"), "value": b64("[]")})
    if not loop.wait(lambda: len(store.hello) == len(CLIENTS), 240):
        evidence("clients", hello=sorted(store.hello))
        print("VERDICT inconclusive: not every client reached the store", flush=True)
        return 3
    store.start = True
    evidence("clients", hello=store.hello)
    end = time.monotonic() + args.duration
    faults = nemesis(world, loop, store, args, rng, end)
    evidence("nemesis", faults=faults)
    store.stop = True
    loop.wait(lambda: len(store.done) == len(CLIENTS), 90)
    evidence("clients-done", done=store.done)
    if hasattr(world, "inspect"):
        evidence("inspect", world=world.inspect())
    wait_cluster(loop, 120, "final")
    if args.guard == "txn":
        kvs = any_member("/v3/kv/range", {"key": b64("set")}).get("kvs", [])
        final_set = json.loads(unb64(kvs[0]["value"])) if kvs else []
    else:
        final_set = list(store.set)
    lock_events, marker = revision_history()
    if lock_events is not None:
        evidence("lock-keys", events=[e for e in lock_events if e["key"].startswith("lock/")], revision=marker)
    result, details, violation = check(list(store.history), final_set, args.guard, lock_events)
    result["faults"] = faults
    result["history_complete"] = lock_events is not None
    evidence("check", **result)
    evidence("details", **details)
    if not result["acked"]:
        print("VERDICT inconclusive: no update was acknowledged", flush=True)
        return 3
    summary = (f"{result['lost']}/{result['acked']} acked updates lost ({result['lost_pct']}%), "
               f"{result['overlap_pairs']} overlapping holds, {result['interleaved_rmw']} interleaved, "
               f"{result['stale_grants']} stale grants, {result['fenced']} fenced, {faults} faults")
    if violation:
        print("VERDICT violation: " + summary, flush=True)
        return 1
    if args.mode != "none" and not faults:
        print("VERDICT inconclusive: no fault was applied; " + summary, flush=True)
        return 3
    print("VERDICT ok: " + summary, flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
