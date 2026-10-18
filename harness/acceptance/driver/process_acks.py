"""Delayed process-control acknowledgements across a checkpoint and a reboot.

The supervisor confirms `stop` only after it has reaped the killed application
leader. A gatekeeper daemon (its own session, so it survives `stop`) traces
each application leader with PTRACE_SEIZE. A traced process that dies is
reported to its tracer first; its real parent, the supervisor, cannot reap it
until the tracer has. The gatekeeper observes the death without reaping it,
holds the zombie for GATE_S of guest time and only then reaps it, so every
`stop` stays admitted but unacknowledged for that long, below the 5 s process
effect deadline. Nothing else about the application is unusual.

Two oracles share the application:

* checkpoint (NEXT-100 case 90): stop the app; applications.rs captures a
  checkpoint while the acknowledgement is held and branches from it. The
  continuation (one stop effect after the gate, its receipt times, the
  gatekeeper's ledger, then a restart) must be the same in the source and in
  every branch.
* reboot (case 91): stop the app; applications.rs crashes the machine while
  the acknowledgement is held. The pending call must fail as
  target_unavailable without an effect, and the reboot must start a new
  application whose own delayed stop is confirmed once, by itself, after its
  readiness; the old incarnation's never-sent acknowledgement must not satisfy
  anything. Run on its own (no injected crash) the first stop simply
  completes; the oracle then crashes and reboots the machine itself and checks
  the same second-incarnation behaviour.
"""
import ctypes
import errno
import hashlib
import json
import os
import select
import socket
import sys
import time

APP, GATEKEEPER = 9200, 9201
GATE_S = 2.0
STATE = "/tmp/acks"

PTRACE_CONT, PTRACE_SEIZE = 7, 0x4206
WALL = 0x40000000  # __WALL: tracees are not this process's children.
CLD_EXITED, CLD_KILLED, CLD_DUMPED = 1, 2, 3


def write_file(path, text):
    with open(path + ".tmp", "w") as f:
        f.write(text)
    os.replace(path + ".tmp", path)


def boot_id():
    with open("/proc/sys/kernel/random/boot_id") as f:
        return f.read().strip()


def listener(port):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", port))
    s.listen(16)
    return s


def request(host, port, line, timeout=5.0):
    with socket.create_connection((host, port), timeout=timeout) as s:
        s.sendall(line.encode() + b"\n")
        reply = s.makefile("rb").readline()
    if not reply:
        raise RuntimeError(f"{host}:{port} closed without a reply to {line!r}")
    return json.loads(reply)


# ---------------------------------------------------------------- gatekeeper

def libc():
    library = ctypes.CDLL(None, use_errno=True)
    library.ptrace.restype = ctypes.c_long
    library.ptrace.argtypes = [ctypes.c_long, ctypes.c_long, ctypes.c_void_p, ctypes.c_void_p]
    return library


def gatekeeper(ready_fd):
    """Trace each registered leader; hold its zombie for GATE_S, then reap it."""
    c = libc()
    acceptor = listener(GATEKEEPER)
    os.write(ready_fd, b"1")
    os.close(ready_fd)
    traced = {}  # pid -> incarnation
    ledger = []
    while True:
        # Tracee stops and deaths are polled; neither wakes select.
        ready, _, _ = select.select([acceptor], [], [], 0.02)
        for pid in list(traced):
            try:
                info = os.waitid(os.P_PID, pid, os.WEXITED | os.WSTOPPED | os.WNOHANG | os.WNOWAIT | WALL)
            except ChildProcessError:
                ledger.append({"incarnation": traced.pop(pid), "error": "tracee vanished"})
                continue
            if info is None:
                continue
            if info.si_code in (CLD_EXITED, CLD_KILLED, CLD_DUMPED):
                # Dead but unreaped: the supervisor's stop is still waiting.
                seen = time.monotonic_ns()
                time.sleep(GATE_S)
                os.waitid(os.P_PID, pid, os.WEXITED | WALL)
                ledger.append({"incarnation": traced.pop(pid), "si_code": info.si_code,
                               "status": info.si_status, "held_ns": time.monotonic_ns() - seen})
            else:
                # A signal-delivery stop: pass the signal on.
                os.waitid(os.P_PID, pid, os.WSTOPPED | WALL)
                if c.ptrace(PTRACE_CONT, pid, None, ctypes.c_void_p(info.si_status)) != 0:
                    ledger.append({"incarnation": traced[pid], "error": f"cont {ctypes.get_errno()}"})
        if not ready:
            continue
        channel, _ = acceptor.accept()
        with channel:
            words = channel.makefile("rb").readline().decode().split()
            if words and words[0] == "TRACE":
                pid, incarnation = int(words[1]), int(words[2])
                if c.ptrace(PTRACE_SEIZE, pid, None, None) != 0:
                    reply = {"traced": False, "errno": errno.errorcode.get(ctypes.get_errno())}
                else:
                    traced[pid] = incarnation
                    reply = {"traced": True}
            elif words == ["LEDGER"]:
                reply = {"ledger": ledger, "tracing": sorted(traced.values()), "boot": boot_id()}
            else:
                reply = {"error": f"unknown command {words}"}
            channel.sendall(json.dumps(reply).encode() + b"\n")


