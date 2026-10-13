"""Redis AOF rewrite under appendfsync always, then a crash (NEXT-100 case 94).

The redis machine runs the official image's redis-server with appendonly yes,
appendfsync always, no RDB snapshots, and its data directory on a retained
volume. With appendfsync always, Redis writes and fsyncs the AOF before it
replies, so every acknowledged write must survive a machine crash.

The oracle prefills about 10 MB so the rewrite takes a while, writes a ledger
of keys and a counter, starts BGREWRITEAOF (a fork:
the child writes a new base file from a copy-on-write snapshot while the
parent keeps serving), keeps writing during the rewrite and requires each
write's acknowledgement, waits until INFO reports the rewrite finished
successfully (the parent reaped the child and installed the new manifest),
writes a final batch and crashes the machine straight after the last
acknowledgement. After the reboot Redis must load the rewritten base plus the
incremental AOF: every acknowledged key has its acknowledged value, the
counter equals the acknowledged increments, and nothing unacknowledged appears.
Rewrite statistics and the recovered key count are printed as evidence.
"""
import hashlib
import json
import socket
import time

HOST, PORT = "redis", 6379
PREFILL, BATCH = 20_000, 100
BEFORE, AFTER = 2000, 500


class Redis:
    """A minimal RESP2 client: one connection, one request at a time."""

    def __init__(self, timeout=120.0):
        deadline = time.monotonic() + timeout
        while True:
            try:
                self.channel = socket.create_connection((HOST, PORT), timeout=30)
                self.stream = self.channel.makefile("rb")
                if self.call("PING") == "PONG":
                    return
            except (OSError, RedisError):
                pass
            if time.monotonic() >= deadline:
                raise RuntimeError("redis readiness timed out")
            time.sleep(0.2)

    def send(self, *args):
        frame = [b"*%d\r\n" % len(args)]
        for arg in args:
            data = arg if isinstance(arg, bytes) else str(arg).encode()
            frame.append(b"$%d\r\n%s\r\n" % (len(data), data))
        self.channel.sendall(b"".join(frame))

    def reply(self):
        line = self.stream.readline()
        if not line.endswith(b"\r\n"):
            raise OSError("redis connection closed")
        kind, rest = line[:1], line[1:-2]
        if kind == b"+":
            return rest.decode()
        if kind == b"-":
            raise RedisError(rest.decode())
        if kind == b":":
            return int(rest)
        if kind == b"$":
            n = int(rest)
            if n < 0:
                return None
            data = self.stream.read(n + 2)
            return data[:-2].decode()
        if kind == b"*":
            return [self.reply() for _ in range(int(rest))]
        raise OSError(f"bad RESP line {line!r}")

    def call(self, *args):
        self.send(*args)
        return self.reply()

    def close(self):
        self.channel.close()


class RedisError(RuntimeError):
    pass


def info(redis, section):
    fields = {}
    for line in redis.call("INFO", section).splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            fields[key] = value
    return fields


def check(condition, message, evidence=None):
    if not condition:
        raise RuntimeError(f"{message}: {json.dumps(evidence, sort_keys=True, default=str)}")


def value(i):
    return f"v{i:06d}-" + "x" * 64


def main():
    from seedbox import World
    redis = Redis()
    config = {k: redis.call("CONFIG", "GET", k)[1] for k in ("appendonly", "appendfsync", "save", "dir")}
    check(config["appendonly"] == "yes" and config["appendfsync"] == "always" and config["save"] == "",
          "redis runs with appendonly yes, appendfsync always and no snapshots", config)
    acknowledged = {}
    increments = 0

    def write(i):
        nonlocal increments
        check(redis.call("SET", f"k:{i:06d}", value(i)) == "OK", "SET acknowledged", i)
        acknowledged[f"k:{i:06d}"] = value(i)
        if i % 10 == 0:
            check(redis.call("INCR", "counter") == increments + 1, "INCR acknowledged", i)
            increments += 1

    for start in range(0, PREFILL, BATCH):
        pairs = []
        for j in range(start, start + BATCH):
            pairs += [f"p:{j:06d}", value(j) * 8]
        check(redis.call("MSET", *pairs) == "OK", "MSET acknowledged", start)
        for j in range(start, start + BATCH):
            acknowledged[f"p:{j:06d}"] = value(j) * 8
    for i in range(BEFORE):
        write(i)
    started = redis.call("BGREWRITEAOF")
    check("rewriting started" in started, "BGREWRITEAOF started at once", started)
    i = BEFORE
    # Keep writing while the child rewrites. A write counts as overlapping when
    # the rewrite is still in progress after its acknowledgement.
    overlapping = 0
    while True:
        stats = info(redis, "persistence")
        if stats["aof_rewrite_in_progress"] == "0" and stats["aof_rewrite_scheduled"] == "0":
            break
        if i > BEFORE:
            overlapping += 1
        write(i)
        i += 1
        check(i < 1_000_000, "rewrite never finished", stats)
    check(overlapping > 0, "writes were acknowledged during the rewrite", stats)
    check(stats["aof_last_bgrewrite_status"] == "ok", "rewrite succeeded", stats)
    rewrite = {k: stats[k] for k in ("aof_rewrites", "aof_last_bgrewrite_status", "aof_last_write_status")}
    check(int(stats["aof_rewrites"]) == 1, "exactly one rewrite ran", stats)
    for _ in range(AFTER):
        write(i)
        i += 1
    before_crash = info(redis, "persistence")
    check(before_crash["aof_last_write_status"] == "ok", "AOF writes succeeded", before_crash)
    redis.close()
    with World.connect() as world:
        # Straight after the last acknowledgement.
        world.crash("redis")
        world.reboot("redis")
    redis = Redis()
    while info(redis, "persistence").get("loading") == "1":
        time.sleep(0.2)
    loaded = info(redis, "persistence")
    keys = sorted(acknowledged)
    missing, wrong = [], []
    for start in range(0, len(keys), 500):
        batch = keys[start:start + 500]
        for key, got in zip(batch, redis.call("MGET", *batch)):
            if got is None:
                missing.append(key)
            elif got != acknowledged[key]:
                wrong.append(key)
    check(not missing and not wrong, "every acknowledged key survives the crash",
          {"missing": missing[:20], "missing_count": len(missing), "wrong": wrong[:20]})
    counter = int(redis.call("GET", "counter"))
    check(counter == increments, "counter equals acknowledged increments", [counter, increments])
    size = redis.call("DBSIZE")
    check(size == len(acknowledged) + 1, "no unacknowledged keys appear", [size, len(acknowledged)])
    evidence = {"config": config, "writes": len(acknowledged), "during_rewrite": overlapping,
                "increments": increments, "rewrite": rewrite, "dbsize": size,
                "loaded": {k: loaded.get(k) for k in ("aof_enabled", "loading", "aof_last_write_status")}}
    text = json.dumps(evidence, sort_keys=True)
    print("EVIDENCE " + text, flush=True)
    print("RESULT pass " + hashlib.sha256(text.encode()).hexdigest(), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
