"""Whole-run determinism fixture (crates/seedbox/tests/determinism.rs).

`server` serves digests of seeded reads of a file on its volume; `worker` rewrites code while
threads execute it (`/rewrite`), then makes a numbered TCP exchange with the
server; the oracle prints markers at fixed simulated times, each followed by a
child blocked on a pipe, partitions and heals the worker from the server, and
checks the worker's final digest. The run lasts about 20 simulated seconds so
external inputs at 5, 10, 12 and 15 seconds all land inside it.
"""
import hashlib
import os
import socket
import subprocess
import sys
import threading
import time

REQUESTS = 72
INTERVAL = 0.25
MARKS = (3, 6, 9, 13, 16)
PARTITION = (7, 8)


def connect(host, port):
    while True:
        channel = socket.socket()
        try:
            channel.connect((host, port))
            return channel
        except OSError:
            channel.close()
            time.sleep(0.25)


def server():
    data = os.urandom(1 << 20)  # Guest entropy: equal for one seed.
    # Written to a volume, so checkpoints and exports carry changed disk data.
    with open("/data/data.bin", "wb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
    print("server data", hashlib.sha256(data).hexdigest()[:16], flush=True)
    fd = os.open("/data/data.bin", os.O_RDONLY)

    def serve(channel):
        with channel, channel.makefile("rwb") as stream:
            for line in stream:
                seq, offset, length = map(int, line.split())
                chunk = os.pread(fd, length, offset)
                digest = hashlib.sha256(chunk).hexdigest()[:16]
                # About 1 KiB of output per request, so stalled log readers see backpressure.
                print("server", seq, digest, chunk[:512].hex(), flush=True)
                stream.write(f"{seq} {digest}\n".encode())
                stream.flush()

    with socket.socket() as listener:
        listener.bind(("0.0.0.0", 7000))
        listener.listen()
        while True:
            threading.Thread(target=serve, args=(listener.accept()[0],), daemon=True).start()


def worker():
    rewrite = subprocess.run(["/rewrite", "300", "300000"], capture_output=True, text=True)
    print(rewrite.stdout, end="", flush=True)
    if rewrite.returncode:
        sys.exit(f"rewrite failed: {rewrite.returncode}")
    seed = int.from_bytes(os.urandom(8), "little")
    digest = hashlib.sha256()
    with connect("server", 7000) as channel, channel.makefile("rwb") as stream:
        for seq in range(REQUESTS):
            offset = (seed * (seq + 1) * 2654435761) % ((1 << 20) - 4096)
            length = 1 + (seed >> (seq % 48)) % 4096
            stream.write(f"{seq} {offset} {length}\n".encode())
            stream.flush()
            reply = stream.readline().decode()
            if not reply.startswith(f"{seq} "):
                sys.exit(f"bad reply {reply!r}")
            digest.update(reply.encode())
            if seq % 12 == 11:
                print(f"worker seq={seq} digest={digest.hexdigest()[:16]}", flush=True)
            time.sleep(INTERVAL)
    result = digest.hexdigest()[:16]
    print("worker done", result, flush=True)
    with socket.socket() as listener:
        listener.bind(("0.0.0.0", 7001))
        listener.listen()
        while True:
            with listener.accept()[0] as channel:
                channel.sendall(result.encode() + b"\n")


def blocked_child(mark):
    """A child blocks reading a pipe that its parent fills a little later."""
    read, write = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(write)
        data = os.read(read, 64)
        os._exit(0 if data == f"mark {mark}".encode() else 1)
    os.close(read)
    time.sleep(0.5)
    os.write(write, f"mark {mark}".encode())
    os.close(write)
    _, status = os.waitpid(pid, 0)
    if status:
        sys.exit(f"blocked child {mark} failed: {status}")


def scenario():
    from seedbox import World
    with World.connect() as world:
        def until(seconds):
            while world.now_ns() < seconds * 1_000_000_000:
                time.sleep(0.05)

        with connect("server", 7000) as channel, channel.makefile("rwb") as stream:
            stream.write(b"0 0 64\n")
            stream.flush()
            print("oracle probe", stream.readline().decode().split()[1], flush=True)
        handle = None
        for mark in MARKS:
            if PARTITION[0] < mark and handle is None:
                until(PARTITION[0])
                handle = world.partition(["worker"], ["server"])
                print("oracle partitioned", flush=True)
            if PARTITION[1] < mark and handle is not None and handle != "healed":
                until(PARTITION[1])
                world.heal(handle)
                handle = "healed"
                print("oracle healed", flush=True)
            until(mark)
            print(f"MARK {mark}", flush=True)
            blocked_child(mark)
        with connect("worker", 7001) as channel:
            result = channel.makefile("rb").readline().decode().strip()
        print("oracle worker digest", result, flush=True)
        if len(result) != 16:
            sys.exit("missing worker digest")


if __name__ == "__main__":
    {"server": server, "worker": worker, "scenario": scenario}[sys.argv[1]]()
