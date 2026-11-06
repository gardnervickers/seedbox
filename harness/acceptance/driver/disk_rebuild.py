"""Acknowledged records across repeated crash/reboot rounds on one retained volume.

Each boot of the store machine replaces the machine's block device and DMA
regions. The store appends checksummed 128-byte records to one ledger with an
fsync per record and acknowledges each one only after fsync returns; each round
it also writes a bulk file of seeded bytes, fsyncs it, drops the page cache and
reads it back from the device. The oracle crashes the store immediately after a
seeded acknowledged record in each of four rounds, reboots it and checks, after
every successor boots:

* every acknowledged record of every earlier generation is present, in order
  and intact; a record after the last acknowledged one is either intact or the
  ledger ends inside it with zeros or a prefix of it;
* every acknowledged bulk file reads back with its digest;
* the per-generation ledger digests match what the oracle acknowledged.

The final boot only verifies and reports the per-generation digests, which a
replay must reproduce.
"""
import hashlib
import json
import os
import random
import socket
import sys
import time
import zlib

ROOT = "/data"
LEDGER = os.path.join(ROOT, "ledger.log")
PORT = 7000
RECORD = 128
ROUNDS = 4
BULK = 1 << 20


def record(gen, seq):
    """A self-describing 128-byte record ending in its CRC32 and a newline."""
    head = f"g{gen:02} s{seq:05} ".encode()
    body = bytes((gen * 37 + seq * 11 + i) % 251 for i in range(RECORD - len(head) - 9))
    data = head + body
    return data + f"{zlib.crc32(data):08x}".encode() + b"\n"


def bulk(gen):
    return random.Random(f"bulk-{gen}").randbytes(BULK)


def parse(ledger):
    """Intact records in order, then the trailing bytes that do not form one."""
    records, offset = [], 0
    while offset + RECORD <= len(ledger):
        piece = ledger[offset:offset + RECORD]
        try:
            gen, seq = int(piece[1:3]), int(piece[5:10])
        except ValueError:
            break
        if piece != record(gen, seq):
            break
        records.append((gen, seq))
        offset += RECORD
    return records, ledger[offset:]


# ---------------------------------------------------------------- store side

def drop_caches():
    os.sync()
    with open("/proc/sys/vm/drop_caches", "w") as f:
        f.write("3\n")


def recover():
    """What this boot finds, read from the device rather than the page cache."""
    drop_caches()
    found = {"ledger": b"", "bulk": {}}
    if os.path.exists(LEDGER):
        with open(LEDGER, "rb") as f:
            found["ledger"] = f.read()
    for name in sorted(os.listdir(ROOT)):
        if name.startswith("bulk-"):
            with open(os.path.join(ROOT, name), "rb") as f:
                found["bulk"][name] = hashlib.sha256(f.read()).hexdigest()
    records, tail = parse(found["ledger"])
    return {"records": records, "tail": tail.hex(), "bulk": found["bulk"],
            "size": len(found["ledger"])}


def store():
    os.makedirs(ROOT, exist_ok=True)
    recovered = recover()
    boot = open("/proc/sys/kernel/random/boot_id").read().strip()
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", PORT))
    acceptor.listen()
    print(f"STORE_READY records={len(recovered['records'])} size={recovered['size']}", flush=True)
    while True:
        channel, _ = acceptor.accept()
        with channel:
            for line in channel.makefile("rb"):
                request = json.loads(line)

                def send(**message):
                    channel.sendall(json.dumps(message).encode() + b"\n")

                if request["op"] == "recover":
                    send(boot=boot, **recovered)
                elif request["op"] == "bulk":
                    gen = request["gen"]
                    path = os.path.join(ROOT, f"bulk-{gen:02}")
                    data = bulk(gen)
                    with open(path + ".tmp", "wb") as f:
                        f.write(data)
                        f.flush()
                        os.fsync(f.fileno())
                    os.rename(path + ".tmp", path)
                    fd = os.open(ROOT, os.O_RDONLY | os.O_DIRECTORY)
                    os.fsync(fd)
                    os.close(fd)
                    drop_caches()
                    with open(path, "rb") as f:
                        digest = hashlib.sha256(f.read()).hexdigest()
                    send(bulk=gen, digest=digest)
                elif request["op"] == "append":
                    fd = os.open(LEDGER, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o644)
                    for seq in range(request["first"], request["first"] + request["count"]):
                        os.write(fd, record(request["gen"], seq))
                        os.fsync(fd)
                        send(ack=seq)
                    os.close(fd)
                    send(done=request["count"])
                else:
                    raise RuntimeError(f"unknown op {request['op']}")


