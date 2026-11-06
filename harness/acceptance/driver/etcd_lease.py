"""etcd lease expiry across a client partition (NEXT-100 case 100).

Three etcd members (n1-n3) form a cluster. The client machine runs
etcd_lease_client.sh: it grants a 5 s lease, attaches lk/a and lk/b to it,
publishes the lease ID and runs `etcdctl lease keep-alive`, a clientv3 gRPC
stream that renews the lease about every TTL/3.

The oracle uses the members' JSON gateway. It watches the lk/ prefix on n2
from before the partition, then:

1. confirms the keep-alives: for more than two TTLs the lease's remaining TTL
   never falls below 2 s and both keys stay attached;
2. partitions the client from all three members, so renewals stop while the
   quorum stays healthy;
3. requires the leader to expire the lease: the watch reports both keys
   deleted in one revision (one revoke), between TTL*2/3 - 1 s and TTL + 2 s
   after the partition, and the lease is then unknown;
4. heals the partition. The client's keep-alive has given up; its report of
   how it ended can only be written once gRPC has reconnected, after the
   deletion. All three members must be healthy, agree on one leader and on
   the current revision, and lk/ must be empty.

Timings, revisions and the client's report are printed as evidence.
"""
import base64
import hashlib
import http.client
import json
import threading
import time

MEMBERS = ("n1", "n2", "n3")
TTL = 5


def b64(text):
    return base64.b64encode(text.encode()).decode()


def unb64(text):
    return base64.b64decode(text).decode()


def post(member, path, body, timeout=10):
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


def healthy(member):
    connection = http.client.HTTPConnection(member, 2379, timeout=10)
    try:
        connection.request("GET", "/health")
        response = connection.getresponse()
        return response.status == 200 and json.loads(response.read()).get("health") == "true"
    finally:
        connection.close()


def get(member, key):
    kvs = post(member, "/v3/kv/range", {"key": b64(key)}).get("kvs", [])
    return unb64(kvs[0]["value"]) if kvs else None


def prefix(member, start):
    end = start[:-1] + chr(ord(start[-1]) + 1)
    return post(member, "/v3/kv/range", {"key": b64(start), "range_end": b64(end)})


def time_to_live(lease):
    reply = post("n1", "/v3/lease/timetolive", {"ID": str(lease), "keys": True})
    return int(reply.get("TTL", "-1")), sorted(unb64(k) for k in reply.get("keys", []))


def check(condition, message, evidence=None):
    if not condition:
        raise RuntimeError(f"{message}: {json.dumps(evidence, sort_keys=True, default=str)}")


def wait_for(predicate, what, timeout):
    deadline = time.monotonic() + timeout
    while True:
        try:
            if value := predicate():
                return value
        except (OSError, RuntimeError, ValueError, KeyError):
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out waiting for {what}")
        time.sleep(0.2)


class Watch(threading.Thread):
    """A live watch on lk/ through n2's gateway; records event arrival times."""

    def __init__(self, start_revision):
        super().__init__(daemon=True)
        self.start_revision = start_revision
        self.events = []
        self.created = threading.Event()
        self.deleted = threading.Event()
        self.error = None

    def run(self):
        try:
            connection = http.client.HTTPConnection("n2", 2379, timeout=600)
            request = {"create_request": {"key": b64("lk/"), "range_end": b64("lk0"),
                                          "start_revision": str(self.start_revision)}}
            connection.request("POST", "/v3/watch", json.dumps(request), {"Content-Type": "application/json"})
            response = connection.getresponse()
            while len([e for e in self.events if e["type"] == "DELETE"]) < 2:
                line = response.readline()
                if not line:
                    raise RuntimeError("watch stream ended")
                result = json.loads(line).get("result", {})
                if result.get("created"):
                    self.created.set()
                for event in result.get("events", []):
                    self.events.append({"type": event.get("type", "PUT"), "key": unb64(event["kv"]["key"]),
                                        "mod_revision": int(event["kv"]["mod_revision"]),
                                        "at": time.monotonic()})
            self.deleted.set()
        except Exception as error:  # Reported by the main thread.
            self.error = repr(error)
            self.deleted.set()


