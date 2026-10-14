"""Jepsen's PostgreSQL 12.3 list-append workload as a Seedbox oracle.

The pg machine runs the official postgres image. Each object is a row of one
of ``--tables`` tables, chosen by key: ``txnN (id int primary key, sk int,
val text)``, where ``val`` is a comma-separated list. As in Jepsen's
``jepsen.postgres`` append workload:

* an append is ``INSERT INTO txnN AS t (id, sk, val) VALUES (k, k, 'v') ON
  CONFLICT (id) DO UPDATE SET val = CONCAT(t.val, ',', 'v') WHERE t.id = k``,
  so the first append to a key inserts a fresh row;
* a read is ``SELECT val FROM txnN WHERE id = k`` (or ``sk = k`` for the
  ``--sk-reads`` fraction: an unindexed scan), and a missing row reads as [].

``--clients`` client threads each hold one connection and run transactions
back to back for ``--duration`` seconds: ``BEGIN ISOLATION LEVEL ...``, then
1..``--max-txn-length`` reads and appends, then ``COMMIT``. Keys follow Elle's
generator: ``--key-count`` active keys picked with exponentially decreasing
probability (base 2), and a key that has had ``--max-writes-per-key``
appends is retired for a fresh one, so new rows are inserted throughout the
run. Appended values are unique per key.

An error from the server before or at ``COMMIT`` (serialization failure,
deadlock, ...) is a definite abort (``fail``) after ``ROLLBACK``. A timeout or
a lost connection is indeterminate (``info``): the client's connection is
discarded and it continues as a new process, as Jepsen's clients do. Nothing
is retried. After the clients stop, one read-only transaction reads every key
the run touched.

The client speaks the frontend/backend protocol itself (simple query, trust
authentication), so there is no driver to pin. Every completed transaction is
printed as one ``H {json}`` line with its process, invocation and completion
times, its micro-operations and their results; the Elle-style checker in
``list_append.py`` then checks the history. Each evidence line is ``EVIDENCE
{json}``; the last line is ``VERDICT ...``. Exit status: 0 when the history is
valid for ``--model``, 1 when it is not, 3 when a valid run is inconclusive
(no transaction committed, or the final read failed). Run ``pg_append.py --check FILE`` to re-check a saved
log of ``H`` lines.
"""
import argparse
import json
import random
import socket
import struct
import sys
import threading
import time

import list_append

START = time.monotonic()
ISOLATION = {"serializable": "SERIALIZABLE", "repeatable-read": "REPEATABLE READ", "read-committed": "READ COMMITTED"}
MODEL = {"serializable": "serializable", "repeatable-read": "snapshot-isolation", "read-committed": "read-committed"}
PRINT = threading.Lock()


def now():
    return round(time.monotonic() - START, 6)


def emit(line):
    with PRINT:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def evidence(kind, **fields):
    emit("EVIDENCE " + json.dumps(dict(t=now(), kind=kind, **fields), sort_keys=True))


# ---------------------------------------------------------------- protocol

class PgError(RuntimeError):
    """An ErrorResponse: the server rejected the statement."""

    def __init__(self, fields):
        super().__init__(f"{fields.get('C')}: {fields.get('M')}")
        self.code = fields.get("C")


class Indeterminate(RuntimeError):
    """A timeout or a lost connection: the outcome is unknown."""


