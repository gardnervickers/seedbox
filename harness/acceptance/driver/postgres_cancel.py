"""Postgres query cancellation and backend replacement (NEXT-100 case 95).

The pg machine runs the official postgres image (initdb, then the
postmaster). The oracle speaks the frontend/backend protocol itself. Every
cancellation goes the way drivers send it: a CancelRequest carrying the
target backend's PID and secret key on a fresh connection; the postmaster
forks a child that signals (SIGINT) the target backend, which must stop at its
next interrupt check and report SQLSTATE 57014. The oracle cancels:

* a sleeping query (pg_sleep, a latch wait with a timeout);
* a CPU-bound query (counting a streamed generate_series; in FROM it would
  materialize and spill temp files into the guest's memory);
* a sleeping query inside a transaction, which then stays aborted (25P02)
  until ROLLBACK.

Each canceled connection must then be reusable. A CancelRequest with a wrong
secret key must have no effect. Finally the oracle terminates an idle
connection's backend with pg_terminate_backend (SQLSTATE 57P01) and replaces
it with a new connection and backend. Throughout, a writer thread on its own
connection inserts numbered rows; every acknowledged row must be present at
the end, exactly once and in order.
"""
import hashlib
import json
import socket
import struct
import threading
import time

HOST, PORT = "pg", 5432
CANCEL_CODE = 80877102


class PgError(RuntimeError):
    def __init__(self, fields):
        super().__init__(f"{fields.get('S')} {fields.get('C')}: {fields.get('M')}")
        self.fields = fields
        self.code = fields.get("C")


class Pg:
    """One connection, simple-query protocol only, trust authentication."""

    def __init__(self, timeout=300.0):
        deadline = time.monotonic() + timeout
        while True:
            try:
                self._open()
                return
            except (OSError, PgError):
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.5)

    def _open(self):
        self.channel = socket.create_connection((HOST, PORT), timeout=120)
        self.stream = self.channel.makefile("rb")
        body = struct.pack("!I", 196608) + b"user\0postgres\0database\0postgres\0\0"
        self.channel.sendall(struct.pack("!I", len(body) + 4) + body)
        self.pid = self.secret = None
        while True:
            kind, payload = self.message()
            if kind == b"R" and struct.unpack("!I", payload[:4])[0] != 0:
                raise RuntimeError("unexpected authentication request; use trust")
            if kind == b"K":
                self.pid, self.secret = struct.unpack("!II", payload)
            if kind == b"E":
                self.channel.close()
                raise PgError(fields(payload))
            if kind == b"Z":
                return

    def message(self):
        header = self.stream.read(5)
        if len(header) < 5:
            raise ConnectionResetError("server closed the connection")
        kind, length = header[:1], struct.unpack("!I", header[1:])[0]
        return kind, self.stream.read(length - 4)

    def send_query(self, sql):
        data = sql.encode() + b"\0"
        self.channel.sendall(b"Q" + struct.pack("!I", len(data) + 4) + data)

    def result(self):
        """Rows of the last statement, or the PgError of the query; then ReadyForQuery."""
        rows, error = [], None
        while True:
            kind, payload = self.message()
            if kind == b"D":
                count = struct.unpack("!H", payload[:2])[0]
                row, offset = [], 2
                for _ in range(count):
                    size = struct.unpack("!i", payload[offset:offset + 4])[0]
                    offset += 4
                    row.append(None if size < 0 else payload[offset:offset + size].decode())
                    offset += max(size, 0)
                rows.append(row)
            elif kind == b"E":
                error = PgError(fields(payload))
            elif kind == b"Z":
                self.status = payload.decode()
                if error:
                    raise error
                return rows

    def query(self, sql):
        self.send_query(sql)
        return self.result()

    def close(self):
        try:
            self.channel.sendall(b"X" + struct.pack("!I", 4))
        except OSError:
            pass
        self.channel.close()


def fields(payload):
    found = {}
    for part in payload.split(b"\0"):
        if part:
            found[chr(part[0])] = part[1:].decode()
    return found


def cancel(pid, secret):
    with socket.create_connection((HOST, PORT), timeout=30) as s:
        s.sendall(struct.pack("!IIII", 16, CANCEL_CODE, pid, secret))
        # The server closes the connection without replying.
        s.settimeout(30)
        rest = s.recv(1)
    if rest:
        raise RuntimeError(f"unexpected reply to a CancelRequest: {rest!r}")


def check(condition, message, evidence=None):
    if not condition:
        raise RuntimeError(f"{message}: {json.dumps(evidence, sort_keys=True, default=str)}")