def start_gatekeeper():
    """Fork the gatekeeper into its own session with no inherited outputs."""
    readiness, notify = os.pipe()
    pid = os.fork()
    if pid == 0:
        try:
            os.close(readiness)
            os.setsid()
            if os.fork() != 0:
                os._exit(0)
            devnull = os.open("/dev/null", os.O_RDWR)
            for fd in (0, 1, 2):
                os.dup2(devnull, fd)
            gatekeeper(notify)
        finally:
            os._exit(1)
    os.close(notify)
    os.waitpid(pid, 0)
    if os.read(readiness, 1) != b"1":
        raise RuntimeError("gatekeeper did not start")
    os.close(readiness)


def app():
    os.makedirs(STATE, exist_ok=True)
    path = f"{STATE}/incarnation"
    incarnation = int(open(path).read()) + 1 if os.path.exists(path) else 1
    write_file(path, str(incarnation))
    if incarnation == 1:
        start_gatekeeper()
    acceptor = listener(APP)
    trace = request("127.0.0.1", GATEKEEPER, f"TRACE {os.getpid()} {incarnation}")
    if not trace.get("traced"):
        raise RuntimeError(f"gatekeeper could not trace incarnation {incarnation}: {trace}")
    status = {"incarnation": incarnation, "boot": boot_id(), "pid": os.getpid()}
    print(f"APP_READY incarnation={incarnation}", flush=True)
    while True:
        channel, _ = acceptor.accept()
        with channel:
            channel.makefile("rb").readline()
            channel.sendall(json.dumps(status).encode() + b"\n")


# -------------------------------------------------------------------- oracle

def check(condition, message, evidence=None):
    if not condition:
        raise RuntimeError(f"{message}: {json.dumps(evidence, sort_keys=True, default=str)}")


def wait_for(predicate, what, timeout=60.0):
    deadline = time.monotonic() + timeout
    while True:
        try:
            if value := predicate():
                return value
        except (OSError, RuntimeError, ValueError):
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out waiting for {what}")
        time.sleep(0.1)


def ready(incarnation, boot=None):
    def probe():
        status = request("app", APP, "STATUS", timeout=2.0)
        if status["incarnation"] == incarnation and (boot is None or status["boot"] != boot):
            return status
        return None
    return wait_for(probe, f"app incarnation {incarnation}")


def refused():
    try:
        socket.create_connection(("app", APP), timeout=2.0).close()
    except ConnectionRefusedError:
        return True
    return False


def ledger():
    return request("app", GATEKEEPER, "LEDGER")


def receipt(effect):
    return {"requested_at_ns": effect.requested_at_ns, "admitted_at_ns": effect.admitted_at_ns,
            "effect_at_ns": effect.effect_at_ns, "replied_at_ns": effect.replied_at_ns}


def app_state(world):
    return {m["name"]: m["state"] for m in world.inspect()["machines"]}["app"]


def held_stop(world, incarnation, status, evidence, name):
    """A gated stop confirmed once, after the gatekeeper releases the zombie."""
    effect = world.stop("app")
    held = effect.effect_at_ns - effect.admitted_at_ns
    check(held >= GATE_S * 1e9 * 0.9, f"{name}: stop confirmed only after the held reap", receipt(effect))
    check(refused(), f"{name}: stopped app refuses")
    book = ledger()
    entries = [e for e in book["ledger"] if e.get("incarnation") == incarnation]
    check(len(entries) == 1 and entries[0].get("si_code") == CLD_KILLED and entries[0]["status"] == 9,
          f"{name}: one SIGKILL death held by the gatekeeper", book)
    check(abs(entries[0]["held_ns"] - GATE_S * 1e9) < 0.2e9, f"{name}: gatekeeper held the zombie", entries)
    check(book["boot"] == status["boot"], f"{name}: gatekeeper from the same boot", book)
    evidence[name] = {"receipt": receipt(effect), "ledger": book["ledger"], "state": app_state(world)}
    check(evidence[name]["state"] == "Stopped", f"{name}: inspect reports stopped", evidence[name])
    return effect


