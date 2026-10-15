"""A checksummed sector journal across two crashes with partial survival.

The store keeps a journal of 512-byte records, each one sector, in a
preallocated and flushed file, written with O_DIRECT so every record reaches
the device as one sector write. Each generation writes FLUSHED records with
fdatasync after each, then UNFLUSHED records that reach the device's volatile
layer but are never flushed. The oracle crashes the store; after the reboot the
store reports every sector. Recovery keeps the longest valid prefix, zeroes and
flushes everything after it (repair), and the next generation starts there and
overwrites any survivors beyond the prefix. Then a second crash.

After each crash the oracle checks every sector against the disk model:
flushed records and repaired sectors are exactly as written; a sector written
but never flushed holds either the new record or what was stable before it,
never a survivor the repair overwrote. With `--policy drop` (the default model)
no unflushed sector survives. With `--policy torn` (simulator.disk.torn_writes,
each unflushed sector kept with probability one half) the two crashes must
together keep some and lose some unflushed records. The recovery ledgers and
survivor digests are printed as evidence, which a replay must reproduce.
"""
import hashlib
import json
import mmap
import os
import socket
import sys
import time
import zlib

ROOT = "/data"
JOURNAL = os.path.join(ROOT, "journal.dat")
PORT = 7000
SECTOR = 512
SECTORS = 256
FLUSHED = 16
UNFLUSHED = 32
MAGIC = b"SBXJ"


def record(gen, seq):
    """One sector: magic, generation, sequence, payload and a trailing CRC32."""
    head = MAGIC + f" g{gen} s{seq:04} ".encode()
    body = bytes((gen * 89 + seq * 13 + i) % 253 for i in range(SECTOR - len(head) - 8))
    data = head + body
    return data + f"{zlib.crc32(data):08x}".encode()


def classify(sector):
    """'zero', [gen, seq] for a valid record, or 'garbage'."""
    if sector == bytes(SECTOR):
        return "zero"
    if sector.startswith(MAGIC):
        try:
            gen = int(sector[6:7])
            seq = int(sector[9:13])
        except ValueError:
            return "garbage"
        if sector == record(gen, seq):
            return [gen, seq]
    return "garbage"


def valid_prefix(sectors):
    """Records 0.. that form one unbroken sequence of generations."""
    count = 0
    while count < len(sectors) and isinstance(sectors[count], list):
        count += 1
    return count


# ---------------------------------------------------------------- store side

class Journal:
    def __init__(self):
        fresh = not os.path.exists(JOURNAL)
        if fresh:
            with open(JOURNAL, "wb") as f:
                f.write(bytes(SECTOR * SECTORS))
                f.flush()
                os.fsync(f.fileno())
            fd = os.open(ROOT, os.O_RDONLY | os.O_DIRECTORY)
            os.fsync(fd)
            os.close(fd)
        self.fd = os.open(JOURNAL, os.O_RDWR | os.O_DIRECT)
        self.buffer = mmap.mmap(-1, SECTOR * SECTORS)  # Page-aligned for O_DIRECT.

    def write(self, sector, data):
        self.buffer[:SECTOR] = data
        if os.pwrite(self.fd, memoryview(self.buffer)[:SECTOR], sector * SECTOR) != SECTOR:
            raise RuntimeError("short O_DIRECT write")

    def read(self):
        view = memoryview(self.buffer)
        if os.preadv(self.fd, [view], 0) != SECTOR * SECTORS:
            raise RuntimeError("short O_DIRECT read")
        return bytes(view)


def store():
    os.makedirs(ROOT, exist_ok=True)
    journal = Journal()
    boot = open("/proc/sys/kernel/random/boot_id").read().strip()
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", PORT))
    acceptor.listen()
    print("STORE_READY", flush=True)
    while True:
        channel, _ = acceptor.accept()
        with channel:
            for line in channel.makefile("rb"):
                request = json.loads(line)

                def send(**message):
                    channel.sendall(json.dumps(message).encode() + b"\n")

                if request["op"] == "recover":
                    data = journal.read()
                    sectors = [classify(data[i * SECTOR:(i + 1) * SECTOR]) for i in range(SECTORS)]
                    garbage = {i: data[i * SECTOR:i * SECTOR + 32].hex()
                               for i, s in enumerate(sectors) if s == "garbage"}
                    send(boot=boot, sectors=sectors, garbage=garbage,
                         digest=hashlib.sha256(data).hexdigest())
                elif request["op"] == "repair":
                    for sector in range(request["from"], SECTORS):
                        journal.write(sector, bytes(SECTOR))
                    os.fdatasync(journal.fd)
                    send(repaired=SECTORS - request["from"])
                elif request["op"] == "write":
                    gen, start = request["gen"], request["start"]
                    for seq in range(FLUSHED + UNFLUSHED):
                        journal.write(start + seq, record(gen, seq))
                        if seq < FLUSHED:
                            os.fdatasync(journal.fd)
                            send(ack=seq)
                    send(done=gen)
                else:
                    raise RuntimeError(f"unknown op {request['op']}")