class Pg:
    """One connection: simple-query protocol only, trust authentication."""

    def __init__(self, host, port, timeout):
        self.host, self.timeout, self.buf = host, timeout, bytearray()
        try:
            self.sock = socket.create_connection((host, port), timeout=timeout)
            self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            body = struct.pack("!I", 196608) + b"user\0postgres\0database\0postgres\0\0"
            self.sock.sendall(struct.pack("!I", len(body) + 4) + body)
            deadline = time.monotonic() + timeout
            while True:
                kind, payload = self._message(deadline)
                if kind == b"R" and struct.unpack("!I", payload[:4])[0] != 0:
                    raise RuntimeError("server asked for a password; run postgres with trust authentication")
                if kind == b"E":
                    raise PgError(_fields(payload))
                if kind == b"Z":
                    return
        except BaseException:
            self.close()
            raise

    def close(self):
        try:
            self.sock.close()
        except (OSError, AttributeError):
            pass

    def _message(self, deadline):
        while True:
            if len(self.buf) >= 5:
                length = struct.unpack("!I", self.buf[1:5])[0]
                if len(self.buf) >= 1 + length:
                    kind, payload = bytes(self.buf[:1]), bytes(self.buf[5:1 + length])
                    del self.buf[:1 + length]
                    return kind, payload
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise Indeterminate(f"{self.host}: timeout")
            self.sock.settimeout(remaining)
            try:
                data = self.sock.recv(65536)
            except socket.timeout as error:
                raise Indeterminate(f"{self.host}: timeout") from error
            except OSError as error:
                raise Indeterminate(f"{self.host}: {error}") from error
            if not data:
                raise Indeterminate(f"{self.host}: server closed the connection")
            self.buf += data

    def query(self, sql):
        """Run one statement; returns (rows, command tag). Raises PgError or Indeterminate."""
        data = sql.encode() + b"\0"
        try:
            self.sock.sendall(b"Q" + struct.pack("!I", len(data) + 4) + data)
        except OSError as error:
            raise Indeterminate(f"{self.host}: send failed: {error}") from error
        deadline = time.monotonic() + self.timeout
        rows, tag, error = [], None, None
        while True:
            kind, payload = self._message(deadline)
            if kind == b"D":
                count, offset, row = struct.unpack("!H", payload[:2])[0], 2, []
                for _ in range(count):
                    size = struct.unpack("!i", payload[offset:offset + 4])[0]
                    offset += 4
                    if size < 0:
                        row.append(None)
                    else:
                        row.append(payload[offset:offset + size].decode())
                        offset += size
                rows.append(row)
            elif kind == b"C":
                tag = payload.rstrip(b"\0").decode()
            elif kind == b"E":
                error = PgError(_fields(payload))
            elif kind == b"Z":
                if error:
                    raise error
                return rows, tag


def _fields(payload):
    out = {}
    for item in payload.split(b"\0"):
        if item:
            out[chr(item[0])] = item[1:].decode(errors="replace")
    return out


# ---------------------------------------------------------------- workload

class Keys:
    """Elle's list-append key generator: a pool of active keys, exponential choice, retirement."""

    def __init__(self, rng, key_count, max_writes, max_length, read_fraction):
        self.rng, self.max_writes, self.max_length, self.read_fraction = rng, max_writes, max_length, read_fraction
        self.active = list(range(key_count))
        self.next_key = key_count
        self.writes = {}
        self.touched = set()
        self.weights = [2.0 ** -i for i in range(key_count)]
        self.lock = threading.Lock()

    def transaction(self):
        with self.lock:
            ops = []
            for _ in range(self.rng.randint(1, self.max_length)):
                slot = self.rng.choices(range(len(self.active)), self.weights)[0]
                k = self.active[slot]
                self.touched.add(k)
                if self.rng.random() < self.read_fraction:
                    ops.append(["r", k, None])
                    continue
                self.writes[k] = self.writes.get(k, 0) + 1
                ops.append(["append", k, self.writes[k]])
                if self.writes[k] >= self.max_writes:
                    self.active[slot] = self.next_key
                    self.next_key += 1
            return ops


def table(k, tables):
    return f"txn{k % tables}"


def parse(rows):
    if not rows or rows[0][0] is None or rows[0][0] == "":
        return []
    return [int(x) for x in rows[0][0].split(",")]


