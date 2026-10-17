"""The guest kernel's out-of-memory killer kills the app machine's application.

The app fills memory in 8 MiB steps on a 384 MiB machine until the guest
kernel kills it. The oracle waits until inspect reports the app exited with
SIGKILL's status, 137, and passes. applications.rs checks that events,
`get world` and `diagnostics` name the kill and the process.
"""
import os
import sys

STEP = 8 << 20


def app():
    held = []
    while True:
        # Bytes multiplication writes every page, so the memory is resident.
        held.append(b"\x01" * STEP)
        os.write(1, f"HELD {len(held) * STEP >> 20} MiB\n".encode())


def oracle():
    from seedbox import World
    with World.connect() as world:
        deadline = world.now_ns() + 120_000_000_000
        while True:
            state = world.inspect()
            machine = next(m for m in state["machines"] if m["name"] == "app")
            if machine["state"].startswith("Exited("):
                break
            if world.now_ns() > deadline:
                print("OOM_ERROR app was never reported exited", flush=True)
                return 1
            world.sleep(0.1)
    print("EXITED " + machine["state"], flush=True)
    if machine["state"] != "Exited(137)":
        print("RESULT fail", flush=True)
        return 1
    print("RESULT pass", flush=True)
    return 0


if __name__ == "__main__":
    role = sys.argv[1]
    raise SystemExit(app() if role == "app" else oracle())
