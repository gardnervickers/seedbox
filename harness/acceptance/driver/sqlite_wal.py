"""SQLite WAL with a pinned reader, checkpoints, and a crash (NEXT-100 case 93).

The db machine keeps a SQLite database on a retained volume in WAL mode with
synchronous=FULL, memory-mapped reads (mmap_size) and automatic checkpoints
off. A forked reader process holds a read transaction while the writer commits
100 single-row updates, each acknowledged to the oracle only after its commit
returned. With that snapshot pinned:

* every checkpoint mode copies no frame back (the reader still reads the
  database file), PASSIVE reports no contention and FULL/RESTART/TRUNCATE
  report busy;
* the reader still sees its snapshot, and the writer's connection the updates.

The reader then ends its transaction; a TRUNCATE checkpoint copies every frame
and empties the WAL. The writer commits 50 more acknowledged updates, which
live only in the WAL, and the oracle crashes the machine straight after the
last acknowledgement. After the reboot SQLite must recover from the WAL:
integrity_check is ok and every acknowledged update is present with the
expected value. A further update and checkpoint must then succeed.

These are SQLite's documented semantics; the checkpoint results, file sizes and
recovered rows are printed as evidence, which a replay must reproduce.
"""
import hashlib
import json
import os
import socket
import sqlite3
import sys
import time

PATH = "/data/ledger.db"
PORT = 7300
ROWS = 100
PINNED_UPDATES = 100
WAL_UPDATES = 50


def connect():
    db = sqlite3.connect(PATH, isolation_level=None, timeout=0)
    db.execute("PRAGMA busy_timeout=0")
    db.execute("PRAGMA mmap_size=67108864")
    db.execute("PRAGMA synchronous=FULL")
    db.execute("PRAGMA wal_autocheckpoint=0")
    return db


def wal_size():
    try:
        return os.stat(PATH + "-wal").st_size
    except FileNotFoundError:
        return None


def reader(channel):
    """The pinned reader, a separate process with its own connection."""
    db = None
    stream = channel.makefile("rwb")
    for line in stream:
        command = line.decode().strip()
        if command == "pin":
            db = connect()
            db.execute("BEGIN")
        if command in ("pin", "sum"):
            count, total = db.execute("SELECT count(*), sum(v) FROM ledger").fetchone()
            reply = {"count": count, "sum": total}
        elif command == "release":
            db.execute("COMMIT")
            db.close()
            reply = {"released": True}
        stream.write(json.dumps(reply).encode() + b"\n")
        stream.flush()
        if command == "release":
            os._exit(0)
    os._exit(1)


class Db:
    def __init__(self):
        self.writer = None
        self.reader = None

    def handle(self, request):
        op = request["op"]
        if op == "setup":
            if os.path.exists(PATH):
                raise RuntimeError("setup on an existing database")
            self.writer = connect()
            mode = self.writer.execute("PRAGMA journal_mode=WAL").fetchone()[0]
            self.writer.execute("CREATE TABLE ledger (k INTEGER PRIMARY KEY, v INTEGER NOT NULL, pad TEXT)")
            self.writer.execute("BEGIN")
            self.writer.executemany("INSERT INTO ledger VALUES (?, 0, ?)",
                                    [(k, "x" * 200) for k in range(ROWS)])
            self.writer.execute("COMMIT")
            result = self.writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
            return {"journal_mode": mode, "checkpoint": list(result), "wal_size": wal_size()}
        if op == "open":
            self.writer = connect()
            return {"journal_mode": self.writer.execute("PRAGMA journal_mode").fetchone()[0]}
        if op in ("pin", "sum", "release"):
            if op == "pin":
                ours, theirs = socket.socketpair()
                pid = os.fork()
                if pid == 0:
                    ours.close()
                    reader(theirs)
                theirs.close()
                self.reader = (pid, ours.makefile("rwb"))
            pid, stream = self.reader
            stream.write(op.encode() + b"\n")
            stream.flush()
            reply = json.loads(stream.readline())
            if op == "release":
                _, status = os.waitpid(pid, 0)
                reply["exit"] = os.waitstatus_to_exitcode(status)
                self.reader = None
            return reply
        if op == "update":
            k, v = request["k"], request["v"]
            changed = self.writer.execute("UPDATE ledger SET v = ?, pad = ? WHERE k = ?",
                                          (v, f"{v:08d}" * 25, k)).rowcount
            return {"changed": changed}
        if op == "checkpoint":
            busy, log, done = self.writer.execute(f"PRAGMA wal_checkpoint({request['mode']})").fetchone()
            return {"busy": busy, "log": log, "checkpointed": done, "wal_size": wal_size()}
        if op == "state":
            db = self.writer or connect()
            count, total = db.execute("SELECT count(*), sum(v) FROM ledger").fetchone()
            return {"integrity": db.execute("PRAGMA integrity_check").fetchall(),
                    "journal_mode": db.execute("PRAGMA journal_mode").fetchone()[0],
                    "count": count, "sum": total,
                    "values": [v for (v,) in db.execute("SELECT v FROM ledger ORDER BY k")],
                    "wal_size": wal_size(), "db_size": os.stat(PATH).st_size}
        raise RuntimeError(f"unknown op {op}")


