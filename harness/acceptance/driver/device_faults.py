"""Disk and scheduler faults take effect in the guest and heal.

The oracle's own machine is outside the failure model, so the `disk` machine
runs each phase's work when the oracle asks (`probe`). The oracle applies the
phase's fault to it, observes the fault there, heals it and observes the same
work unfaulted. An engine that refuses a fault as unsupported has that phase
observed unfaulted. Meanwhile the oracle crashes the victim on a held FLUSH
and reboots it.
"""
import json
import re
import socket
import subprocess
import sys
import time

LATENCY_NS = 20_000_000
PORT = 9000


def faults(arguments):
    result = subprocess.run(["/faults", *arguments], capture_output=True, text=True, check=True)
    return result.stdout


def probe():
    """Run /faults for each JSON argument list the oracle sends; reply with its output."""
    with socket.socket() as listener:
        listener.bind(("0.0.0.0", PORT))
        listener.listen()
        while True:
            with listener.accept()[0] as channel:
                for line in channel.makefile("rb"):
                    output = faults(json.loads(line))
                    print(output, end="", flush=True)
                    channel.sendall(json.dumps(output).encode() + b"\n")


def connect(host):
    deadline = time.monotonic() + 30
    while True:
        channel = socket.socket()
        try:
            channel.connect((host, PORT))
            return channel
        except OSError:
            channel.close()
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def check(phase, values, faulted):
    expected = {
        "read-eio": lambda v: v["eio"] == 8 if faulted else v["ok"] == 8,
        "corrupt": lambda v: v["corrupt"] == 8 if faulted else v["ok"] == 8,
        "write-eio": lambda v: 0 < v["eio"] < 16 if faulted else v["eio"] == 0,
        "slow": lambda v: v["min_ns"] >= LATENCY_NS if faulted else v["max_ns"] < LATENCY_NS,
        "race": lambda v: v["switches"] > 1000 if faulted else True,
    }[phase]
    if not expected(values):
        raise RuntimeError(f"{phase}: unexpected observation {values} (faulted={faulted})")


def phases(disk):
    """Each phase: its name, its fault on the disk machine and the work that observes it."""
    return [
        ("read-eio", lambda w: w.disk_error(disk, "read", 1_000_000, scope="data"),
         ["read", "/data/f", "8"]),
        ("corrupt", lambda w: w.disk_error(disk, "read", 1_000_000, effect="corrupt", scope="data"),
         ["read", "/data/f", "8"]),
        ("write-eio", lambda w: w.disk_error(disk, "write", 500_000, scope="data"),
         ["write", "/data/f", "16"]),
        ("slow", lambda w: w.disk_latency(disk, "flush", LATENCY_NS), ["fsync", "/data/g", "4"]),
        ("race", lambda w: w.preempt(disk, 100_000), ["race", "20000"]),
    ]


def victim(world, trigger, machine):
    """Wait for the held FLUSH's crash, then reboot the victim."""
    if trigger is None:
        return
    while world.inspect()["triggers"][trigger.id - 1]["status"] != "fired":
        world.sleep(0.001)
    while not any(m["name"] == "victim" and m["state"] == "Down" for m in world.inspect()["machines"]):
        world.sleep(0.001)
    world.reboot(machine)
    print("VICTIM crashed on a held flush and rebooted", flush=True)


def scenario():
    from seedbox import FaultError, Machine, World

    victim_machine = Machine("victim")
    channel = connect("disk")
    replies = channel.makefile("rb")

    def run(arguments):
        channel.sendall(json.dumps(arguments).encode() + b"\n")
        output = json.loads(replies.readline())
        print(output, end="", flush=True)
        return {key: int(value) for key, value in re.findall(r"(\w+)=(\d+)", output)}

    def apply(world, phase, fault):
        """Apply the phase's fault, or return None if the engine lacks it."""
        try:
            return fault(world)
        except FaultError as error:
            # x86_64 PV has no guest yield hook, so it refuses preempt.
            if error.code != "unsupported":
                raise
            print(f"PHASE {phase} unsupported: {error}", flush=True)
            return None

    with World.connect() as world:
        try:
            trigger = world.arm_block_request_crash(victim_machine, "flush", 20, match_timeout=5)
        except FaultError as error:
            # An engine without held block requests refuses it.
            if error.code != "unsupported":
                raise
            print(f"VICTIM crash unavailable: {error}", flush=True)
            trigger = None
        run(["prepare", "/data/f"])
        faulted_phases = []
        for phase, fault, arguments in phases(Machine("disk")):
            handle = apply(world, phase, fault)
            print(f"PHASE {phase} handle={handle.handle if handle else '-'}", flush=True)
            check(phase, run(arguments), handle is not None)
            if handle is None:
                continue
            world.heal(handle)
            if handle.handle in world.inspect()["active_handles"]:
                raise RuntimeError(f"{phase}: the fault is still active after its heal")
            print(f"HEAL {phase}", flush=True)
            check(phase, run(arguments), False)
            faulted_phases.append(phase)
        victim(world, trigger, victim_machine)
    print(f"RESULT pass device faults faulted={','.join(faulted_phases) or 'none'}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(probe() if sys.argv[1:] == ["probe"] else scenario())
