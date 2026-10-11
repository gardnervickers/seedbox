"""Guest-owned slow-machine throttle: CPU work slows during the window and recovers."""
import json
import os
import socket
import sys
import time

# Syscall charges reach the guest clock at exits, up to one 1 ms scheduler
# window after the work. Measure roughly 200 ms so the uncharged tails at
# either clock read cannot dominate the slowdown ratio.
SYSCALLS = 200_000


def worker():
    with socket.socket() as listener:
        listener.bind(("0.0.0.0", 7100))
        listener.listen()
        while True:
            channel, _ = listener.accept()
            with channel, channel.makefile("rb") as reader:
                for _ in reader:
                    # Each getppid is one syscall: the PV clock's unit of CPU work.
                    start = time.monotonic_ns()
                    for _ in range(SYSCALLS):
                        os.getppid()
                    channel.sendall(b"%d\n" % (time.monotonic_ns() - start))


def scenario():
    from seedbox import FaultError, World

    deadline = time.monotonic() + 10
    while True:
        try:
            channel = socket.create_connection(("worker", 7100), 3)
            break
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)
    reader = channel.makefile("rb")

    def work():
        channel.sendall(b"WORK\n")
        return int(reader.readline())

    with World.connect() as world:
        work()  # Warm the interpreter's code paths before measuring.
        before = work()
        slow = world.throttle("worker", "500MHz")  # a quarter of the 2 GHz default
        assert slow.handle in world.inspect()["active_handles"]
        try:
            world.throttle("worker", "1GHz")
        except FaultError as error:
            assert error.code == "invalid_state", error
        else:
            raise AssertionError("second throttle of one machine succeeded")
        during = work()
        world.heal(slow)
        assert slow.handle not in world.inspect()["active_handles"]
        after = work()
    channel.close()
    result = {"before_ns": before, "during_ns": during, "after_ns": after,
              "slowdown": round(during / before, 2), "recovered": round(after / before, 2)}
    print("SLOW_MACHINE_RESULT " + json.dumps(result, sort_keys=True), flush=True)
    # Clock reads and the partial scheduler windows are small at this work size.
    assert 3.5 <= during / before <= 4.05, result
    assert 0.9 <= after / before <= 1.1, result
    return 0


if __name__ == "__main__":
    sys.exit(worker() if sys.argv[1] == "worker" else scenario())