def db_main():
    db = Db()
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", PORT))
    acceptor.listen(4)
    print(f"DB_READY existing={os.path.exists(PATH)} wal={wal_size()}", flush=True)
    while True:
        channel, _ = acceptor.accept()
        with channel:
            stream = channel.makefile("rwb")
            for line in stream:
                try:
                    reply = {"ok": True, **db.handle(json.loads(line))}
                except Exception as error:  # Reported to the oracle, which fails.
                    reply = {"ok": False, "error": f"{type(error).__name__}: {error}"}
                stream.write(json.dumps(reply).encode() + b"\n")
                stream.flush()


# -------------------------------------------------------------------- oracle

class Client:
    def __init__(self, timeout=60.0):
        deadline = time.monotonic() + timeout
        while True:
            try:
                self.channel = socket.create_connection(("db", PORT), timeout=30)
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise RuntimeError("db readiness timed out")
                time.sleep(0.1)
        self.stream = self.channel.makefile("rwb")

    def __call__(self, op, **arguments):
        self.stream.write(json.dumps({"op": op, **arguments}).encode() + b"\n")
        self.stream.flush()
        line = self.stream.readline()
        if not line:
            raise RuntimeError(f"db closed during {op}")
        reply = json.loads(line)
        if not reply.pop("ok"):
            raise RuntimeError(f"{op} failed: {reply['error']}")
        return reply

    def close(self):
        self.channel.close()


def check(condition, message, evidence=None):
    if not condition:
        raise RuntimeError(f"{message}: {json.dumps(evidence, sort_keys=True)}")


def oracle():
    from seedbox import World
    expected = [0] * ROWS
    evidence = {}
    call = Client()
    setup = call("setup")
    check(setup["journal_mode"] == "wal" and setup["wal_size"] == 0, "WAL mode, empty WAL", setup)
    evidence["setup"] = setup
    pinned = call("pin")
    check(pinned == {"count": ROWS, "sum": 0}, "reader pins the initial snapshot", pinned)
    for i in range(PINNED_UPDATES):
        k = (i * 37) % ROWS
        expected[k] = i + 1
        check(call("update", k=k, v=i + 1)["changed"] == 1, "update acknowledged", i)
    evidence["pinned_checkpoints"] = {}
    for mode in ("PASSIVE", "FULL", "RESTART", "TRUNCATE"):
        result = call("checkpoint", mode=mode)
        evidence["pinned_checkpoints"][mode] = result
        check(result["log"] >= PINNED_UPDATES and result["checkpointed"] == 0,
              f"{mode}: a pinned reader keeps every frame in the WAL", result)
        check(result["busy"] == (0 if mode == "PASSIVE" else 1), f"{mode}: busy only for blocking modes", result)
        check(result["wal_size"] > 0, f"{mode}: WAL retained", result)
    snapshot = call("sum")
    check(snapshot == {"count": ROWS, "sum": 0}, "reader still sees its snapshot", snapshot)
    current = call("state")
    check(current["values"] == expected, "writer sees every acknowledged update", current)
    released = call("release")
    check(released["released"] and released["exit"] == 0, "reader released", released)
    final = call("checkpoint", mode="TRUNCATE")
    check(final["busy"] == 0 and final["log"] == final["checkpointed"] and final["wal_size"] == 0,
          "TRUNCATE copies every frame and empties the WAL", final)
    evidence["released_checkpoint"] = final
    for i in range(PINNED_UPDATES, PINNED_UPDATES + WAL_UPDATES):
        k = (i * 11) % ROWS
        expected[k] = i + 1
        check(call("update", k=k, v=i + 1)["changed"] == 1, "WAL update acknowledged", i)
    with World.connect() as world:
        # Straight after the last acknowledgement: those commits live only in the WAL.
        world.crash("db")
        world.reboot("db")
    call.close()
    call = Client()
    recovered = call("state")
    evidence["recovered"] = {k: v for k, v in recovered.items() if k != "values"}
    check(recovered["integrity"] == [["ok"]], "integrity_check after recovery", recovered)
    check(recovered["journal_mode"] == "wal", "still in WAL mode", recovered)
    check(recovered["count"] == ROWS and recovered["values"] == expected,
          "every acknowledged update survives the crash", {"recovered": recovered["values"], "expected": expected})
    call("open")
    check(call("update", k=0, v=10_000)["changed"] == 1, "update after recovery")
    after = call("checkpoint", mode="TRUNCATE")
    check(after["busy"] == 0 and after["wal_size"] == 0, "checkpoint after recovery", after)
    evidence["after_recovery"] = after
    text = json.dumps(evidence, sort_keys=True)
    print("EVIDENCE " + text, flush=True)
    print("RESULT pass " + hashlib.sha256(text.encode()).hexdigest(), flush=True)
    return 0


def main():
    role = sys.argv[1]
    if role == "db":
        db_main()
    elif role == "oracle":
        return oracle()
    else:
        raise RuntimeError(f"unknown role {role}")


if __name__ == "__main__":
    raise SystemExit(main())
