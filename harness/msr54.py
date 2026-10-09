"""Scoped Zen counter workaround; exclusive snapshots survive failed restoration.

Only the Zen counter profile needs it (crates/seedbox/src/counter.rs).
"""
import json
import os
from pathlib import Path
import struct
import sys

MSR = 0xc0011020
BIT = 1 << 54


def required(cpuinfo="/proc/cpuinfo"):
    """False only for a host positively identified as not AMD. Unidentifiable
    hosts raise rather than silently skip the workaround."""
    fields = {}
    for line in Path(cpuinfo).read_text().splitlines():
        key, _, value = line.partition(":")
        fields.setdefault(key.strip(), value.strip())
    vendor = fields.get("vendor_id")
    if not vendor:
        raise OSError(f"{cpuinfo}: no vendor_id; cannot tell whether the Zen workaround is needed")
    return vendor == "AuthenticAMD"


def rd(cpu):
    fd = os.open(f"/dev/cpu/{cpu}/msr", os.O_RDONLY)
    try:
        return struct.unpack("<Q", os.pread(fd, 8, MSR))[0]
    finally:
        os.close(fd)


def wr(cpu, value):
    fd = os.open(f"/dev/cpu/{cpu}/msr", os.O_WRONLY)
    try:
        if os.pwrite(fd, struct.pack("<Q", value), MSR) != 8:
            raise OSError(f"cpu {cpu}: short MSR write")
    finally:
        os.close(fd)


def restore(saved, snapshot):
    errors = []
    readbacks = {}
    # Try every CPU even when one write/readback fails.
    for cpu, value in saved.items():
        try:
            wr(cpu, value)
            readbacks[cpu] = rd(cpu)
            if readbacks[cpu] != value:
                raise OSError(f"cpu {cpu}: restore readback mismatch")
        except OSError as error:
            errors.append(str(error))
    if errors:
        raise OSError("; ".join(errors))
    snapshot.unlink()
    print("restored", {cpu: hex(value) for cpu, value in readbacks.items()})


def show(cpus):
    print({cpu: hex(rd(cpu)) for cpu in cpus}, flush=True)


def set_workaround(cpus, snapshot):
    saved = {cpu: rd(cpu) for cpu in cpus}
    # Exclusive creation precedes every write; keep failed restoration evidence.
    with snapshot.open("x") as stream:
        json.dump(saved, stream)
        stream.flush()
        os.fsync(stream.fileno())
    try:
        for cpu in cpus:
            wr(cpu, saved[cpu] | BIT)
            if rd(cpu) != saved[cpu] | BIT:
                raise OSError(f"cpu {cpu}: workaround readback mismatch")
    except OSError:
        restore(saved, snapshot)
        raise
    show(cpus)


def restore_snapshot(cpus, snapshot):
    saved = {int(cpu): value for cpu, value in json.loads(snapshot.read_text()).items()}
    if set(saved) != set(cpus):
        raise ValueError("snapshot CPU set differs")
    restore(saved, snapshot)


def main():
    cmd, cpu_arg = sys.argv[1:3]
    cpus = [int(cpu) for cpu in cpu_arg.split(",")]
    if len(set(cpus)) != len(cpus):
        raise ValueError("duplicate CPU")
    # Same default as batch.sh: the checkout this script belongs to.
    root = Path(os.environ.get("SEEDBOX_ROOT", Path(__file__).resolve().parents[1]))
    snapshot = Path(os.environ.get("SEEDBOX_MSR_STATE", root / f"msr54-saved-{cpu_arg}.json"))
    if cmd == "show":
        show(cpus)
    elif cmd == "set":
        set_workaround(cpus, snapshot)
    elif cmd == "restore":
        restore_snapshot(cpus, snapshot)
    elif cmd == "clear":
        for cpu in cpus:
            wr(cpu, rd(cpu) & ~BIT)
        show(cpus)
    else:
        raise ValueError("expected show, set, restore or clear")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError) as error:
        sys.exit(f"msr54: {error}")
