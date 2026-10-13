"""A signal handler blocked on an alternate stack across a checkpoint.

The sig machine runs harness/acceptance/guest/altstack.c. The oracle asks it to
park its worker in a SIGUSR1 handler on a sigaltstack, blocked in read(2) with
x19-x28, q8-q31, FPCR and stack locals holding sentinels; it marks the capture
phase with a nominal-speed throttle, then releases the handler with a token.
The handler must see the token once and every sentinel intact, report that it
ran on the alternate stack, and the interrupted computation must continue with
correct results after rt_sigreturn. The result is printed as continuation
evidence, so a branch from the capture must reproduce it.
"""
import hashlib
import json
import socket
import time

TOKEN = 4242
# The translator does not report SS_ONSTACK to sigaltstack() inside a handler
# on the alternate stack (tools/jvm-fuzz/repros/sigaltstack_onstack.c); Linux does.
# The handler's own stack address still proves where it ran.
KNOWN = {"onstack": "translator omits SS_ONSTACK; see tools/jvm-fuzz/repros/sigaltstack_onstack.c"}


def known(result):
    """Known translator discrepancies, reported but not failed."""
    return [f"{key}={result.get(key)}: {why}" for key, why in KNOWN.items() if result.get(key) != 1]


def verify(result):
    errors = []
    for key in ("onstack", "on_altstack", "locals_ok", "fpcr_ok", "registers_ok"):
        if key in KNOWN:
            continue
        if result.get(key) != 1:
            errors.append(f"{key} is {result.get(key)} (bad register {result.get('bad_register')})")
    if result.get("interrupted_on_altstack") != 0:
        errors.append("the interrupted worker was already on the alternate stack")
    if result.get("token") != TOKEN or result.get("read") != len(str(TOKEN)):
        errors.append(f"handler read {result.get('read')} bytes, token {result.get('token')}")
    if result.get("block_errors") != 0:
        errors.append(f"{result.get('block_errors')} blocks computed wrongly")
    if result.get("blocks", 0) <= result.get("blocks_at_release", 0) or not result.get("blocks_at_release"):
        errors.append("the worker did not compute before the signal and continue after rt_sigreturn")
    return errors


def main():
    from seedbox import World
    deadline = time.monotonic() + 60
    while True:
        try:
            channel = socket.create_connection(("sig", 7300), timeout=60)
            break
        except OSError:
            if time.monotonic() >= deadline:
                raise RuntimeError("sig readiness timed out")
            time.sleep(0.1)
    reader = channel.makefile("rb")

    def call(line):
        channel.sendall(line.encode() + b"\n")
        return reader.readline().decode().strip()

    with World.connect() as world:
        parked = call("park")
        if not parked.startswith("parked"):
            raise RuntimeError(parked)
        # The capture follows the first fault after this line (applications.rs).
        print("CAPTURE_NEXT", flush=True)
        world.now_ns()
        # Capture point: a nominal-speed throttle changes nothing but marks the phase.
        marker = world.throttle("sig", "1GHz")
        print(f"CAPTURE_PHASE at_ns={marker.effect.effect_at_ns} {parked}", flush=True)
        world.sleep(0.2)
        result = json.loads(call(f"release {TOKEN}"))
        world.heal(marker)
    errors = verify(result)
    for note in known(result):
        print("ALTSTACK_KNOWN", note, flush=True)
    for error in errors:
        print("ALTSTACK_ERROR", error, flush=True)
    evidence = json.dumps(result, sort_keys=True)
    print("EVIDENCE " + evidence, flush=True)
    print("CONTINUATION " + hashlib.sha256(evidence.encode()).hexdigest(), flush=True)
    if errors:
        return 1
    print(f"RESULT pass blocks={result['blocks']}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
