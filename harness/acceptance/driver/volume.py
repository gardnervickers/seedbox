"""Acknowledged fsynced TCP writes survive a machine crash on a named volume."""
import time

from main import exchange
from seedbox import World

ENDPOINT = "server:7000"


def ready():
    deadline = time.monotonic() + 20
    while True:
        try:
            if exchange(ENDPOINT, "PING", timeout=2).startswith("PONG "):
                return
        except OSError:
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError("key/value server readiness timed out")
        time.sleep(0.1)


def main():
    ready()
    expected = {f"k/{index:04}": f"v{index}" for index in range(20)}
    for key, value in expected.items():
        response = exchange(ENDPOINT, f"PUT {key} {value}", timeout=2)
        if not response.startswith("OK "):
            raise RuntimeError(f"write not acknowledged: {response}")
    with World.connect() as world:
        world.crash("server")
        world.sleep(0.5)
        world.reboot("server")
    ready()
    # Read all acknowledged values from the restarted server, not its old RAM.
    for key, value in expected.items():
        response = exchange(ENDPOINT, f"GET {key}", timeout=2).split()
        if len(response) != 3 or response[0] != "VALUE" or response[2] != value:
            raise RuntimeError(f"acknowledged value lost or changed: {key}: {response}")
    print(f"RESULT pass acknowledged_and_recovered={len(expected)}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
