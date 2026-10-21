"""Per-machine drift: all guest clocks run at a rate error against the oracle."""
import json
import socket
import sys
import time

WINDOW_S = 2.0
SETTLE_S = 0.1
# Measure a whole-machine time-base change against the unmodified oracle's
# clock, not the target's raw clock (which drifts with the rest of the machine).
# Allow 1% of the rate plus 20 ppm, as for the original rate assertions.
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
        before = time.monotonic_ns()
        channels[name].sendall(b"sample\n")
        line = readers[name].readline()
        after = time.monotonic_ns()
        # Decoding charges simulated CPU time and its cost need not be constant.
        # It is not part of the network clock exchange.
        result = json.loads(line)
        # Symmetric fixed-latency links put the target sample near the middle
        # of this exchange. Constant transport/clock-read costs cancel between
        # samples; this clock is simulated oracle time, never host wall time.
        result["oracle_ns"] = (before + after) // 2
        return result

    def rates():
        """Each machine's clock rate error against the oracle over one window, in ppb."""
        time.sleep(SETTLE_S)
        first = {name: sample(name) for name in names}
        time.sleep(WINDOW_S)
        last = {name: sample(name) for name in names}
        result = {}
        for name in names:
            raw = last[name]["raw_ns"] - first[name]["raw_ns"]
            monotonic = last[name]["monotonic_ns"] - first[name]["monotonic_ns"]
            wall = (last[name]["realtime_ns"] - last[name]["monotonic_ns"]) - (first[name]["realtime_ns"] - first[name]["monotonic_ns"])
            reference = last[name]["oracle_ns"] - first[name]["oracle_ns"]
            assert monotonic > 0 and raw > 0 and reference > 0, f"{name} clocks did not advance"
            assert abs(wall) <= SLACK_NS, f"{name} realtime left monotonic by {wall} ns"
            # The Driver scales the entire machine, including MONOTONIC_RAW.
            # Both target clocks must preserve their common rate as well as
            # independently showing the requested drift against the oracle.
            near((monotonic - raw) * 1_000_000_000 // raw, 0, f"{name} monotonic vs raw")
            result[name] = (monotonic - reference) * 1_000_000_000 // reference
            near((raw - reference) * 1_000_000_000 // reference, result[name], f"{name} raw vs oracle")
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