def main():
    from seedbox import World
    wait_for(lambda: all(post(m, "/v3/maintenance/status", {}).get("leader") for m in MEMBERS),
             "cluster leader", 300)
    lease_hex = wait_for(lambda: get("n1", "lease-id"), "client lease", 300)
    lease = int(lease_hex, 16)
    held = prefix("n1", "lk/")
    check(int(held.get("count", 0)) == 2, "client attached two keys", held)
    put_revisions = sorted(int(kv["mod_revision"]) for kv in held["kvs"])
    check(all(kv.get("lease") == str(lease) for kv in held["kvs"]), "keys carry the lease", held)
    watch = Watch(put_revisions[0])
    watch.start()
    check(watch.created.wait(60), "watch created", watch.error)

    # 1. Keep-alives hold the lease up for more than two TTLs.
    samples = []
    started = time.monotonic()
    while time.monotonic() - started < 2.5 * TTL:
        ttl, keys = time_to_live(lease)
        samples.append(ttl)
        check(ttl >= 2 and keys == ["lk/a", "lk/b"], "keep-alives renew the lease", [ttl, keys])
        time.sleep(0.5)

    with World.connect() as world:
        # 2. Cut the client off from every member; the quorum is untouched.
        cut = world.partition(["client"], list(MEMBERS))
        partitioned = time.monotonic()
        # 3. The leader expires the lease and revokes both keys at once.
        check(watch.deleted.wait(4 * TTL), "watch reports the deletions", watch.events)
        check(watch.error is None, "watch stream stayed up", watch.error)
        deletes = [e for e in watch.events if e["type"] == "DELETE"]
        check(sorted(e["key"] for e in deletes) == ["lk/a", "lk/b"], "both leased keys deleted", watch.events)
        check(len({e["mod_revision"] for e in deletes}) == 1, "one revoke deletes both keys", deletes)
        delay = max(e["at"] for e in deletes) - partitioned
        check(TTL * 2 / 3 - 1 <= delay <= TTL + 2, "expiry follows the last renewal by one TTL", delay)
        ttl, keys = wait_for(lambda: (r := time_to_live(lease))[0] == -1 and r, "lease gone", 10)
        check(keys == [], "expired lease has no keys", keys)
        world.sleep(2)
        world.heal(cut)
        healed = time.monotonic()

    # 4. The client reconnects and reports how its keep-alive ended.
    ended = wait_for(lambda: get("n1", "keepalive-ended"), "client report after heal", 120)
    report = post("n1", "/v3/kv/range", {"key": b64("keepalive-ended")})["kvs"][0]
    check(int(report["create_revision"]) > deletes[0]["mod_revision"], "client report follows the revoke",
          [report, deletes])
    statuses = {m: post(m, "/v3/maintenance/status", {}) for m in MEMBERS}
    check(all(healthy(m) for m in MEMBERS), "every member reports healthy")
    leaders = {s["leader"] for s in statuses.values()}
    check(len(leaders) == 1, "members agree on one leader", statuses)
    revisions = {m: int(post(m, "/v3/kv/range", {"key": b64("lease-id")})["header"]["revision"]) for m in MEMBERS}
    check(len(set(revisions.values())) == 1, "members agree on the revision", revisions)
    check(int(prefix("n2", "lk/").get("count", 0)) == 0, "leased keys stay deleted")
    evidence = {"lease": lease_hex, "puts": put_revisions, "min_ttl": min(samples),
                "deletes": [{k: e[k] for k in ("key", "mod_revision")} for e in deletes],
                "expiry_after_partition_ms": round(delay * 1000), "heal_after_partition_ms":
                round((healed - partitioned) * 1000), "client": ended,
                "report_revision": int(report["create_revision"]), "revision": revisions["n1"]}
    text = json.dumps(evidence, sort_keys=True)
    print("EVIDENCE " + text, flush=True)
    print("RESULT pass " + hashlib.sha256(text.encode()).hexdigest(), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
