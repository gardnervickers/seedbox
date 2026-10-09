"""Per-machine wall-clock skew seen by running processes, with monotonic time intact."""
import json
import socket
import sys
import time

HOUR_NS = 3_600_000_000_000
MINUTE_NS = 60_000_000_000
# Realtime and monotonic advance together, so their difference moves only by
# the step. The two reads are not one instant; allow a little slack.
SLACK_NS = 1_000_000


def connect(host, port):
    deadline = time.monotonic() + 10
    while True:
        channel = socket.socket()
        channel.settimeout(5)
        try:
            channel.connect((host, port))
            return channel
        except OSError:
            channel.close()
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def clock():
    """Report this machine's clocks from one long-running process."""
    with socket.socket() as listener:
        listener.bind(("0.0.0.0", 9000))
        listener.listen()
        while True:
            with listener.accept()[0] as channel:
                for _ in channel.makefile("rb"):
                    sample = {"realtime_ns": time.time_ns(), "monotonic_ns": time.monotonic_ns()}
                    channel.sendall(json.dumps(sample).encode() + b"\n")


def scenario():
    from seedbox import World
    channels = {name: connect(name, 9000) for name in ("a", "b")}
    readers = {name: channel.makefile("rb") for name, channel in channels.items()}
    previous = {}

    def offset(name):
        """Realtime minus monotonic on that machine; monotonic must only move forward."""
        channels[name].sendall(b"sample\n")
        sample = json.loads(readers[name].readline())
        assert sample["monotonic_ns"] > previous.get(name, 0), f"{name} monotonic moved backwards"
        previous[name] = sample["monotonic_ns"]
        return sample["realtime_ns"] - sample["monotonic_ns"]

    def near(actual, expected, what):
        assert abs(actual - expected) <= SLACK_NS, f"{what}: {actual} != {expected}"

    with World.connect() as world:
        base = {name: offset(name) for name in ("a", "b")}
        forward = world.clock_skew("a", offset=HOUR_NS)
        seeded = world.clock_skew("b", max_offset=MINUTE_NS)
        assert forward.offset_ns == HOUR_NS and abs(seeded.offset_ns) <= MINUTE_NS
        near(offset("a") - base["a"], HOUR_NS, "a after +1h")
        near(offset("b") - base["b"], seeded.offset_ns, "b after seeded skew")
        # A negative step on top of an active skew is independent and healable.
        back = world.clock_skew("a", offset=-2 * HOUR_NS)
        near(offset("a") - base["a"], -HOUR_NS, "a after -2h")
        world.heal(forward)
        near(offset("a") - base["a"], -2 * HOUR_NS, "a after healing +1h")
        world.heal(back)
        world.heal(seeded)
        near(offset("a") - base["a"], 0, "a healed")
        near(offset("b") - base["b"], 0, "b healed")
    print(f"clock-skew: PASS b_offset_ns={seeded.offset_ns}", flush=True)


if __name__ == "__main__":
    {"clock": clock, "scenario": scenario}[sys.argv[1]]()
