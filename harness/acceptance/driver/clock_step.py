"""Wall-clock steps seen by timer and sleep consumers (cfztime.c).

The oracle drives three phases on the `clock` machine, each with waiters armed
DEADLINE ahead of the arm time, then heals that phase's skew:

* past: +1 hour, beyond every REALTIME deadline;
* short: +SHORT, short of them;
* back: -1 hour.

Expected: the cancel-on-set REALTIME timerfd returns ECANCELED at every
discontinuity (each step and each heal); absolute REALTIME clock_nanosleep,
timer_settime(TIMER_ABSTIME) and an absolute REALTIME futex wait (glibc's
pthread_cond_timedwait) wake only once the wall clock has passed their deadline, never at a backward step; MONOTONIC
absolute and relative waits finish at the arm time plus DEADLINE in every
phase. Times are the guest's CLOCK_MONOTONIC; each wakeup also carries
CLOCK_MONOTONIC_RAW brackets, which must contain it.

The watcher's discontinuity times are when its thread ran after the step, not
the step itself; see at_step().
"""
import json
import socket
import sys
import time

HOUR = 3_600_000_000_000
MS = 1_000_000
DEADLINE = 4_000 * MS
SHORT = 2_000 * MS
SLACK = 100 * MS  # Wakeup latency allowed after the moment a wait may end.
ECANCELED = 125
REALTIME = ("nanosleep_real_abs", "timer_real_abs", "futex_real_abs")
UNMOVED = ("nanosleep_mono_abs", "futex_mono_abs", "nanosleep_rel", "nanosleep_real_rel")


def connect(host, port):
    deadline = time.monotonic() + 60
    while True:
        channel = socket.socket()
        try:
            channel.connect((host, port))
            return channel
        except OSError:
            channel.close()
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


class Probe:
    def __init__(self):
        self.channel = connect("clock", 9100)
        self.reader = self.channel.makefile("rb")

    def call(self, command, phase, ms):
        self.channel.sendall(f"{command} {phase} {ms}\n".encode())
        line = self.reader.readline()
        if not line:
            raise RuntimeError(f"{command} {phase}: probe closed the connection")
        reply = json.loads(line)
        if reply["kind"] == "unfinished":
            raise AssertionError(f"{phase}: waiters never finished: {reply}")
        return reply


def within(value, low, high, what):
    assert low <= value <= high, f"{what}: {value} not in [{low}, {high}]"


def at_step(value, step, before, what):
    """Check a wakeup caused by the discontinuity the watcher recorded at `step`.

    The kernel applies a step at one unobserved instant, then wakes every
    cancel-on-set timerfd, the watcher's included, and retriggers REALTIME
    timers; each woken thread reads CLOCK_MONOTONIC when it runs. The watcher's
    time is therefore only an upper bound on the step, and another wakeup may
    precede it. Every such wakeup lies within SLACK after the step, so within
    SLACK of the watcher either way. `before` is a guest time from a reply the
    oracle received before requesting the step, which no wakeup may precede.
    """
    within(value, max(before, step - SLACK), step + SLACK, what)


def check_phase(name, final, before, steps_before, expect_steps):
    """Common checks; returns the discontinuities that happened in this phase.

    `before` precedes the phase's first step (the armed reply's time).
    """
    arm, waiters = final["arm_mono"], final["waiters"]
    steps = final["steps"][steps_before:]
    assert len(steps) == expect_steps, f"{name}: discontinuities {steps} (expected {expect_steps})"
    assert steps[0] > before, f"{name}: discontinuity at {steps[0]} before the step was requested at {before}"
    for label, w in waiters.items():
        assert w["done"], f"{name}: {label} unfinished"
        assert w["raw_lo"] <= w["raw_hi"], f"{name}: {label} raw bracket inverted"
        if label != "timerfd_cancel":
            assert w["rc"] == 0, f"{name}: {label} returned {w['rc']}"
    timerfd = waiters["timerfd_cancel"]
    assert timerfd["rc"] == -ECANCELED, f"{name}: timerfd returned {timerfd['rc']}, not ECANCELED"
    # The phase timerfd and the watcher saw the same first discontinuity.
    at_step(timerfd["mono"], steps[0], before, f"{name}: timerfd cancel vs watcher")
    for label in UNMOVED:
        within(waiters[label]["mono"], arm + DEADLINE, arm + DEADLINE + SLACK, f"{name}: {label}")
    return steps


