"""The application leader exits 0 while an orphaned grandchild keeps writing.

The app machine's main process double-forks a writer that inherits stdout and
prints one numbered line every 100 ms for 10 s, prints its own lines, waits one
second and exits 0. The supervisor then drains only the output already queued
and reports the exit; it does not wait for inherited writers, so the machine is
reported exited at the leader's exit and the writer's later lines are never
logged. The oracle waits until inspect reports the app exited, waits two more
seconds and exits, which ends the simulation well before the writer would have
finished.

The guest-visible part is checked here; crates/seedbox/tests/support/derived.rs checks what `logs`
retained, when the machine was reported exited, and when the simulation ended.
"""
import json
import os
import sys
import time

WRITER_LINES = 100
WRITER_PERIOD = 0.1
LEADER_LINES = 5


def app():
    if os.fork() == 0:
        os.setsid()
        if os.fork() == 0:
            # The grandchild: adopted by PID 1 once its parent exits.
            for n in range(WRITER_LINES):
                os.write(1, f"ORPHAN {n:03} ppid={os.getppid()}\n".encode())
                time.sleep(WRITER_PERIOD)
            os._exit(0)
        os._exit(0)
    os.wait()
    for n in range(LEADER_LINES):
        os.write(1, f"LEADER {n}\n".encode())
    time.sleep(1.0)
    os.write(1, b"LEADER_EXIT\n")
    return 0


def oracle():
    from seedbox import World
    with World.connect() as world:
        deadline = world.now_ns() + 30_000_000_000
        while True:
            state = world.inspect()
            machine = next(m for m in state["machines"] if m["name"] == "app")
            if machine["state"].startswith("Exited("):
                break
            if world.now_ns() > deadline:
                print("LEADER_ERROR app was never reported exited", flush=True)
                return 1
            world.sleep(0.05)
        exited_seen_ns = world.now_ns()
        world.sleep(2)
        end_ns = world.now_ns()
    print("EXITED " + json.dumps({"state": machine["state"], "seen_ns": exited_seen_ns,
                                  "oracle_end_ns": end_ns}, sort_keys=True), flush=True)
    print("RESULT pass", flush=True)
    return 0


if __name__ == "__main__":
    role = sys.argv[1]
    raise SystemExit(app() if role == "app" else oracle())