def execute(pg, ops, args, rng):
    """Run one transaction; returns (type, value, error, connection broken)."""
    value = []
    try:
        pg.query(f"BEGIN ISOLATION LEVEL {ISOLATION[args.isolation]}")
        for f, k, v in ops:
            t = table(k, args.tables)
            if f == "r":
                column = "sk" if rng.random() < args.sk_reads else "id"
                rows, _ = pg.query(f"SELECT val FROM {t} WHERE {column} = {k}")
                value.append(["r", k, parse(rows)])
            else:
                pg.query(f"INSERT INTO {t} AS t (id, sk, val) VALUES ({k}, {k}, '{v}') "
                         f"ON CONFLICT (id) DO UPDATE SET val = CONCAT(t.val, ',', '{v}') WHERE t.id = {k}")
                value.append(["append", k, v])
        _, tag = pg.query("COMMIT")
    except PgError as error:
        broken = False
        try:
            pg.query("ROLLBACK")
        except PgError:
            pass
        except Indeterminate:
            broken = True  # still a definite abort: the server rejected the transaction
        return "fail", None, error.code, broken
    if tag != "COMMIT":
        return "fail", None, f"commit tag {tag}", False
    return "ok", value, None, False


class Run:
    def __init__(self, args):
        self.args = args
        self.rng = random.Random(args.seed)
        self.keys = Keys(self.rng, args.key_count, args.max_writes_per_key, args.max_txn_length, args.read_fraction)
        self.txns, self.lock, self.counter = [], threading.Lock(), 0
        self.stop_at = None

    def record(self, process, kind, invoke_value, value, t0, t1, error):
        with self.lock:
            index = self.counter
            self.counter += 1
            txn = list_append.Txn(index, kind, value if kind == "ok" else
                                  [[f, k, None if f == "r" else v] for f, k, v in invoke_value],
                                  process, t0, t1, error)
            self.txns.append(txn)
        emit("H " + json.dumps({"i": index, "p": process, "type": kind, "t0": t0, "t1": t1,
                                "value": txn.value, "error": error}, separators=(",", ":")))

    def client(self, slot):
        process, rng = slot, random.Random(self.args.seed * 1000 + slot)
        pg = None
        while time.monotonic() < self.stop_at:
            if pg is None:
                try:
                    pg = Pg(self.args.host, self.args.port, self.args.timeout)
                except (OSError, PgError, Indeterminate) as error:
                    evidence("connect-error", process=process, error=str(error))
                    time.sleep(1)
                    continue
            ops = self.keys.transaction()
            t0 = now()
            try:
                kind, value, error, broken = execute(pg, ops, self.args, rng)
            except Indeterminate as error_:
                kind, value, error, broken = "info", None, str(error_), True
            self.record(process, kind, ops, value, t0, now(), error)
            if broken:
                pg.close()
                pg, process = None, process + self.args.clients

    def final_read(self):
        keys = sorted(self.keys.touched)
        for attempt in range(10):
            pg = None
            try:
                pg = Pg(self.args.host, self.args.port, 30)
                pg.query("BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY")
                value = []
                for k in keys:
                    rows, _ = pg.query(f"SELECT val FROM {table(k, self.args.tables)} WHERE id = {k}")
                    value.append(["r", k, parse(rows)])
                pg.query("COMMIT")
                return value
            except (PgError, Indeterminate, OSError) as error:
                evidence("final-read-error", attempt=attempt, error=str(error))
                time.sleep(1)
            finally:
                if pg:
                    pg.close()
        return None


def wait_ready(args):
    deadline = time.monotonic() + args.ready_timeout
    while True:
        try:
            pg = Pg(args.host, args.port, 10)
            pg.query("SELECT 1")
            return pg
        except (OSError, PgError, Indeterminate):
            if time.monotonic() > deadline:
                raise
            time.sleep(0.5)


def report(txns, model, examples):
    result = list_append.check(txns, model=model)
    evidence("check", valid=result["valid"], model=model, counts=result["counts"], illegal=result["illegal"],
             txns=result["txns"], edges=result["edges"])
    for kind, items in sorted(result["anomalies"].items()):
        for item in items[:examples]:
            if "explanation" in item:
                evidence("anomaly", type=kind, cycle=item["cycle"], steps=item["steps"],
                         component_size=item["component-size"],
                         txns=[next(t for t in txns if t.id == i).describe() for i in item["cycle"]])
                emit(f"ANOMALY {kind}\n{item['explanation']}")
            else:
                evidence("anomaly", type=kind, **item)
    return result


