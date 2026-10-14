"""One lock-set client for etcd_jepsen.py (Jepsen's etcd 3.4.3 lock-set workload).

Each client machine runs this program bound to one etcd member's JSON gateway.
For every operation it grants a fresh lease (TTL `--ttl`) and renews it every
TTL/3, takes the lock through etcd's Lock RPC (`/v3/lock/lock`, the
server-side `concurrency.Mutex` that Jepsen's jetcd client uses), then performs
a read-modify-write of a set: read the set, wait `--hold` seconds, write the
set plus one unique element, release the lock and revoke the lease. Nothing in
the client reacts to a lost lease, exactly like a lock user that trusts the
lock.

`--guard` selects where the set lives and how the write is protected:

* `none`: the set lives in the oracle's store; reads and writes are plain.
* `token`: the same store, but every read and write carries a fencing token,
  the create revision of this client's lock key; the store rejects any
  operation whose token is lower than one it has already seen.
* `txn`: the set lives in etcd key `set`; the write is a transaction that only
  succeeds while this client's lock key still exists (version > 0).

Every step is reported to the oracle's store (`/event`) so the oracle has one
totally ordered history. The client is one thread: it renews the lease while
it waits for the Lock reply and while it holds the lock, so a pause of this
process (SIGSTOP of its group) stops renewals too, which is how a GC pause
loses the lease.
"""
import argparse
import base64
import http.client
import json
import select
import socket
import sys
import time

ERRORS = (OSError, RuntimeError, ValueError, KeyError, http.client.HTTPException)


def b64(text):
    return base64.b64encode(text.encode()).decode()


def unb64(text):
    return base64.b64decode(text).decode()


class Lease:
    """A lease renewed every TTL/3 from the client's only thread; a failed renewal is retried at once."""

    def __init__(self, client, lease):
        self.client, self.id = client, lease
        self.interval = client.args.ttl / 3
        self.due = time.monotonic() + self.interval
        self.stats = {"ok": 0, "expired": 0, "error": 0}

    def renew_if_due(self):
        if time.monotonic() < self.due:
            return
        sent = time.monotonic()
        try:
            reply = self.client.etcd("/v3/lease/keepalive", {"ID": str(self.id)}, timeout=self.interval)
            ttl = int(reply.get("result", {}).get("TTL", "0"))
            self.stats["ok" if ttl > 0 else "expired"] += 1
            self.due = sent + self.interval
        except ERRORS:
            self.stats["error"] += 1
            self.due = time.monotonic() + 0.05

    def wait(self, seconds, sock=None):
        """Renew while waiting `seconds`; with `sock`, return True as soon as it is readable."""
        deadline = time.monotonic() + seconds
        while True:
            self.renew_if_due()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            pause = max(0.0, min(remaining, self.due - time.monotonic()))
            if sock is not None:
                if select.select([sock], [], [], pause)[0]:
                    return True
            else:
                time.sleep(pause)


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

    def lock(self, lease):
        """The Lock RPC, renewing the lease while it blocks; returns the reply or raises."""
        body = json.dumps({"name": b64("lock"), "lease": str(lease.id)}).encode()
        sock = socket.create_connection((self.args.member, 2379), timeout=2)
        try:
            sock.sendall(b"POST /v3/lock/lock HTTP/1.1\r\nHost: " + self.args.member.encode()
                         + b"\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: "
                         + str(len(body)).encode() + b"\r\n\r\n" + body)
            if not lease.wait(self.args.lock_timeout, sock):
                raise TimeoutError("lock timed out")
            sock.settimeout(5)
            response = http.client.HTTPResponse(sock)
            response.begin()
            data = response.read()
            if response.status != 200:
                raise RuntimeError(f"lock: HTTP {response.status} {data[:160]!r}")
            return json.loads(data)
        finally:
            sock.close()

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
            lease = Lease(self, int(self.etcd("/v3/lease/grant", {"TTL": self.args.ttl}, timeout=2)["ID"]))
        except ERRORS as error:
            self.event(type="fail", op=number, element=element, stage="grant", error=str(error)[:120])
            time.sleep(0.5)
            return
        key = None
        try:
            self.event(type="invoke", op=number, element=element, lease=lease.id)
            try:
                reply = self.lock(lease)
            except ERRORS as error:
                self.event(type="fail", op=number, element=element, stage="lock", error=str(error)[:120])
                return
            key = unb64(reply["key"])
            granted_at = int(reply.get("header", {}).get("revision", 0))
            # Evidence only: does the lock key still exist? Unguarded clients ignore the answer.
            try:
                kvs = self.etcd("/v3/kv/range", {"key": b64(key)}, timeout=2).get("kvs", [])
                present, create = bool(kvs), int(kvs[0]["create_revision"]) if kvs else None
            except ERRORS:
                present, create = None, None
            token = create
            self.event(type="acquired", op=number, element=element, key=key, lease=lease.id,
                       granted_revision=granted_at, key_present=present, create_revision=create)
            if self.args.guard == "token" and token is None:
                self.event(type="fail", op=number, element=element, stage="token", key=key)
                return
            try:
                lease.renew_if_due()
                value, ok = self.read_set(token, key)
                if not ok:
                    self.event(type="fail", op=number, element=element, stage="read-fenced", key=key)
                    return
                lease.wait(self.args.hold)
                ok = self.write_set(sorted(set(value) | {element}), token, key, element)
            except ERRORS as error:
                self.event(type="info", op=number, element=element, stage="rmw", key=key, error=str(error)[:120])
                return
            if ok:
                self.event(type="ok", op=number, element=element, key=key)
            else:
                self.event(type="fail", op=number, element=element, stage="write-fenced", key=key)
        finally:
            if key is not None:
                self.event(type="releasing", op=number, key=key, keepalives=dict(lease.stats))
                try:
                    self.etcd("/v3/lock/unlock", {"key": b64(key)}, timeout=2)
                except ERRORS:
                    pass
            try:
                self.etcd("/v3/lease/revoke", {"ID": str(lease.id)}, timeout=2)
            except ERRORS:
                pass

    def run(self):
        while True:
            try:
                if self.event(type="hello", member=self.args.member).get("start"):
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
    Client(args).run()
    return 0


if __name__ == "__main__":
    sys.exit(main())