# --------------------------------------------------------------- oracle side

def check_crash(sectors, stable, written, policy):
    """Errors and survivor count after one crash.

    `stable` is each sector's content before the generation, as classify()
    reports it; `written` maps sector to (record, flushed) for the generation.
    """
    errors, survived, lost = [], 0, 0
    for i, found in enumerate(sectors):
        if i in written:
            want, flushed = written[i]
            if flushed:
                if found != want:
                    errors.append(f"sector {i}: flushed record {want} is {found}")
            elif found == want:
                survived += 1
            elif found == stable[i]:
                lost += 1
            else:
                errors.append(f"sector {i}: unflushed {want} is {found}, stable was {stable[i]}")
        elif found != stable[i]:
            errors.append(f"sector {i}: unwritten sector changed from {stable[i]} to {found}")
    if policy == "drop" and survived:
        errors.append(f"{survived} unflushed sectors survived although they are dropped")
    return errors, survived, lost


class Connection:
    def __init__(self):
        deadline = time.monotonic() + 60
        while True:
            try:
                self.channel = socket.create_connection(("store", PORT), timeout=30)
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise RuntimeError("store readiness timed out")
                time.sleep(0.1)
        self.reader = self.channel.makefile("rb")

    def send(self, **request):
        self.channel.sendall(json.dumps(request).encode() + b"\n")

    def line(self):
        line = self.reader.readline()
        if not line:
            raise RuntimeError("store closed the connection")
        return json.loads(line)

    def call(self, **request):
        self.send(**request)
        return self.line()


def oracle(policy):
    from seedbox import World
    ledgers, errors, survived, lost = [], [], 0, 0
    with World.connect() as world:
        connection = Connection()
        found = connection.call(op="recover")
        boots = {found["boot"]}
        stable = found["sectors"]
        if any(s != "zero" for s in stable):
            raise RuntimeError("journal not fresh")
        start = 0
        for gen in (1, 2):
            written = {start + seq: ([gen, seq], seq < FLUSHED) for seq in range(FLUSHED + UNFLUSHED)}
            connection.send(op="write", gen=gen, start=start)
            while "done" not in connection.line():
                pass
            world.crash("store")
            connection.channel.close()
            world.sleep(0.2)
            world.reboot("store")
            connection = Connection()
            found = connection.call(op="recover")
            if found["boot"] in boots:
                raise RuntimeError("store did not reboot")
            boots.add(found["boot"])
            sectors = found["sectors"]
            crash_errors, kept, dropped = check_crash(sectors, stable, written, policy)
            errors += [f"crash {gen}: {e}" for e in crash_errors]
            survived, lost = survived + kept, lost + dropped
            prefix = valid_prefix(sectors)
            ledgers.append({"crash": gen, "prefix": prefix, "survived": kept, "lost": dropped,
                            "orphans": sum(isinstance(s, list) for s in sectors[prefix:]),
                            "digest": found["digest"][:16]})
            print(f"RECOVERED {json.dumps(ledgers[-1], sort_keys=True)}", flush=True)
            if errors:
                break
            connection.call(op="repair", **{"from": prefix})
            stable = sectors[:prefix] + ["zero"] * (len(sectors) - prefix)
            start = prefix
        if not errors:
            # The repair of the last generation is flushed: check it as written.
            found = connection.call(op="recover")
            if found["sectors"] != stable:
                errors.append("final repair did not read back as written")
    if policy == "torn" and not errors and not (survived and lost):
        errors.append(f"torn-write model kept {survived} and lost {lost} unflushed records")
    for error in errors:
        print("JOURNAL_ERROR", error, flush=True)
    print("EVIDENCE " + json.dumps(ledgers, sort_keys=True), flush=True)
    if errors:
        return 1
    print(f"RESULT pass policy={policy} survived={survived} lost={lost}", flush=True)
    return 0


def main():
    role = sys.argv[1]
    if role == "store":
        store()
    elif role == "oracle":
        policy = sys.argv[sys.argv.index("--policy") + 1] if "--policy" in sys.argv else "drop"
        if policy not in ("drop", "torn"):
            raise RuntimeError(f"unknown policy {policy}")
        return oracle(policy)
    else:
        raise RuntimeError(f"unknown role {role}")


if __name__ == "__main__":
    raise SystemExit(main())
