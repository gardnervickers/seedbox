"""Spawn copies of the lifecycle target, reach them by address, crash and reboot one."""
import json
import socket
import sys
import time
from main import exchange
from seedbox import Machine, World


def ready(address):
    deadline = time.monotonic() + 10
    while True:
        try:
            assert exchange(address, "PING") == "READY socket_absent"
            return
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.02)


def boot(address):
    reply = exchange(address, "BOOT").split()
    assert reply[0] == "BOOT", reply
    return reply[1]


def state(world, name):
    return next(m["state"] for m in world.inspect()["machines"] if m["name"] == name)


def unreachable(address):
    """A down machine neither answers nor refuses: its frames are dropped."""
    try:
        exchange(address, "PING", 0.5)
    except (socket.timeout, OSError):
        return True
    raise AssertionError(f"{address} answered while down")


def run():
    with World.connect() as world:
        ready("server:7000")
        assert exchange("server:7000", "ROLE") == "ROLE none server", exchange("server:7000", "ROLE")
        joiner = world.spawn("server", name="joiner", env={"ROLE": "joiner"})
        print(f"SPAWN_FIRST machine={joiner.machine} address={joiner.address} "
              f"effect_at_ns={joiner.effect.effect_at_ns}", flush=True)
        at = f"{joiner.address}:7000"
        ready(at)
        assert exchange(at, "ROLE") == "ROLE joiner server"
        assert state(world, "joiner") == "Running"
        assert boot(at) != boot("server:7000")
        default = world.spawn("server", args=["python3", "/workload/control_target.py", "replaced"])
        assert default.machine == "server-2", default
        ready(f"{default.address}:7000")
        assert exchange(f"{default.address}:7000", "ROLE") == "ROLE none replaced"
        first = boot(at)
        world.crash(Machine("joiner"))
        assert state(world, "joiner") == "Down"
        assert unreachable(at)
        world.reboot(Machine("joiner"))
        ready(at)
        assert exchange(at, "ROLE") == "ROLE joiner server"
        assert boot(at) != first
        ready("server:7000")
    print("SPAWN_RESULT " + json.dumps({"spawned": [joiner.machine, default.machine],
                                        "addresses": [joiner.address, default.address],
                                        "rebooted": True}), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(run())