def load(path):
    txns = []
    with open(path, errors="replace") as handle:
        for line in handle:
            if line.startswith("H "):
                op = json.loads(line[2:])
                txns.append(list_append.Txn(op["i"], op["type"], op["value"], op["p"], op["t0"], op["t1"], op["error"]))
    return txns


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--host", default="pg")
    parser.add_argument("--port", type=int, default=5432)
    parser.add_argument("--isolation", choices=sorted(ISOLATION), default="serializable")
    parser.add_argument("--model", choices=sorted(list_append.ALLOWED), help="default: from --isolation")
    parser.add_argument("--clients", type=int, default=20)
    parser.add_argument("--duration", type=float, default=60)
    parser.add_argument("--tables", type=int, default=3)
    parser.add_argument("--key-count", type=int, default=10)
    parser.add_argument("--max-writes-per-key", type=int, default=32)
    parser.add_argument("--max-txn-length", type=int, default=4)
    parser.add_argument("--read-fraction", type=float, default=0.5)
    parser.add_argument("--sk-reads", type=float, default=0.0)
    parser.add_argument("--timeout", type=float, default=10)
    parser.add_argument("--ready-timeout", type=float, default=300)
    parser.add_argument("--seed", type=int, default=1, help="generator seed (Seedbox varies the schedule)")
    parser.add_argument("--examples", type=int, default=5, help="anomaly examples printed per type")
    parser.add_argument("--label", default="")
    parser.add_argument("--check", metavar="FILE", help="re-check a saved log of H lines and exit")
    args = parser.parse_args()
    model = args.model or MODEL[args.isolation]
    if args.check:
        txns = load(args.check)
        result = report(txns, model, args.examples)
        print(f"VERDICT {'valid' if result['valid'] else 'INVALID'} {json.dumps(result['counts'])}")
        return 0 if result["valid"] else 1

    evidence("config", **{k: v for k, v in vars(args).items() if k != "check"}, model_checked=model)
    pg = wait_ready(args)
    version, _ = pg.query("SHOW server_version")
    for i in range(args.tables):
        pg.query(f"DROP TABLE IF EXISTS txn{i}")
        pg.query(f"CREATE TABLE txn{i} (id int NOT NULL PRIMARY KEY, sk int NOT NULL, val text)")
    pg.close()
    evidence("ready", server_version=version[0][0])

    run = Run(args)
    run.stop_at = time.monotonic() + args.duration
    threads = [threading.Thread(target=run.client, args=(slot,), daemon=True) for slot in range(args.clients)]
    for thread in threads:
        thread.start()
    while any(thread.is_alive() for thread in threads):
        for thread in threads:
            thread.join(timeout=10)
        with run.lock:
            counts = {kind: sum(1 for t in run.txns if t.type == kind) for kind in ("ok", "fail", "info")}
        evidence("progress", **counts)
    final = run.final_read()
    if final is not None:
        run.record("final", "ok", final, final, now(), now(), None)
    with run.lock:
        txns = list(run.txns)
    committed = sum(1 for t in txns if t.type == "ok")
    result = report(txns, model, args.examples)
    if result["valid"] and (committed <= 1 or final is None):
        reason = "no transaction committed" if committed <= 1 else "the final read failed"
        print(f"VERDICT inconclusive: {reason}: {json.dumps(result['txns'])}", flush=True)
        return 3
    print(f"VERDICT {'valid' if result['valid'] else 'INVALID'} {args.isolation} checked as {model}: "
          f"{json.dumps(result['counts'])} txns {json.dumps(result['txns'])}", flush=True)
    return 0 if result["valid"] else 1


if __name__ == "__main__":
    sys.exit(main())
