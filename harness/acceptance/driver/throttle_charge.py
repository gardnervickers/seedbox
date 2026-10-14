"""ARM progress charges across throttle, heal, reboot and a checkpoint branch.

The worker (2 GHz, no jitter) runs pure-Python compute: no syscalls, so its
simulated time is the progress calls the translator makes every 50,000
instructions, each charged at the speed the kernel loads when the call starts.

The oracle measures compute rates per phase: before; throttled to 500 MHz
(about 4x slower; this throttle also marks the checkpoint capture point);
healed; throttled across a crash and reboot (still about 4x: reboot
reapplies an active throttle); healed again, each within 10%. It also checks the
load-once contract of `seedbox_charge`: under a 2 MHz throttle one progress
call costs 25 ms and so spans about 25 world bounds; a heal landing inside
that call must not shorten it, and the calls after it run at full speed.
"""
import hashlib
import json
import socket
import sys
import time

WORK = 400_000      # Loop iterations per rate measurement.
SPAN = 20_000       # A short burst: a few dozen progress calls at full speed.
MS = 1_000_000
THROTTLED_CALL = 25 * MS  # 50,000 instructions at 2 MHz.


def compute(count):
    x = 1
    for _ in range(count):
        x = (x * 1103515245 + 12345) & 0xFFFFFFFF
    return x


def worker():
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("0.0.0.0", 7100))
        listener.listen()
        while True:
            channel, _ = listener.accept()
            with channel, channel.makefile("rb") as reader:
                for line in reader:
                    count = int(line)
                    start = time.monotonic_ns()
                    value = compute(count)
                    elapsed = time.monotonic_ns() - start
                    channel.sendall(b"%d %d\n" % (elapsed, value))


class Worker:
    def __init__(self):
        deadline = time.monotonic() + 120
        while True:
            try:
                self.channel = socket.create_connection(("worker", 7100), 3)
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.05)
        self.reader = self.channel.makefile("rb")

    def send(self, count):
        self.channel.sendall(b"%d\n" % count)

    def receive(self):
        elapsed, value = self.reader.readline().split()
        return int(elapsed), int(value)

    def work(self, count=WORK):
        self.send(count)
        return self.receive()[0]

    def close(self):
        self.reader.close()
        self.channel.close()


def scenario():
    from seedbox import World
    worker_channel = Worker()
    with World.connect() as world:
        worker_channel.work()  # Warm the interpreter's code paths.
        before = worker_channel.work()

        # The capture follows the first fault after this line (applications.rs).
        print("CAPTURE_NEXT", flush=True)
        world.now_ns()
        # Capture point: the branch starts with this throttle active.
        slow = world.throttle("worker", "500MHz")
        print(f"CAPTURE_PHASE at_ns={slow.effect.effect_at_ns}", flush=True)
        during = worker_channel.work()
        world.heal(slow)
        after = worker_channel.work()

        # Load-once: a heal inside a 25 ms progress call leaves that call's charge.
        # The oracle times the burst: the throttled call may start before the
        # worker's own first clock read.
        crawl = world.throttle("worker", "2MHz")
        sent = time.monotonic_ns()
        worker_channel.send(SPAN)
        world.sleep(0.005)
        healed = world.heal(crawl)
        worker_channel.receive()
        span = time.monotonic_ns() - sent
        span_after = worker_channel.work(SPAN)
        assert healed.effect_at_ns > crawl.effect.effect_at_ns

        # A crash and reboot while throttled keep the throttle.
        slow = world.throttle("worker", "500MHz")
        worker_channel.close()
        world.crash("worker")
        world.reboot("worker")
        worker_channel = Worker()
        worker_channel.work()
        rebooted = worker_channel.work()
        world.heal(slow)
        recovered = worker_channel.work()
    worker_channel.close()

    result = {"before_ns": before, "during_ns": during, "after_ns": after,
              "span_ns": span, "span_after_ns": span_after,
              "rebooted_ns": rebooted, "recovered_ns": recovered}
    ratios = {name: round(result[f"{name}_ns"] / before, 3)
              for name in ("during", "after", "rebooted", "recovered")}
    print("THROTTLE_CHARGE_RESULT " + json.dumps({**result, **ratios}, sort_keys=True), flush=True)
    # Within 10% of the nominal ratios: clock reads, syscalls and scheduler
    # windows inside the measurement vary a little with the seed.
    assert 3.6 <= ratios["during"] <= 4.4, ratios
    assert 0.9 <= ratios["after"] <= 1.1, ratios
    assert 3.6 <= ratios["rebooted"] <= 4.4, ratios
    assert 0.9 <= ratios["recovered"] <= 1.1, ratios
    # One throttled call charged in full, then the burst's remaining calls at
    # full speed (span_after bounds them), plus two network hops and a few
    # throttled 1 ms syscalls before the heal. Re-reading the speed at the heal
    # would end the call about 4 ms after it started, under 25 ms in all.
    assert THROTTLED_CALL <= span <= THROTTLED_CALL + span_after + 8 * MS, result
    evidence = json.dumps({k: v for k, v in result.items() if k != "before_ns"}, sort_keys=True)
    print("CONTINUATION " + hashlib.sha256(evidence.encode()).hexdigest(), flush=True)
    print("RESULT pass", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(worker() if sys.argv[1] == "worker" else scenario())