def canceled(connection, sql, secret=None, delay=0.5):
    """Run `sql`, cancel it after `delay`, and return (SQLSTATE, seconds)."""
    started = time.monotonic()
    connection.send_query(sql)
    time.sleep(delay)
    cancel(connection.pid, connection.secret if secret is None else secret)
    try:
        connection.result()
        code = None
    except PgError as error:
        code = error.code
    return code, time.monotonic() - started


class Writer(threading.Thread):
    """Insert numbered rows one statement at a time until stopped."""

    def __init__(self):
        super().__init__(daemon=True)
        self.connection = Pg()
        self.acknowledged = []
        self.stopping = threading.Event()
        self.error = None

    def run(self):
        try:
            n = 0
            while not self.stopping.is_set():
                self.connection.query(f"INSERT INTO ledger (n) VALUES ({n})")
                self.acknowledged.append(n)
                n += 1
                time.sleep(0.01)
        except Exception as error:  # Reported by the main thread.
            self.error = repr(error)


def main():
    a = Pg()
    version = a.query("SHOW server_version")[0][0]
    a.query("CREATE TABLE ledger (n integer PRIMARY KEY)")
    writer = Writer()
    writer.start()
    evidence = {"server_version": version}

    code, seconds = canceled(a, "SELECT pg_sleep(60)")
    check(code == "57014" and seconds < 10, "sleeping query canceled", [code, seconds])
    check(a.status == "I" and a.query("SELECT 6 * 7") == [["42"]], "connection reusable after cancel")
    evidence["sleep"] = code

    code, seconds = canceled(a, "SELECT count(*) FROM (SELECT generate_series(1, 4000000000)) AS s")
    check(code == "57014" and seconds < 30, "CPU-bound query canceled", [code, seconds])
    check(a.query("SELECT 1") == [["1"]], "connection reusable after a CPU-bound cancel")
    evidence["cpu"] = code

    a.query("BEGIN")
    a.query("INSERT INTO ledger (n) VALUES (-1)")
    code, seconds = canceled(a, "SELECT pg_sleep(60)")
    check(code == "57014" and a.status == "E", "cancel inside a transaction aborts it", [code, a.status])
    try:
        a.query("SELECT 1")
        aborted = None
    except PgError as error:
        aborted = error.code
    check(aborted == "25P02", "aborted transaction refuses statements", aborted)
    a.query("ROLLBACK")
    check(a.status == "I" and a.query("SELECT count(*) FROM ledger WHERE n = -1") == [["0"]],
          "rollback discards the canceled transaction's insert")
    evidence["transaction"] = [code, aborted]

    code, seconds = canceled(a, "SELECT pg_sleep(2)", secret=(a.secret ^ 0x5A5A5A5A) or 1)
    check(code is None and seconds >= 2, "a wrong secret key cancels nothing", [code, seconds])

    c = Pg()
    old_pid = c.pid
    check(a.query(f"SELECT pg_terminate_backend({old_pid})") == [["t"]], "terminate requested")
    # The backend reports FATAL to its idle client unprompted, then closes.
    kind, payload = c.message()
    terminated = fields(payload).get("C") if kind == b"E" else kind.decode()
    check(terminated == "57P01", "terminated backend reports admin_shutdown", terminated)
    check(c.stream.read(1) == b"", "terminated backend closes its connection")
    c.channel.close()
    replacement = Pg()
    check(replacement.pid != old_pid and replacement.query("SELECT 1") == [["1"]],
          "a replacement backend serves", [old_pid, replacement.pid])
    alive = a.query(f"SELECT count(*) FROM pg_stat_activity WHERE pid = {old_pid}")
    check(alive == [["0"]], "the terminated backend is gone", alive)
    evidence["terminate"] = terminated

    time.sleep(1)
    writer.stopping.set()
    writer.join(60)
    check(not writer.is_alive() and writer.error is None, "writer kept running throughout", writer.error)
    stored = [int(n) for (n,) in replacement.query("SELECT n FROM ledger ORDER BY n")]
    check(stored == writer.acknowledged, "every acknowledged insert is present once",
          {"stored": len(stored), "acknowledged": len(writer.acknowledged)})
    check(len(stored) > 100, "the writer overlapped the cancellations", len(stored))
    evidence["writer_rows"] = len(stored)
    for connection in (a, replacement, writer.connection):
        connection.close()
    text = json.dumps(evidence, sort_keys=True)
    print("EVIDENCE " + text, flush=True)
    print("RESULT pass " + hashlib.sha256(text.encode()).hexdigest(), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
