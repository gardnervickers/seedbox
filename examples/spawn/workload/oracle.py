"""Spawn a replica mid-run and check that it serves alongside the server."""
import socket
import time

from seedbox import FaultError, World

REPLICA = {"build": {"context": "./workload"}, "environment": {"ROLE": "replica"}}


def identity(host, timeout=10):
    deadline = time.monotonic() + timeout
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


def machines(world):
    return {machine["name"] for machine in world.inspect()["machines"]}


def main():
    with World.connect() as world:
        assert identity("server") == "server primary"
        assert "replica" not in machines(world)
        world.spawn("replica", REPLICA)
        assert "replica" in machines(world)
        try:
            world.spawn("replica", REPLICA)
        except FaultError:
            pass
        else:
            raise AssertionError("a machine name was spawnable twice")
        assert identity("replica") == "replica replica"
        assert identity("server") == "server primary"
        print("SPAWN_OK", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
