"""Disk and scheduler faults take effect in the guest and heal.

The oracle applies each phase's fault to its own machine, observes it from
inside the guest, heals it and observes the same work unfaulted. An engine
that refuses a fault as unsupported has that phase observed unfaulted.
Meanwhile the oracle crashes the victim on a held FLUSH and reboots it.
"""
import re
import subprocess

from seedbox import FaultError, Machine, World

LATENCY_NS = 20_000_000
ORACLE = Machine("oracle")
VICTIM = Machine("victim")
# Each phase: its name, its fault and the work that observes it.
PHASES = [
    ("read-eio", lambda w: w.disk_error(ORACLE, "read", 1_000_000, scope="data"),
     ["read", "/data/f", "8"]),
    ("corrupt", lambda w: w.disk_error(ORACLE, "read", 1_000_000, effect="corrupt", scope="data"),
     ["read", "/data/f", "8"]),
    ("write-eio", lambda w: w.disk_error(ORACLE, "write", 500_000, scope="data"),
     ["write", "/data/f", "16"]),
    ("slow", lambda w: w.disk_latency(ORACLE, "flush", LATENCY_NS), ["fsync", "/data/g", "4"]),
    ("race", lambda w: w.preempt(ORACLE, 100_000), ["race", "20000"]),
]


def run(arguments):
    result = subprocess.run(["/faults", *arguments], capture_output=True, text=True, check=True)
    print(result.stdout, end="", flush=True)
    return {key: int(value) for key, value in re.findall(r"(\w+)=(\d+)", result.stdout)}


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
        for phase, fault, arguments in PHASES:
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
        victim(world, trigger)
    print(f"RESULT pass device faults faulted={','.join(faulted_phases) or 'none'}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
