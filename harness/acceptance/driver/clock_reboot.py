"""Wall time follows world time when a machine boots a fresh kernel."""
import json
import time

from clock_skew import connect
from seedbox import World

# Bracket each remote sample with oracle reads. Allow clock-set syscall skew,
# but much less than the three-second delay that exposes a reset to the epoch.
SLACK_NS = 10_000_000


def sample():
    with connect("a", 9000) as channel:
        before = time.time_ns()
        channel.sendall(b"sample\n")
        with channel.makefile("rb") as reader:
            remote = json.loads(reader.readline())["realtime_ns"]
        after = time.time_ns()
    return before, remote, after


def main():
    first = sample()
    with World.connect() as world:
        world.sleep(3)
        world.crash("a")
        world.sleep(0.5)
        world.reboot("a")
    second = sample()
    elapsed = second[1] - first[1]
    low = second[0] - first[2] - SLACK_NS
    high = second[2] - first[0] + SLACK_NS
    assert elapsed > 0, f"wall time went backward after reboot: {first} -> {second}"
    assert low <= elapsed <= high, (
        f"reboot wall elapsed {elapsed} outside oracle interval [{low}, {high}]"
    )
    print(f"clock-reboot: PASS elapsed_ns={elapsed} oracle_interval_ns={low}..{high}", flush=True)


if __name__ == "__main__":
    main()
