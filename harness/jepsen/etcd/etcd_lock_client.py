"""One lock-set client for etcd_jepsen.py (Jepsen's etcd 3.4.3 lock-set workload).

Each client machine runs this program bound to one etcd member's JSON gateway.
For every operation it grants a fresh lease (TTL `--ttl`), keeps it alive from
a background thread every TTL/3, takes the lock through etcd's Lock RPC
(`/v3/lock/lock`, the server-side `concurrency.Mutex` that Jepsen's jetcd
client uses), then performs a read-modify-write of a set: read the set, sleep
`--hold` seconds, write the set plus one unique element, release the lock and
revoke the lease. Nothing in the client reacts to a lost lease, exactly like a
lock user that trusts the lock.

`--guard` selects where the set lives and how the write is protected:

* `none`: the set lives in the oracle's store; reads and writes are plain.
* `token`: the same store, but every read and write carries a fencing token,
  the create revision of this client's lock key; the store rejects any
  operation whose token is lower than one it has already seen.
* `txn`: the set lives in etcd key `set`; the write is a transaction that only
  succeeds while this client's lock key still exists (version > 0).

Every step is reported to the oracle's store (`/event`) so the oracle has one
totally ordered history. A pause of this process (SIGSTOP of its group) freezes
the keep-alive thread too, which is how a GC pause loses the lease.
"""
import argparse
import base64
import http.client
import json
import socket
import sys
import threading
import time


def b64(text):
    return base64.b64encode(text.encode()).decode()


def unb64(text):
    return base64.b64decode(text).decode()


