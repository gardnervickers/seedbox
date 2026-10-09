"""Per-machine clock drift: monotonic and realtime run at a rate error against the raw clock."""
import json
import socket
import sys
import time

WINDOW_S = 2.0
SETTLE_S = 0.1
# Rates are measured from two samples taken right after the guest kernel has
# folded elapsed time in at the current rate; allow 1% of the rate plus 20 ppm.
FLOOR_PPB = 20_000
# Realtime and monotonic drift together, so their difference holds to 1 ms.
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
                    sample = {
                        "raw_ns": time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW),
                        "monotonic_ns": time.monotonic_ns(),
                        "realtime_ns": time.time_ns(),
                    }
                    channel.sendall(json.dumps(sample).encode() + b"\n")


def scenario():
    from seedbox import FaultError, World
    names = ("a", "b")
    channels = {name: connect(name, 9000) for name in names}
    readers = {name: channel.makefile("rb") for name, channel in channels.items()}

    def sample(name):
        channels[name].sendall(b"sample\n")
        return json.loads(readers[name].readline())

    def rates():
        """Each machine's monotonic rate error against its raw clock over one window, in ppb."""
        time.sleep(SETTLE_S)
        first = {name: sample(name) for name in names}
        time.sleep(WINDOW_S)
        last = {name: sample(name) for name in names}
        result = {}
        for name in names:
            raw = last[name]["raw_ns"] - first[name]["raw_ns"]
            monotonic = last[name]["monotonic_ns"] - first[name]["monotonic_ns"]
            wall = (last[name]["realtime_ns"] - last[name]["monotonic_ns"]) - (first[name]["realtime_ns"] - first[name]["monotonic_ns"])
            assert monotonic > 0 and raw > 0, f"{name} clocks did not advance"
            assert abs(wall) <= SLACK_NS, f"{name} realtime left monotonic by {wall} ns"
            result[name] = (monotonic - raw) * 1_000_000_000 // raw
        return result

    def near(actual, expected, what):
        assert abs(actual - expected) <= FLOOR_PPB + abs(expected) // 100, f"{what}: {actual} ppb != {expected} ppb"

    with World.connect() as world:
        for name, rate in rates().items():
            near(rate, 0, f"{name} before drift")
        fast = world.clock_drift("a", ppb=50_000_000)
        seeded = world.clock_drift("b", max_ppb=10_000_000)
        assert fast.rate_ppb == 50_000_000 and abs(seeded.rate_ppb) <= 10_000_000
        measured = rates()
        near(measured["a"], fast.rate_ppb, "a at +5%")
        near(measured["b"], seeded.rate_ppb, "b at its seeded rate")
        # A machine runs at one rate; a second drift waits for the heal.
        try:
            world.clock_drift("a", ppb=-1)
            raise AssertionError("second drift on a was admitted")
        except FaultError as error:
            assert error.code == "invalid_state", error
        world.heal(fast)
        slow = world.clock_drift("a", ppb=-30_000_000)
        near(rates()["a"], slow.rate_ppb, "a at -3%")
        world.heal(slow)
        world.heal(seeded)
        for name, rate in rates().items():
            near(rate, 0, f"{name} healed")
    print(f"clock-drift: PASS a_ppb={measured['a']} b_ppb={measured['b']} b_seeded_ppb={seeded.rate_ppb}", flush=True)


if __name__ == "__main__":
    {"clock": clock, "scenario": scenario}[sys.argv[1]]()
