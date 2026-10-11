"""Spawn a machine from the prepared server and check its new address."""
import socket
import time

from seedbox import World


def identity(host):
    deadline = time.monotonic() + 10
    while True:
        try:
            with socket.create_connection((host, 7000), timeout=1) as connection:
                connection.settimeout(1)
                with connection.makefile("r") as reply:
                    return reply.readline().strip()
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def main():
    with World.connect() as world:
        assert identity("server") == "server primary"
        spawned = world.spawn("server", name="replica", env={"ROLE": "replica"})
        assert spawned.machine == "replica"
        # A spawned name is not added to existing guests' DNS; use its address.
        assert identity(spawned.address) == "replica replica"
        assert identity("server") == "server primary"
        print(f"SPAWN_OK machine={spawned.machine} address={spawned.address}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
