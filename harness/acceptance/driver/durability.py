"""Durability ledgers under systematic crash placement on a retained ext4 volume.

The store machine writes tagged records through each durability primitive and
acknowledges a record only after its primitive returns. The oracle crashes the
store at every phase boundary and inside every phase, reboots it, and checks
everything any earlier generation wrote against what the disk model guarantees:

* acknowledged records are present and byte-exact (fsync, fdatasync, O_DSYNC,
  msync(MS_SYNC), rename + directory fsync, fallocate + fdatasync, truncate);
* a renamed file is always one complete version, never empty or mixed;
* unacknowledged bytes are either the expected bytes or zeros: the device may
  have flushed any part of them, but nothing is garbage, stale or reordered;
* an in-place overwrite, pushed to the device with sync_file_range() but never
  flushed, never mixes versions inside one filesystem block when unflushed
  sectors are dropped (`--policy drop`, the default disk model). The
  torn-write model (`--policy torn`, `simulator.disk.torn_writes` in a resolved
  definition) keeps each unflushed sector with probability one half: mixing is
  allowed per 512-byte sector there, and a run that never mixes fails, because
  the model would then not have been exercised.

Guest page cache is lost under both policies. sync_file_range() writes pages
without a device flush, so its records are recorded, never required. Only the
two policies above are reachable from a definition; a policy that keeps every
unflushed sector is not.
"""
import base64
import ctypes
import json
import mmap
import os
import random
import socket
import sys
import time

ROOT = "/data"
PORT = 7000
RECORD = 64
COUNT = 8           # Records per append phase.
SLOTS = 8           # msync and fallocate slots.
VERSIONS = 3        # Renamed manifest versions.
BLOCKS = 4          # Overwritten filesystem blocks.
SECTOR = 512
PHASES = ("fsync", "fdatasync", "odsync", "msync", "rename", "fallocate",
          "truncate", "overwrite", "plain", "sfr")
FILES = {"fsync": "fsync.log", "fdatasync": "fdatasync.log", "odsync": "odsync.log",
         "msync": "index.map", "rename": "manifest", "fallocate": "prealloc.dat",
         "truncate": "trunc.log", "overwrite": "over.dat", "plain": "plain.log",
         "sfr": "sfr.log"}


def record(gen, phase, index):
    """A self-describing 64-byte record; every byte depends on its tag."""
    head = f"g{gen:03} {phase:<9} i{index:04} ".encode()
    body = bytes((gen * 131 + PHASES.index(phase) * 31 + index * 7 + i) % 26 + 97
                 for i in range(RECORD - len(head) - 1))
    return head + body + b"\n"


def stream(gen, phase, count):
    return b"".join(record(gen, phase, i) for i in range(count))


def manifest(gen, version):
    return record(gen, "rename", version) * 4


