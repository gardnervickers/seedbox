"""ENOSPC on a retained ext4 volume, then recovery, a new record and a crash.

The store preserves a flushed sentinel, fills the volume until ENOSPC with
preallocated fillers and then ordinary writes (whose partial outcome it records),
checks that a further small file cannot be written, removes one filler and
syncs, then writes and flushes a second tagged sentinel. The oracle crashes and
reboots the store and checks that both sentinels survive byte-exact, that the
fsynced bytes of the partial fill survive, and that the freed space is usable
again. The outcomes (filler count, partial bytes, errno names) are printed as
evidence, which a replay must reproduce.
"""
import errno
import hashlib
import json
import os
import socket
import sys
import time

ROOT = "/data"
PORT = 7000
SENTINEL = 4096
FILLERS = (64 << 20, 1 << 20, 64 << 10)
CHUNK = 64 << 10


def sentinel(tag):
    unit = f"sentinel {tag} ".encode()
    return (unit * (SENTINEL // len(unit) + 1))[:SENTINEL]


def chunk(index):
    unit = f"fill {index:06} ".encode()
    return (unit * (CHUNK // len(unit) + 1))[:CHUNK]


# ---------------------------------------------------------------- store side

def fsync_dir(path=ROOT):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def write_durably(name, data):
    path = os.path.join(ROOT, name)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        view = memoryview(data)
        while view:
            view = view[os.write(fd, view):]
        os.fsync(fd)
    finally:
        os.close(fd)
    fsync_dir()


def fill():
    """Fill the volume; returns the outcome of every stage."""
    outcome = {"fillers": [], "partial": {}}
    for size in FILLERS:
        count = 0
        while True:
            name = f"filler-{size}-{count:04}"
            fd = os.open(os.path.join(ROOT, name), os.O_WRONLY | os.O_CREAT, 0o644)
            try:
                os.posix_fallocate(fd, 0, size)
                count += 1
            except OSError as error:
                if error.errno != errno.ENOSPC:
                    raise
                os.close(fd)
                os.unlink(os.path.join(ROOT, name))
                break
            os.close(fd)
        outcome["fillers"].append({"size": size, "count": count})
    fsync_dir()
    # Ordinary writes into the remaining blocks until the allocator refuses.
    fd = os.open(os.path.join(ROOT, "tail.fill"), os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    written, short, failure = 0, 0, None
    index = 0
    while failure is None:
        data = memoryview(chunk(index))
        while data:
            try:
                count = os.write(fd, data)
            except OSError as error:
                failure = errno.errorcode[error.errno]
                break
            if count < len(data):
                short += 1
            written += count
            data = data[count:]
        index += 1
    try:
        os.fsync(fd)
        synced = "ok"
    except OSError as error:
        synced = errno.errorcode[error.errno]
    os.close(fd)
    outcome["partial"] = {"written": written, "short_writes": short, "error": failure,
                          "fsync": synced}
    try:
        write_durably("refused", b"x" * SENTINEL)
        outcome["full_write"] = "ok"
    except OSError as error:
        outcome["full_write"] = errno.errorcode[error.errno]
    return outcome


def recover():
    found = {}
    for name in ("sentinel-a", "sentinel-b"):
        path = os.path.join(ROOT, name)
        if os.path.exists(path):
            with open(path, "rb") as f:
                found[name] = hashlib.sha256(f.read()).hexdigest()
    path = os.path.join(ROOT, "tail.fill")
    if os.path.exists(path):
        with open(path, "rb") as f:
            data = f.read()
        intact = 0
        while intact * CHUNK < len(data) and data[intact * CHUNK:(intact + 1) * CHUNK] \
                == chunk(intact)[:len(data) - intact * CHUNK]:
            intact += 1
        found["tail"] = {"size": len(data), "intact_prefix": min(len(data), intact * CHUNK)}
    stat = os.statvfs(ROOT)
    found["free_blocks"] = stat.f_bfree
    found["fillers"] = len([n for n in os.listdir(ROOT) if n.startswith("filler-")])
    return found


def store():
    os.makedirs(ROOT, exist_ok=True)
    recovered = recover()
    boot = open("/proc/sys/kernel/random/boot_id").read().strip()
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", PORT))
    acceptor.listen()
    print(f"STORE_READY {json.dumps(recovered, sort_keys=True)}", flush=True)
    while True:
        channel, _ = acceptor.accept()
        with channel:
            for line in channel.makefile("rb"):
                request = json.loads(line)

                def send(**message):
                    channel.sendall(json.dumps(message).encode() + b"\n")

                if request["op"] == "recover":
                    send(boot=boot, **recovered)
                elif request["op"] == "sentinel":
                    write_durably(f"sentinel-{request['tag']}", sentinel(request["tag"]))
                    send(ack=request["tag"])
                elif request["op"] == "fill":
                    send(**fill())
                elif request["op"] == "free":
                    victim = sorted(n for n in os.listdir(ROOT) if n.startswith(f"filler-{FILLERS[0]}-"))[-1]
                    os.unlink(os.path.join(ROOT, victim))
                    os.sync()
                    send(freed=victim, free_blocks=os.statvfs(ROOT).f_bfree)
                else:
                    raise RuntimeError(f"unknown op {request['op']}")


# --------------------------------------------------------------- oracle side

def verify(found, acked, outcome):
    errors = []
    for tag in acked:
        name = f"sentinel-{tag}"
        if found.get(name) != hashlib.sha256(sentinel(tag)).hexdigest():
            errors.append(f"acknowledged {name} lost or changed")
    partial = outcome["partial"]
    if partial["error"] != "ENOSPC":
        errors.append(f"filling ended with {partial['error']}, not ENOSPC")
    if outcome["full_write"] != "ENOSPC":
        errors.append(f"a write on the full volume returned {outcome['full_write']}")
    tail = found.get("tail")
    if partial["fsync"] == "ok":
        if tail is None or tail["intact_prefix"] < partial["written"]:
            errors.append(f"fsynced fill of {partial['written']} bytes recovered as {tail}")
    elif tail is not None and tail["intact_prefix"] != tail["size"]:
        errors.append(f"unsynced fill recovered with garbage: {tail}")
    if not outcome["fillers"][0]["count"]:
        errors.append("no 64 MiB filler fit on the volume")
    return errors


class Connection:
    def __init__(self):
        deadline = time.monotonic() + 60
        while True:
            try:
                self.channel = socket.create_connection(("store", PORT), timeout=300)
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise RuntimeError("store readiness timed out")
                time.sleep(0.1)
        self.reader = self.channel.makefile("rb")

    def call(self, **request):
        self.channel.sendall(json.dumps(request).encode() + b"\n")
        line = self.reader.readline()
        if not line:
            raise RuntimeError("store closed the connection")
        return json.loads(line)


def oracle():
    from seedbox import World
    with World.connect() as world:
        connection = Connection()
        first = connection.call(op="recover")
        if "sentinel-a" in first or first["fillers"]:
            raise RuntimeError(f"volume not fresh: {first}")
        connection.call(op="sentinel", tag="a")
        outcome = connection.call(op="fill")
        print("FILLED " + json.dumps(outcome, sort_keys=True), flush=True)
        freed = connection.call(op="free")
        connection.call(op="sentinel", tag="b")
        world.crash("store")
        connection.channel.close()
        world.sleep(0.2)
        world.reboot("store")
        found = Connection().call(op="recover")
    if found["boot"] == first["boot"]:
        raise RuntimeError("store did not reboot")
    errors = verify(found, ("a", "b"), outcome)
    if found["fillers"] != sum(f["count"] for f in outcome["fillers"]) - 1:
        errors.append(f"{found['fillers']} fillers after freeing one of "
                      f"{sum(f['count'] for f in outcome['fillers'])}")
    for error in errors:
        print("ENOSPC_ERROR", error, flush=True)
    del found["boot"]
    print("EVIDENCE " + json.dumps({"filled": outcome, "freed": freed, "recovered": found},
                                   sort_keys=True), flush=True)
    if errors:
        return 1
    print(f"RESULT pass partial_bytes={outcome['partial']['written']} "
          f"fillers={[f['count'] for f in outcome['fillers']]}", flush=True)
    return 0


def main():
    role = sys.argv[1]
    if role == "store":
        store()
    elif role == "oracle":
        return oracle()
    else:
        raise RuntimeError(f"unknown role {role}")


if __name__ == "__main__":
    raise SystemExit(main())