def scenario():
    from seedbox import World
    probe = Probe()
    ledger = {}
    with World.connect() as world:
        steps = 0

        # none: without a step every wait ends at its deadline and the timerfd expires.
        armed = probe.call("arm", "none", DEADLINE // MS)
        final = probe.call("finish", "none", 2 * DEADLINE // MS)
        assert final["steps"] == [], f"none: discontinuities {final['steps']}"
        for label, w in final["waiters"].items():
            assert w["rc"] == 0, f"none: {label} returned {w['rc']}"
            within(w["mono"], armed["arm_mono"] + DEADLINE, armed["arm_mono"] + DEADLINE + SLACK, f"none: {label}")
        ledger["none"] = final

        # past: a forward step beyond every REALTIME deadline wakes them at the step.
        armed = probe.call("arm", "past", DEADLINE // MS)
        skew = world.clock_skew("clock", offset=HOUR)
        final = probe.call("finish", "past", 2 * DEADLINE // MS)
        seen = check_phase("past", final, armed["now"], steps, 1)
        for label in REALTIME:
            at_step(final["waiters"][label]["mono"], seen[0], armed["now"], f"past: {label}")
            assert final["waiters"][label]["mono"] < armed["arm_mono"] + DEADLINE, f"past: {label} waited for monotonic"
        ledger["past"] = final
        world.heal(skew)
        steps += 2

        # short: a forward step short of the deadline moves REALTIME waits earlier by the step.
        armed = probe.call("arm", "short", DEADLINE // MS)
        skew = world.clock_skew("clock", offset=SHORT)
        final = probe.call("finish", "short", 2 * DEADLINE // MS)
        seen = check_phase("short", final, armed["now"], steps, 1)
        due = armed["arm_mono"] + DEADLINE - SHORT
        assert seen[0] < due, f"short: step at {seen[0]} after the moved deadline {due}"
        for label in REALTIME:
            within(final["waiters"][label]["mono"], due, due + SLACK, f"short: {label}")
            assert final["waiters"][label]["mono"] > seen[0] + SLACK, f"short: {label} woke at the step"
        ledger["short"] = final
        world.heal(skew)
        steps += 2

        # back: a backward step never wakes REALTIME waits; healing it does, at the heal.
        armed = probe.call("arm", "back", DEADLINE // MS)
        skew = world.clock_skew("clock", offset=-HOUR)
        report = probe.call("report", "back", DEADLINE // MS + 1_000)
        for label in REALTIME:
            assert not report["waiters"][label]["done"], f"back: {label} woke after a backward step"
        for label in UNMOVED:
            assert report["waiters"][label]["done"], f"back: {label} still waiting"
        world.heal(skew)
        final = probe.call("finish", "back", 2 * DEADLINE // MS)
        seen = check_phase("back", final, armed["now"], steps, 2)
        assert seen[1] > report["now"], f"back: heal seen at {seen[1]} before it was requested at {report['now']}"
        for label in REALTIME:
            at_step(final["waiters"][label]["mono"], seen[1], report["now"], f"back: {label} after heal")
        ledger["back"] = final
        steps += 2
        assert len(final["steps"]) == steps, f"discontinuities {final['steps']}"

    summary = {phase: {label: w["mono"] - r["arm_mono"] for label, w in r["waiters"].items()}
               for phase, r in ledger.items()}
    print("CLOCK_STEP_RESULT", json.dumps(summary, sort_keys=True), flush=True)
    print("clock-step: PASS", flush=True)


if __name__ == "__main__":
    {"scenario": scenario}[sys.argv[1]]()