def block(gen, version, index, size):
    unit = f"v{version} g{gen:03} b{index:02} ".encode()
    return (unit * (size // len(unit) + 1))[:size]


# ---------------------------------------------------------------- store side

def fsync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def create(directory, name, flags=0):
    fd = os.open(os.path.join(directory, name), os.O_RDWR | os.O_CREAT | flags, 0o644)
    fsync_dir(directory)
    return fd


def sync_file_range(fd, offset, length):
    libc = ctypes.CDLL(None, use_errno=True)
    function = libc.sync_file_range
    function.argtypes = [ctypes.c_int, ctypes.c_int64, ctypes.c_int64, ctypes.c_uint]
    # WAIT_BEFORE | WRITE | WAIT_AFTER: pages reach the device, which is not a flush.
    if function(fd, offset, length, 1 | 2 | 4) != 0:
        raise OSError(ctypes.get_errno(), "sync_file_range")


def write_all(fd, data, offset=None):
    view = memoryview(data)
    while view:
        written = os.write(fd, view) if offset is None else os.pwrite(fd, view, offset)
        view = view[written:]
        if offset is not None:
            offset += written


def run_phase(gen, phase, send, block_size):
    """Perform one phase; `send(kind, tag)` reports progress before continuing."""
    directory = os.path.join(ROOT, f"g{gen:03}")
    name = FILES[phase]
    if phase in ("fsync", "fdatasync", "plain", "sfr"):
        fd = create(directory, name, os.O_APPEND)
        for i in range(COUNT):
            write_all(fd, record(gen, phase, i))
            if phase == "fsync":
                os.fsync(fd)
            elif phase == "fdatasync":
                os.fdatasync(fd)
            elif phase == "sfr":
                sync_file_range(fd, i * RECORD, RECORD)
            send("ack" if phase in ("fsync", "fdatasync") else "note", i)
        os.close(fd)
    elif phase == "odsync":
        fd = create(directory, name, os.O_APPEND | os.O_DSYNC)
        for i in range(COUNT):
            write_all(fd, record(gen, phase, i))
            send("ack", i)
        os.close(fd)
    elif phase == "msync":
        fd = create(directory, name)
        os.ftruncate(fd, SLOTS * RECORD)
        os.fsync(fd)
        send("ack", "sized")
        with mmap.mmap(fd, SLOTS * RECORD) as index:
            for i in range(SLOTS):
                index[i * RECORD:(i + 1) * RECORD] = record(gen, phase, i)
                index.flush(0, SLOTS * RECORD)  # msync(MS_SYNC) of the whole mapping.
                send("ack", i)
        os.close(fd)
    elif phase == "rename":
        for version in range(1, VERSIONS + 1):
            fd = os.open(os.path.join(directory, "manifest.tmp"),
                         os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
            write_all(fd, manifest(gen, version))
            os.fsync(fd)
            os.close(fd)
            os.rename(os.path.join(directory, "manifest.tmp"), os.path.join(directory, name))
            fsync_dir(directory)
            send("ack", version)
    elif phase == "fallocate":
        fd = create(directory, name)
        os.posix_fallocate(fd, 0, SLOTS * RECORD)
        os.fsync(fd)
        send("ack", "sized")
        for i in range(SLOTS):
            write_all(fd, record(gen, phase, i), i * RECORD)
            os.fdatasync(fd)
            send("ack", i)
        os.close(fd)
    elif phase == "truncate":
        fd = create(directory, name, os.O_APPEND)
        write_all(fd, stream(gen, phase, 2 * COUNT))
        os.fsync(fd)
        send("ack", "full")
        os.ftruncate(fd, COUNT * RECORD)
        os.fsync(fd)
        send("ack", "cut")
        os.close(fd)
    elif phase == "overwrite":
        fd = create(directory, name)
        write_all(fd, b"".join(block(gen, 0, b, block_size) for b in range(BLOCKS)), 0)
        os.fsync(fd)
        send("ack", "v0")
        # One aligned write per block: writeback can only see a whole version.
        for b in range(BLOCKS):
            write_all(fd, block(gen, 1, b, block_size), b * block_size)
            send("note", b)
        # Put the new version in the device's volatile layer without a flush,
        # so the crash policy, not the page cache, decides what survives.
        sync_file_range(fd, 0, BLOCKS * block_size)
        send("note", "written")
        os.close(fd)
    else:
        raise RuntimeError(f"unknown phase {phase}")


def snapshot():
    """Every generation's files exactly as this boot found them."""
    found = {}
    for entry in sorted(os.listdir(ROOT)):
        path = os.path.join(ROOT, entry)
        if not (entry.startswith("g") and os.path.isdir(path)):
            continue
        files = {}
        for name in sorted(os.listdir(path)):
            with open(os.path.join(path, name), "rb") as f:
                files[name] = base64.b64encode(f.read()).decode()
        found[entry] = files
    return found


def store():
    os.makedirs(ROOT, exist_ok=True)
    recovered = snapshot()  # Before this boot writes anything.
    block_size = os.statvfs(ROOT).f_bsize
    boot = open("/proc/sys/kernel/random/boot_id").read().strip()
    acceptor = socket.socket()
    acceptor.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    acceptor.bind(("0.0.0.0", PORT))
    acceptor.listen()
    print(f"STORE_READY generations={len(recovered)} block={block_size}", flush=True)
    while True:
        channel, _ = acceptor.accept()
        with channel:
            reader = channel.makefile("rb")

            def send(kind, tag):
                channel.sendall(json.dumps({kind: tag}).encode() + b"\n")

            for line in reader:
                request = json.loads(line)
                if request["op"] == "recover":
                    channel.sendall(json.dumps({"boot": boot, "block": block_size,
                                                "files": recovered}).encode() + b"\n")
                elif request["op"] == "generation":
                    directory = os.path.join(ROOT, f"g{request['gen']:03}")
                    os.mkdir(directory)
                    fsync_dir(ROOT)
                    send("ack", "dir")
                elif request["op"] == "phase":
                    run_phase(request["gen"], request["phase"], send, block_size)
                    send("done", request["phase"])
                else:
                    raise RuntimeError(f"unknown op {request['op']}")


# --------------------------------------------------------------- oracle side

def weak(found, expected, alternatives=()):
    """Unacknowledged bytes: each is the expected byte (or an alternative) or zero."""
    if len(found) > len(expected):
        return f"{len(found)} bytes, more than the {len(expected)} ever written"
    for i, byte in enumerate(found):
        if byte != 0 and byte != expected[i] and all(byte != alt[i] for alt in alternatives):
            return f"byte {i} is {byte:#x}, neither written nor zero"
    return None


def intact_records(found, expected):
    count = 0
    while (count + 1) * RECORD <= len(found) and found[count * RECORD:(count + 1) * RECORD] \
            == expected[count * RECORD:(count + 1) * RECORD]:
        count += 1
    return count


def verify_generation(gen, files, acks, block_size, policy):
    """Errors and observations for one generation; `acks` maps phase to tags."""
    errors, seen = [], {}

    def fail(phase, message):
        errors.append(f"g{gen:03} {phase}: {message}")

    if acks.get("generation") and "dir" in acks["generation"] and files is None:
        fail("dir", "acknowledged directory is missing")
    files = files or {}
    for phase in PHASES:
        name = FILES[phase]
        tags = acks.get(phase, set())
        data = files.get(name)
        if data is None:
            if tags:
                fail(phase, f"{name} missing after acknowledgements {sorted(map(str, tags))}")
            continue
        if phase in ("fsync", "fdatasync", "odsync", "plain", "sfr"):
            expected = stream(gen, phase, COUNT)
            acked = len([t for t in tags if isinstance(t, int)])
            if data[:acked * RECORD] != expected[:acked * RECORD]:
                fail(phase, f"acknowledged records 0..{acked} lost or torn")
            elif problem := weak(data[acked * RECORD:], expected[acked * RECORD:]):
                fail(phase, f"unacknowledged tail: {problem}")
            seen[phase] = intact_records(data, expected)
        elif phase in ("msync", "fallocate"):
            expected = stream(gen, phase, SLOTS)
            if "sized" in tags and len(data) != SLOTS * RECORD:
                fail(phase, f"acknowledged size {SLOTS * RECORD} is {len(data)}")
            for i in range(SLOTS):
                part, want = data[i * RECORD:(i + 1) * RECORD], expected[i * RECORD:(i + 1) * RECORD]
                if i in tags and part != want:
                    fail(phase, f"acknowledged slot {i} lost or torn")
                elif problem := weak(part, want):
                    fail(phase, f"slot {i}: {problem}")
            seen[phase] = sum(data[i * RECORD:(i + 1) * RECORD] == expected[i * RECORD:(i + 1) * RECORD]
                              for i in range(SLOTS))
        elif phase == "rename":
            versions = {manifest(gen, v): v for v in range(1, VERSIONS + 1)}
            version = versions.get(data)
            if version is None:
                fail(phase, f"manifest is not one complete version ({len(data)} bytes)")
            elif version < max([t for t in tags if isinstance(t, int)], default=0):
                fail(phase, f"manifest version {version} older than acknowledged")
            seen[phase] = version
        elif phase == "truncate":
            full, cut = stream(gen, phase, 2 * COUNT), stream(gen, phase, COUNT)
            if "cut" in tags and data != cut:
                fail(phase, f"acknowledged truncation to {len(cut)} bytes has {len(data)}")
            elif "full" in tags and data not in (full, cut):
                fail(phase, f"acknowledged file is neither full nor truncated ({len(data)} bytes)")
            elif problem := weak(data, full):
                fail(phase, problem)
            seen[phase] = len(data) // RECORD
        elif phase == "overwrite":
            old = b"".join(block(gen, 0, b, block_size) for b in range(BLOCKS))
            new = b"".join(block(gen, 1, b, block_size) for b in range(BLOCKS))
            if "v0" not in tags:
                if problem := weak(data, old, (new,)):
                    fail(phase, problem)
                continue
            if len(data) != len(old):
                fail(phase, f"acknowledged size {len(old)} is {len(data)}")
                continue
            mixed = new_blocks = 0
            for b in range(BLOCKS):
                part = slice(b * block_size, (b + 1) * block_size)
                sectors = set()
                for s in range(part.start, part.stop, SECTOR):
                    piece = data[s:s + SECTOR]
                    if piece == old[s:s + SECTOR]:
                        sectors.add(0)
                    elif piece == new[s:s + SECTOR]:
                        sectors.add(1)
                    else:
                        fail(phase, f"block {b} sector {s // SECTOR} is neither version")
                        sectors.add(None)
                if len(sectors) > 1:
                    mixed += 1
                    if policy == "drop":
                        fail(phase, f"block {b} mixes versions although unflushed sectors are dropped")
                new_blocks += sectors == {1}
            seen[phase] = {"mixed_blocks": mixed, "new_blocks": new_blocks}
    unexpected = sorted(set(files) - set(FILES.values()) - {"manifest.tmp"})
    if unexpected:
        fail("dir", f"unexpected files {unexpected}")
    return errors, seen


def verify(recovered, ledger, policy):
    """Check one boot's snapshot against every acknowledgement so far."""
    files = {name: {k: base64.b64decode(v) for k, v in contents.items()}
             for name, contents in recovered["files"].items()}
    errors, observations = [], {}
    for name in sorted(set(files) - {f"g{gen:03}" for gen in ledger}):
        errors.append(f"{name}: directory from no generation")
    for gen, acks in sorted(ledger.items()):
        found, seen = verify_generation(gen, files.get(f"g{gen:03}"), acks,
                                        recovered["block"], policy)
        errors.extend(found)
        observations[gen] = seen
    return errors, observations


def crash_plan(rng):
    """Every phase boundary, then a crash inside every phase."""
    plan = [("boundary", k, 0) for k in range(len(PHASES) + 1)]
    plan += [("inside", k, rng.randint(0, 2)) for k in range(len(PHASES))]
    return plan


class Connection:
    def __init__(self):
        deadline = time.monotonic() + 30
        while True:
            try:
                self.channel = socket.create_connection(("store", PORT), timeout=5)
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

    def drain(self, on_line):
        """Lines the store sent before it crashed still prove their primitive returned."""
        self.channel.settimeout(0.5)
        try:
            while line := self.reader.readline():
                on_line(json.loads(line))
        except (OSError, ValueError):
            pass
        self.channel.close()


def oracle(policy):
    from seedbox import World
    rng = random.Random(os.urandom(16))
    plan = crash_plan(rng)
    ledger, boots, summary = {}, set(), []
    with World.connect() as world:
        for gen, (kind, upto, lines_before_crash) in enumerate(plan + [(None, 0, 0)]):
            connection = Connection()
            connection.send(op="recover")
            recovered = connection.line()
            if recovered["boot"] in boots:
                raise RuntimeError("store did not reboot")
            boots.add(recovered["boot"])
            errors, observations = verify(recovered, ledger, policy)
            if errors:
                for error in errors:
                    print("DURABILITY_ERROR", error, flush=True)
                raise SystemExit(1)
            if kind is None:
                summary.append(observations)
                break
            acks = ledger.setdefault(gen, {})

            def note(message, phase):
                if "ack" in message:
                    acks.setdefault(phase, set()).add(message["ack"])

            connection.send(op="generation", gen=gen)
            note(connection.line(), "generation")
            for phase in PHASES[:upto]:
                connection.send(op="phase", gen=gen, phase=phase)
                while "done" not in (message := connection.line()):
                    note(message, phase)
            if kind == "inside":
                phase = PHASES[upto]
                connection.send(op="phase", gen=gen, phase=phase)
                for _ in range(lines_before_crash):
                    message = connection.line()
                    note(message, phase)
                    if "done" in message:
                        break
            world.crash("store")
            if kind == "inside":
                connection.drain(lambda message: note(message, PHASES[upto]))
            else:
                connection.channel.close()
            world.sleep(0.2)
            world.reboot("store")
            inside = len(acks.get(PHASES[upto], ())) if kind == "inside" else "-"
            print(f"CRASH gen={gen} {kind} phase={upto} after_lines={lines_before_crash} "
                  f"inside_acks={inside} acks="
                  + json.dumps({p: len(t) for p, t in sorted(acks.items())}, sort_keys=True), flush=True)
    final = summary[-1]
    acknowledged = sum(len(tags) for acks in ledger.values() for tags in acks.values())
    mixed = sum(seen.get("overwrite", {}).get("mixed_blocks", 0) for seen in final.values())
    unsynced = {phase: sum(seen.get(phase, 0) for seen in final.values()) for phase in ("plain", "sfr")}
    print("OBSERVED " + json.dumps(final, sort_keys=True), flush=True)
    if policy == "torn" and not mixed:
        # Several crashes land after unflushed overwrites reach the device; with
        # half the sectors surviving, some block must mix versions.
        print("DURABILITY_ERROR torn-write model kept no partial block", flush=True)
        return 1
    print(f"RESULT pass policy={policy} generations={len(plan)} acknowledged={acknowledged} "
          f"mixed_blocks={mixed} unsynced_records_kept={json.dumps(unsynced, sort_keys=True)}", flush=True)
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