# --------------------------------------------------------------- oracle side

def verify(found, acked, bulk_acked):
    """Errors for one boot against every acknowledgement so far.

    `acked` lists (gen, seq) acknowledgements in order; `bulk_acked` maps a
    generation to its read-back digest.
    """
    errors = []
    records = [tuple(r) for r in found["records"]]
    if records[:len(acked)] != acked:
        lost = next((i for i, (a, b) in enumerate(zip(records, acked)) if a != b),
                    min(len(records), len(acked)))
        errors.append(f"acknowledged record {lost} of {len(acked)} lost or reordered "
                      f"(found {len(records)} intact)")
    extra = records[len(acked):]
    last_gen, last_seq = acked[-1] if acked else (0, -1)
    # Records the store fsynced after the oracle's last acknowledgement but
    # before the crash took effect: only the continuation of the last round.
    if extra != [(last_gen, last_seq + 1 + i) for i in range(len(extra))]:
        errors.append(f"unacknowledged records {extra[:3]} do not continue the last round")
    tail = bytes.fromhex(found["tail"])
    if tail:
        want = record(last_gen, last_seq + 1 + len(extra))[:len(tail)]
        if len(tail) >= RECORD or any(b != 0 and b != w for b, w in zip(tail, want)):
            errors.append(f"ledger tail of {len(tail)} bytes is neither zeros nor the in-flight record")
    for gen, digest in bulk_acked.items():
        name = f"bulk-{gen:02}"
        if found["bulk"].get(name) != digest:
            errors.append(f"{name}: acknowledged bulk file digest {found['bulk'].get(name)} != {digest}")
    return errors


def generation_digests(records):
    digests = {}
    for gen, seq in records:
        digests.setdefault(gen, hashlib.sha256())
        digests[gen].update(record(gen, seq))
    return {f"g{gen:02}": h.hexdigest()[:16] for gen, h in sorted(digests.items())}


class Connection:
    def __init__(self):
        deadline = time.monotonic() + 60
        while True:
            try:
                self.channel = socket.create_connection(("store", PORT), timeout=10)
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


def oracle():
    from seedbox import World
    rng = random.Random(os.urandom(16))
    acked, bulk_acked, boots, rounds = [], {}, set(), []
    with World.connect() as world:
        for gen in range(ROUNDS + 1):
            connection = Connection()
            connection.send(op="recover")
            found = connection.line()
            if found["boot"] in boots:
                raise RuntimeError("store did not reboot")
            boots.add(found["boot"])
            errors = verify(found, acked, bulk_acked)
            if errors:
                for error in errors:
                    print("REBUILD_ERROR", f"boot {gen}:", error, flush=True)
                return 1
            # The in-flight record, if it survived, is now part of the ledger.
            acked = [tuple(r) for r in found["records"]]
            if gen == ROUNDS:
                rounds.append({"boot": gen, "records": len(acked)})
                break
            connection.send(op="bulk", gen=gen)
            reply = connection.line()
            if reply["digest"] != hashlib.sha256(bulk(gen)).hexdigest():
                print("REBUILD_ERROR", f"bulk {gen} read back {reply['digest']}", flush=True)
                return 1
            bulk_acked[gen] = reply["digest"]
            crash_after = rng.randint(1, 24)
            connection.send(op="append", gen=gen, first=0, count=32)
            for _ in range(crash_after):
                message = connection.line()
                acked.append((gen, message["ack"]))
            world.crash("store")
            connection.channel.close()
            world.sleep(0.2)
            world.reboot("store")
            rounds.append({"boot": gen, "crash_after": crash_after, "acked": len(acked)})
            print(f"CRASH gen={gen} after_ack={crash_after} acknowledged={len(acked)}", flush=True)
    digests = generation_digests(acked)
    print("EVIDENCE " + json.dumps({"rounds": rounds, "digests": digests}, sort_keys=True), flush=True)
    print(f"RESULT pass rounds={ROUNDS} records={len(acked)} bulk={len(bulk_acked)}", flush=True)
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
