"""Concurrent stdout/stderr storms across a process restart and a reboot.

Every write the app makes is one self-describing record:

    \\x1eREC b=<boot> p=<process> t=<thread> s=<sequence> n=<length>\\x1f<body>

where the body (NULs, ANSI escapes and binary bytes) is a function of the
header. `boot` is a nonce from /proc's boot_id, `process` counts starts within
one boot. A storm has three phases:

* atomic: four threads write records of 64 bytes, 512 bytes, PIPE_BUF (4096)
  bytes and 2048 bytes concurrently, so only pipe atomicity keeps each whole;
* large: one thread writes 64 KiB and 1 MiB records, each in one write();
* bytewise: one thread writes a 4 KiB record to stderr one byte per write()
  while the other three write atomic records to stdout.

No two writers ever share a stream with a write larger than PIPE_BUF, so each
stream must parse as whole records. Before replying, the app waits until both
pipes are empty, so a later crash loses nothing that was written. The oracle runs
a full storm (8 MiB stdout, 2 MiB stderr), stops and restarts the app, runs a small storm,
crashes and reboots the machine, runs another small storm and prints the app's
totals; applications.rs checks the `logs` byte accounting, record integrity,
incarnation attribution and replay ordering against them.
"""
import fcntl
import json
import os
import socket
import struct
import sys
import termios
import threading
import time

PORT = 7000
PIPE_BUF = 4096
ATOMIC = ((1, 64), (2, 512), (1, PIPE_BUF), (0, 2048))  # (stream fd, or 0 to alternate; size)
FULL = {"atomic": (4096, 2048, 512, 512), "large": ((1, 64 << 10, 16), (1, 1 << 20, 4), (2, 64 << 10, 8)),
        "bytewise": 4096, "bytewise_atomic": 64}
SMALL = {"atomic": (512, 128, 64, 64), "large": ((1, 64 << 10, 4), (2, 64 << 10, 1)),
         "bytewise": 1024, "bytewise_atomic": 16}
ANSI = (b"\x1b[31m", b"\x1b[0m", b"\x1b[2K", b"\x1b[1;32m")


def header(boot, process, thread, seq, length):
    return f"\x1eREC b={boot} p={process} t={thread} s={seq} n={length}\x1f".encode()


def unit(boot, process, thread):
    """251 pseudo-random bytes with NULs and ANSI escapes, never 0x1e."""
    seed = (int(boot, 16) ^ (process << 20) ^ (thread << 16)) & 0xFFFFFFFF
    out = bytearray()
    for i in range(251):
        seed = (seed * 1103515245 + 12345) & 0x7FFFFFFF
        value = (seed >> 16) & 0xFF
        out.append(0 if i % 17 == 0 else 0x1d if value == 0x1e else value)
        if i % 61 == 0:
            out += ANSI[(seed >> 8) % len(ANSI)]
    return bytes(out)