def restarted(world, incarnation, status, evidence, name):
    effect = world.restart("app")
    fresh = ready(incarnation + 1)
    check(fresh["boot"] == status["boot"] and fresh["pid"] != status["pid"],
          f"{name}: restart starts a new process in the same boot", [status, fresh])
    evidence[name] = {"receipt": receipt(effect), "state": app_state(world),
                      "tracing": ledger()["tracing"]}
    check(evidence[name]["tracing"] == [incarnation + 1], f"{name}: new leader is traced", evidence[name])
    return fresh


def finish(evidence, result):
    text = json.dumps(evidence, sort_keys=True)
    print("EVIDENCE " + text, flush=True)
    print("CONTINUATION " + hashlib.sha256(text.encode()).hexdigest(), flush=True)
    print("RESULT pass " + result, flush=True)
    return 0


def checkpoint_oracle():
    """Case 90: capture while a stop is admitted and its acknowledgement held."""
    from seedbox import World
    with World.connect() as world:
        status = ready(1)
        evidence = {"ready_state": app_state(world)}
        # The capture follows the first fault after this line (applications.rs).
        print("CAPTURE_NEXT", flush=True)
        world.now_ns()
        effect = held_stop(world, 1, status, evidence, "stop")
        # The checkpoint was taken within a second of admission, so before the
        # effect, which the gate holds back by two seconds.
        print(f"CAPTURE_PHASE at_ns={effect.admitted_at_ns}", flush=True)
        fresh = restarted(world, 1, status, evidence, "restart")
        check(evidence["restart"]["receipt"]["admitted_at_ns"] >= effect.replied_at_ns,
              "restart admitted after the stop's reply", evidence)
        # The new leader's stop is gated the same way.
        held_stop(world, 2, fresh, evidence, "second_stop")
        restarted(world, 2, fresh, evidence, "second_restart")
    return finish(evidence, "mode=checkpoint")


def reboot_oracle():
    """Case 91: a crash while a stop acknowledgement is held, then the same stop again."""
    from seedbox import World
    from seedbox.control import FaultError
    with World.connect() as world:
        status = ready(1)
        evidence = {}
        # applications.rs crashes the machine after this stop is admitted.
        print("CRASH_NEXT", flush=True)
        world.now_ns()
        try:
            effect = world.stop("app")
            mode = "uninterrupted"
            evidence["first_stop"] = {"receipt": receipt(effect), "state": app_state(world)}
            check(effect.effect_at_ns - effect.admitted_at_ns >= GATE_S * 1e9 * 0.9,
                  "uninterrupted stop waits for the held reap", evidence)
            world.crash("app")
        except FaultError as error:
            mode = "crashed"
            response = error.response
            times = {k: response[k] for k in ("requested_at_ns", "admitted_at_ns", "effect_at_ns", "replied_at_ns")}
            evidence["first_stop"] = {"code": error.code, "receipt": times, "state": app_state(world)}
            check(error.code == "target_unavailable", "pending stop fails as target_unavailable", evidence)
            # The crash came before the held acknowledgement could be sent.
            check(times["effect_at_ns"] - times["admitted_at_ns"] < GATE_S * 1e9,
                  "the crash resolved the stop before its gate", evidence)
            check(evidence["first_stop"]["state"] == "Down", "inspect reports the crash", evidence)
        reboot = world.reboot("app")
        fresh = ready(1, boot=status["boot"])
        evidence["reboot"] = {"receipt": receipt(reboot), "state": app_state(world)}
        book = ledger()
        check(book["boot"] == fresh["boot"] and book["ledger"] == [] and book["tracing"] == [1],
              "the rebooted gatekeeper starts empty", book)
        # The same operation in the new incarnation is confirmed once, by itself.
        effect = held_stop(world, 1, fresh, evidence, "second_stop")
        check(effect.admitted_at_ns >= reboot.replied_at_ns, "second stop follows the reboot", evidence)
        restarted(world, 1, fresh, evidence, "restart")
    return finish(evidence, f"mode={mode}")


def main():
    role = sys.argv[1]
    if role == "app":
        app()
    elif role == "checkpoint":
        return checkpoint_oracle()
    elif role == "reboot":
        return reboot_oracle()
    else:
        raise RuntimeError(f"unknown role {role}")


if __name__ == "__main__":
    raise SystemExit(main())
