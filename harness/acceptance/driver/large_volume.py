"""A volume larger than the 1 GiB default, filled past 1 GiB, survives a crash.

The store's volume declares `size: 2GiB`. The store writes 1.25 GiB of tagged
64 MiB files, fsyncing each file and the directory before acknowledging it,
then preallocates and fsyncs a 512 MiB file. The oracle crashes and reboots
the store and checks that the filesystem is still larger than 1 GiB and that
every acknowledged byte reads back exactly. The sizes and counts are printed
as evidence, which a replay must reproduce.
"""
import json
import os
import socket
import sys
import time

ROOT = "/data"
PORT = 7000
FILE = 64 << 20
FILES = 20            # 1.25 GiB of written data.
RESERVED = 512 << 20  # fallocated: allocated blocks without written data.
CHUNK = 1 << 20


def pattern(index):
    unit = f"large volume file {index:04} ".encode()
    return (unit * (CHUNK // len(unit) + 1))[:CHUNK]


def fsync_dir():
    fd = os.open(ROOT, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def usage():
    stat = os.statvfs(ROOT)
    return {"size": stat.f_blocks * stat.f_frsize,
            "used": (stat.f_blocks - stat.f_bfree) * stat.f_frsize}


# ---------------------------------------------------------------- store side

def write_file(index):
    data = pattern(index)
    fd = os.open(os.path.join(ROOT, f"file-{index:04}"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        for _ in range(FILE // CHUNK):
            view = memoryview(data)
            while view:
                view = view[os.write(fd, view):]
        os.fsync(fd)
    finally:
        os.close(fd)
    fsync_dir()


def reserve():
    fd = os.open(os.path.join(ROOT, "reserved"), os.O_WRONLY | os.O_CREAT, 0o644)
    try:
        os.posix_fallocate(fd, 0, RESERVED)
        os.fsync(fd)
    finally:
        os.close(fd)
    fsync_dir()


def recover():
    """Each file's length and how many leading bytes match its pattern."""
    files = {}
    for name in sorted(os.listdir(ROOT)):
        if not name.startswith("file-"):
            continue
        expected = pattern(int(name[5:]))
        intact = 0
        with open(os.path.join(ROOT, name), "rb") as f:
            while True:
                data = f.read(CHUNK)
                if not data:
                    break
                if data != expected[:len(data)]:
                    break
                intact += len(data)
            size = os.fstat(f.fileno()).st_size
        files[name] = {"size": size, "intact": intact}
    reserved = os.path.join(ROOT, "reserved")
    blocks = os.stat(reserved).st_blocks * 512 if os.path.exists(reserved) else None
    return {"files": files, "reserved_allocated": blocks, **usage()}


def store():
    os.makedirs(ROOT, exist_ok=True)
    recovered = recover()
    boot = open("/proc/sys/kernel/random/boot_id").read().strip()
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", PORT))
    acceptor.listen()
    print(f"STORE_READY files={len(recovered['files'])} size={recovered['size']}", flush=True)
    while True:
        channel, _ = acceptor.accept()
        with channel:
            for line in channel.makefile("rb"):
                request = json.loads(line)

                def send(**message):
                    channel.sendall(json.dumps(message).encode() + b"\n")

                if request["op"] == "recover":
                    send(boot=boot, **recovered)
                elif request["op"] == "write":
                    write_file(request["index"])
                    send(ack=request["index"], **usage())
                elif request["op"] == "reserve":
                    reserve()
                    send(reserved=RESERVED, **usage())
                else:
                    raise RuntimeError(f"unknown op {request['op']}")


# --------------------------------------------------------------- oracle side

class Connection:
    def __init__(self):
        deadline = time.monotonic() + 60
        while True:
            try:
                self.channel = socket.create_connection(("store", PORT), timeout=600)
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
        if first["files"]:
            raise RuntimeError(f"volume not fresh: {first['files']}")
        acked = []
        for index in range(FILES):
            connection.call(op="write", index=index)
            acked.append(index)
        filled = connection.call(op="reserve")
        print("FILLED " + json.dumps(filled, sort_keys=True), flush=True)
        world.crash("store")
        connection.channel.close()
        world.sleep(0.2)
        world.reboot("store")
        found = Connection().call(op="recover")
    errors = []
    if found["boot"] == first["boot"]:
        errors.append("store did not reboot")
    if not first["size"] > 1 << 30:
        errors.append(f"filesystem of {first['size']} bytes is not larger than 1 GiB")
    if not filled["used"] > (1 << 30) + RESERVED:
        errors.append(f"only {filled['used']} bytes used after the fill")
    for index in acked:
        name = f"file-{index:04}"
        got = found["files"].get(name)
        if got != {"size": FILE, "intact": FILE}:
            errors.append(f"acknowledged {name} recovered as {got}")
    if found["reserved_allocated"] is None or found["reserved_allocated"] < RESERVED:
        errors.append(f"fsynced reservation recovered as {found['reserved_allocated']}")
    for error in errors:
        print("LARGE_VOLUME_ERROR", error, flush=True)
    del found["boot"]
    print("EVIDENCE " + json.dumps({"fresh": first["size"], "filled": filled,
                                    "recovered": {k: v for k, v in found.items() if k != "files"},
                                    "files": len(found["files"])}, sort_keys=True), flush=True)
    if errors:
        return 1
    print(f"RESULT pass size={first['size']} used={filled['used']} files={len(acked)}", flush=True)
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
