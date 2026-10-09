"""Boot, use, crash and boot again a standby copy of the lifecycle target."""
import json
import socket
import sys
import time
from main import exchange
from seedbox import World


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
        assert state(world, "spare") == "Down"
        assert unreachable("spare:7000")
        world.reboot("spare")
        ready("spare:7000")
        first = boot("spare")
        assert state(world, "spare") == "Running"
        world.crash("spare")
        assert state(world, "spare") == "Down"
        assert unreachable("spare:7000")
        world.reboot("spare")
        ready("spare:7000")
        second = boot("spare")
        assert first != second
        ready("server:7000")
    print("STANDBY_RESULT " + json.dumps({"boots": 2, "fresh_boot": True}), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(run())
