"""Randomness observed over a long run (seedbox determinism counterexample).

Every 5 s of virtual time for 70 s, after a burst of disk and loopback
socket work, print every randomness and address-layout observer we can reach
from Python. Two runs with the same seed must print identical bytes.
"""
import ctypes
import hashlib
import mmap
import os
import socket
import subprocess
import sys
import time

SAMPLES = 14
INTERVAL = 5.0
CONNECTS = 200      # loopback connects per interval (network/softirq work)
FILE_OPS = 32       # small write+fsync+read cycles per interval (disk interrupts)
WORDS = ["ant", "bee", "cat", "dog", "eel", "fox", "gnu", "hen", "ibis", "jay",
         "kite", "lynx", "mole", "newt", "owl", "pig", "quail", "rat", "seal", "toad"]
CHILD = "import os; print(os.urandom(8).hex(), hash('x'))"

lines = []


def out(line):
    print(line, flush=True)


def emit(index, key, value):
    line = f"s{index:02d} {key}: {value}"
    lines.append(line)
    out(line)


def busy(listener, index):
    path = f"/tmp/seedbox-randomness-{index}"
    for i in range(FILE_OPS):
        with open(path, "wb") as f:
            f.write(bytes([i & 0xFF]) * 512)
            f.flush()
            os.fsync(f.fileno())
        with open(path, "rb") as f:
            f.read()
    os.unlink(path)
    addr = listener.getsockname()
    for _ in range(CONNECTS):
        c = socket.create_connection(addr)
        s, _ = listener.accept()
        s.close()
        c.close()


def mmap_address(m):
    view = (ctypes.c_char * 1).from_buffer(m)
    address = ctypes.addressof(view)
    del view
    return address


def sample(index, listener):
    emit(index, "getrandom", os.getrandom(16).hex())
    with open("/proc/sys/kernel/random/uuid") as f:
        emit(index, "uuid", f.read().strip().replace("-", ""))
    emit(index, "hash_str", hash("seedbox"))
    emit(index, "hash_tuple", hash(("a", 1)))
    emit(index, "set_order", ",".join(set(w + str(index) for w in WORDS)))
    a = mmap.mmap(-1, 2 << 20)
    b = mmap.mmap(-1, 2 << 20)
    emit(index, "mmap", f"{mmap_address(a):x},{mmap_address(b):x}")
    a.close()
    b.close()
    c = socket.create_connection(listener.getsockname())
    emit(index, "ephemeral_port", c.getsockname()[1])
    s, _ = listener.accept()
    s.close()
    c.close()
    child = subprocess.run([sys.executable, "-c", CHILD], capture_output=True,
                           text=True, check=True)
    emit(index, "child", child.stdout.strip().replace(" ", ","))
    lib = ctypes.CDLL(None)
    emit(index, "dlsym_getpid", f"{ctypes.cast(lib.getpid, ctypes.c_void_p).value:x}")
    with open("/proc/sys/kernel/random/entropy_avail") as f:
        emit(index, "entropy_avail", f.read().strip())


def main():
    out(f"pythonhashseed: {os.environ.get('PYTHONHASHSEED', 'unset')}")
    out(f"samples: {SAMPLES}")
    out(f"interval_s: {INTERVAL:g}")
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(64)
    start = time.monotonic()
    for index in range(1, SAMPLES + 1):
        busy(listener, index)
        remaining = start + index * INTERVAL - time.monotonic()
        if remaining > 0:
            time.sleep(remaining)
        sample(index, listener)
    listener.close()
    out("digest: " + hashlib.sha256("\n".join(lines).encode() + b"\n").hexdigest())
    return 0


if __name__ == "__main__":
    sys.exit(main())