def body(pattern, seq, length):
    """The writer's pattern, rotated by the sequence number, to `length` bytes."""
    k = seq % len(pattern)
    rotated = pattern[k:] + pattern[:k]
    return (rotated * (length // len(rotated) + 1))[:length]


def record(boot, process, thread, seq, size, pattern=None):
    """A record of exactly `size` bytes, header included."""
    length = size - len(header(boot, process, thread, seq, size))
    while (total := len(header(boot, process, thread, seq, length)) + length) != size:
        length += size - total
    pattern = pattern or unit(boot, process, thread)
    return header(boot, process, thread, seq, length) + body(pattern, seq, length)


# ------------------------------------------------------------------- app side

class Writer:
    def __init__(self, boot, process):
        self.boot, self.process = boot, process
        self.seq = [0] * 5
        self.totals = {"stdout": 0, "stderr": 0}
        self.records = {"stdout": 0, "stderr": 0}
        self.lock = threading.Lock()
        self.patterns = [unit(boot, process, t) for t in range(5)]

    def emit(self, thread, fd, size):
        data = record(self.boot, self.process, thread, self.seq[thread], size, self.patterns[thread])
        self.seq[thread] += 1
        view = memoryview(data)
        while view:
            view = view[os.write(fd, view):]
        self.count(fd, len(data))

    def count(self, fd, size, records=1):
        name = "stdout" if fd == 1 else "stderr"
        with self.lock:
            self.totals[name] += size
            self.records[name] += records

    def bytewise(self, thread, fd, size):
        data = record(self.boot, self.process, thread, self.seq[thread], size, self.patterns[thread])
        self.seq[thread] += 1
        for i in range(len(data)):
            os.write(fd, data[i:i + 1])
        self.count(fd, len(data))

    def storm(self, plan):
        def atomic(thread, count, only_stdout=False):
            fd, size = ATOMIC[thread]
            for n in range(count):
                self.emit(thread, 1 if only_stdout else (fd or (1 + n % 2)), size)

        threads = [threading.Thread(target=atomic, args=(t, plan["atomic"][t])) for t in range(4)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        for fd, size, count in plan["large"]:
            for _ in range(count):
                self.emit(4, fd, size)
        threads = [threading.Thread(target=self.bytewise, args=(0, 2, plan["bytewise"]))]
        threads += [threading.Thread(target=atomic, args=(t, plan["bytewise_atomic"], True))
                    for t in (1, 2, 3)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        # Wait for the supervisor to read everything already written.
        for fd in (1, 2):
            while struct.unpack("i", fcntl.ioctl(fd, termios.FIONREAD, b"\0" * 4))[0]:
                time.sleep(0.01)
        return {"boot": self.boot, "process": self.process, "bytes": dict(self.totals),
                "records": dict(self.records), "sequences": self.seq}


def app():
    boot = open("/proc/sys/kernel/random/boot_id").read().strip().replace("-", "")[:8]
    starts = "/run/output-storm-starts"
    process = int(open(starts).read()) + 1 if os.path.exists(starts) else 1
    with open(starts, "w") as f:
        f.write(str(process))
    writer = Writer(boot, process)
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", PORT))
    acceptor.listen()
    while True:
        channel, _ = acceptor.accept()
        with channel:
            for line in channel.makefile("rb"):
                plan = FULL if json.loads(line)["plan"] == "full" else SMALL
                channel.sendall(json.dumps(writer.storm(plan)).encode() + b"\n")


# --------------------------------------------------------------- oracle side

def storm(plan):
    deadline = time.monotonic() + 60
    while True:
        try:
            channel = socket.create_connection(("app", PORT), timeout=600)
            break
        except OSError:
            if time.monotonic() >= deadline:
                raise RuntimeError("app readiness timed out")
            time.sleep(0.1)
    with channel:
        channel.sendall(json.dumps({"plan": plan}).encode() + b"\n")
        line = channel.makefile("rb").readline()
    if not line:
        raise RuntimeError("app closed the connection")
    return json.loads(line)


def oracle():
    from seedbox import World
    totals = []
    with World.connect() as world:
        totals.append(storm("full"))
        world.sleep(0.5)
        world.stop("app")
        world.restart("app")
        totals.append(storm("small"))
        world.sleep(0.5)
        world.crash("app")
        world.sleep(0.2)
        world.reboot("app")
        totals.append(storm("small"))
        world.sleep(0.5)
    if [t["process"] for t in totals] != [1, 2, 1] or len({t["boot"] for t in totals}) != 2 \
            or totals[0]["boot"] != totals[1]["boot"]:
        print("STORM_ERROR unexpected boots or process starts: "
              + json.dumps([(t["boot"], t["process"]) for t in totals]), flush=True)
        return 1
    print("TOTALS " + json.dumps(totals, sort_keys=True), flush=True)
    print("RESULT pass", flush=True)
    return 0


if __name__ == "__main__":
    role = sys.argv[1]
    if role == "app":
        app()
    else:
        raise SystemExit(oracle())
