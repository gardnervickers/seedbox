"""Spawn the standby replica and check that it serves alongside the server."""
import socket
import time

from seedbox import World


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


def main():
    with World.connect() as world:
        assert identity("server") == "server primary"
        try:
            identity("replica", timeout=0.5)
        except OSError:
            pass
        else:
            raise AssertionError("standby replica answered before it was spawned")
        world.spawn("replica")
        assert identity("replica") == "replica replica"
        assert identity("server") == "server primary"
        print("SPAWN_OK", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