class Client:
    def __init__(self, args):
        self.args = args
        self.name = args.name
        host, _, port = args.store.partition(":")
        self.store_host, self.store_port = host, int(port or 8000)

    # -------------------------------------------------- transport

    def etcd(self, path, body, timeout):
        connection = http.client.HTTPConnection(self.args.member, 2379, timeout=timeout)
        try:
            connection.request("POST", path, json.dumps(body), {"Content-Type": "application/json"})
            response = connection.getresponse()
            data = response.read()
            if response.status != 200:
                raise RuntimeError(f"{path}: HTTP {response.status} {data[:160]!r}")
            return json.loads(data)
        finally:
            connection.close()

    def store(self, path, body, timeout=10.0):
        """Report to the oracle; retried because the oracle's link is never faulted."""
        deadline = time.monotonic() + 60
        while True:
            connection = http.client.HTTPConnection(self.store_host, self.store_port, timeout=timeout)
            try:
                connection.request("POST", path, json.dumps(dict(body, client=self.name)),
                                   {"Content-Type": "application/json"})
                response = connection.getresponse()
                return json.loads(response.read())
            except (OSError, ValueError, http.client.HTTPException):
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.2)
            finally:
                connection.close()

    def event(self, **fields):
        return self.store("/event", fields)

    # -------------------------------------------------- lease

    def keepalive(self, lease, stop, stats):
        """Renew every TTL/3 like clientv3; a failed renewal is retried at once."""
        interval = self.args.ttl / 3
        wait = interval
        while not stop.wait(wait):
            sent = time.monotonic()
            try:
                reply = self.etcd("/v3/lease/keepalive", {"ID": str(lease)}, timeout=interval)
                ttl = int(reply.get("result", {}).get("TTL", "0"))
                stats["ok" if ttl > 0 else "expired"] += 1
                wait = max(0.0, interval - (time.monotonic() - sent))
            except (OSError, RuntimeError, ValueError, http.client.HTTPException):
                stats["error"] += 1
                wait = 0.05

    # -------------------------------------------------- set access

    def read_set(self, token, key):
        if self.args.guard == "txn":
            kvs = self.etcd("/v3/kv/range", {"key": b64("set")}, timeout=5).get("kvs", [])
            value = json.loads(unb64(kvs[0]["value"])) if kvs else []
            self.event(type="read", key=key, size=len(value))
            return value, True
        reply = self.event(type="read", key=key, token=token)
        return reply.get("set"), reply.get("ok", False)

    def write_set(self, value, token, key, element):
        if self.args.guard == "txn":
            txn = {"compare": [{"key": b64(key), "target": "VERSION", "result": "GREATER", "version": "0"}],
                   "success": [{"requestPut": {"key": b64("set"), "value": b64(json.dumps(value))}}]}
            reply = self.etcd("/v3/kv/txn", txn, timeout=5)
            ok = bool(reply.get("succeeded"))
            self.event(type="write", key=key, element=element, ok=ok,
                       revision=int(reply.get("header", {}).get("revision", 0)))
            return ok
        reply = self.event(type="write", key=key, token=token, element=element, set=value)
        return reply.get("ok", False)

    # -------------------------------------------------- one operation

    def operation(self, number):
        element = f"{self.name}-{number}"
        try:
            lease = int(self.etcd("/v3/lease/grant", {"TTL": self.args.ttl}, timeout=2)["ID"])
        except (OSError, RuntimeError, ValueError, KeyError, http.client.HTTPException) as error:
            self.event(type="fail", op=number, element=element, stage="grant", error=str(error)[:120])
            time.sleep(0.5)
            return
        stop, stats = threading.Event(), {"ok": 0, "expired": 0, "error": 0}
        keeper = threading.Thread(target=self.keepalive, args=(lease, stop, stats), daemon=True)
        keeper.start()
        key = None
        try:
            self.event(type="invoke", op=number, element=element, lease=lease)
            try:
                reply = self.etcd("/v3/lock/lock", {"name": b64("lock"), "lease": str(lease)},
                                  timeout=self.args.lock_timeout)
            except (OSError, RuntimeError, ValueError, http.client.HTTPException) as error:
                self.event(type="fail", op=number, element=element, stage="lock", error=str(error)[:120])
                return
            key = unb64(reply["key"])
            granted_at = int(reply.get("header", {}).get("revision", 0))
            # Evidence only: does the lock key still exist? Unguarded clients ignore the answer.
            try:
                kvs = self.etcd("/v3/kv/range", {"key": b64(key)}, timeout=2).get("kvs", [])
                present, create = bool(kvs), int(kvs[0]["create_revision"]) if kvs else None
            except (OSError, RuntimeError, ValueError, http.client.HTTPException):
                present, create = None, None
            token = create
            self.event(type="acquired", op=number, element=element, key=key, lease=lease,
                       granted_revision=granted_at, key_present=present, create_revision=create)
            if self.args.guard == "token" and token is None:
                self.event(type="fail", op=number, element=element, stage="token", key=key)
                return
            try:
                value, ok = self.read_set(token, key)
                if not ok:
                    self.event(type="fail", op=number, element=element, stage="read-fenced", key=key)
                    return
                time.sleep(self.args.hold)
                ok = self.write_set(sorted(set(value) | {element}), token, key, element)
            except (OSError, RuntimeError, ValueError, http.client.HTTPException) as error:
                self.event(type="info", op=number, element=element, stage="rmw", key=key, error=str(error)[:120])
                return
            if ok:
                self.event(type="ok", op=number, element=element, key=key)
            else:
                self.event(type="fail", op=number, element=element, stage="write-fenced", key=key)
        finally:
            if key is not None:
                self.event(type="releasing", op=number, key=key, keepalives=dict(stats))
                try:
                    self.etcd("/v3/lock/unlock", {"key": b64(key)}, timeout=2)
                except (OSError, RuntimeError, ValueError, http.client.HTTPException):
                    pass
            stop.set()
            try:
                self.etcd("/v3/lease/revoke", {"ID": str(lease)}, timeout=2)
            except (OSError, RuntimeError, ValueError, http.client.HTTPException):
                pass

    def run(self):
        while True:
            try:
                go = self.event(type="hello", member=self.args.member)
                if go.get("start"):
                    break
            except (OSError, ValueError, http.client.HTTPException):
                pass
            time.sleep(0.5)
        number = 0
        while not self.event(type="next").get("stop"):
            number += 1
            self.operation(number)
        self.event(type="done", ops=number)
        while True:
            time.sleep(3600)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--name", required=True)
    parser.add_argument("--member", required=True)
    parser.add_argument("--store", default="oracle:8000")
    parser.add_argument("--guard", choices=["none", "token", "txn"], default="none")
    parser.add_argument("--ttl", type=int, default=2)
    parser.add_argument("--hold", type=float, default=1.0, help="Seconds between the read and the write.")
    parser.add_argument("--lock-timeout", type=float, default=30.0)
    args = parser.parse_args()
    socket.setdefaulttimeout(30)
    Client(args).run()
    return 0


if __name__ == "__main__":
    sys.exit(main())
