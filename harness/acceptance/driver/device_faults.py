"""Disk and scheduler faults take effect in the guest and heal.

Without a program this is the unfaulted baseline. The acceptance test adds a
program that applies each phase's fault at the start of its window and heals
it at the end. The oracle runs each phase inside its window and observes the
fault from inside the guest; a phase whose fault is not active is observed
unfaulted. Meanwhile the oracle crashes the victim on a held FLUSH and
reboots it.
"""
import re
import subprocess

from seedbox import FaultError, Machine, World

PHASES = [
    ("read-eio", ["read", "/data/f", "8"]),
    ("corrupt", ["read", "/data/f", "8"]),
    ("write-eio", ["write", "/data/f", "16"]),
    ("slow", ["fsync", "/data/g", "4"]),
    ("race", ["race", "20000"]),
]
LATENCY_NS = 20_000_000
# The program's schedule: phase i's fault is active from START_NS + i *
# PERIOD_NS for ACTIVE_NS. crates/seedbox/tests/applications.rs uses the same.
START_NS = 1_000_000_000
PERIOD_NS = 1_250_000_000
ACTIVE_NS = 1_000_000_000
# Admission and settlement land well within this after a window edge.
SETTLE_NS = 10_000_000
VICTIM = Machine("victim")


def run(arguments):
    result = subprocess.run(["/faults", *arguments], capture_output=True, text=True, check=True)
    print(result.stdout, end="", flush=True)
    return {key: int(value) for key, value in re.findall(r"(\w+)=(\d+)", result.stdout)}


def sleep_until(world, at_ns):
    now = world.now_ns()
    if now < at_ns:
        world.sleep((at_ns - now) / 1e9)


def active(world, handle):
    return handle in world.inspect()["active_handles"]


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


def victim(world, trigger):
    """Wait for the held FLUSH's crash, then reboot the victim."""
    if trigger is None:
        return
    while world.inspect()["triggers"][trigger.id - 1]["status"] != "fired":
        world.sleep(0.001)
    while not any(m["name"] == "victim" and m["state"] == "Down" for m in world.inspect()["machines"]):
        world.sleep(0.001)
    world.reboot(VICTIM)
    print("VICTIM crashed on a held flush and rebooted", flush=True)


def main():
    with World.connect() as world:
        try:
            trigger = world.arm_block_request_crash(VICTIM, "flush", 20, match_timeout=5)
        except FaultError as error:
            # An engine without held block requests refuses it.
            if error.code != "unsupported":
                raise
            print(f"VICTIM crash unavailable: {error}", flush=True)
            trigger = None
        run(["prepare", "/data/f"])
        faulted_phases = []
        for index, (phase, arguments) in enumerate(PHASES):
            start = START_NS + index * PERIOD_NS
            end = start + ACTIVE_NS
            print(f"PHASE {phase}", flush=True)
            sleep_until(world, start + SETTLE_NS)
            # A program may leave out a phase its engine cannot fault (x86_64
            # PV has no guest yield hook); that phase is observed unfaulted.
            faulted = active(world, phase)
            check(phase, run(arguments), faulted)
            if faulted:
                if world.now_ns() >= end:
                    raise RuntimeError(f"{phase}: the phase overran its fault window")
                faulted_phases.append(phase)
            print(f"HEAL {phase}", flush=True)
            sleep_until(world, end + SETTLE_NS)
            if active(world, phase):
                raise RuntimeError(f"{phase}: the fault is still active after its heal")
        victim(world, trigger)
    print(f"RESULT pass device faults faulted={','.join(faulted_phases) or 'none'}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
