"""Process groups and sessions under stop, pause_process, restart and crash.

The application leader spawns three descendants, each a tagged listener with a
counter that advances while it runs and is saved under /tmp per incarnation:

* member (port 9101): an ordinary child, in the application's process group;
* orphan (port 9103): double-forked, adopted by PID 1, still in the group;
* session (port 9102): setsid() daemon with its stdio on /dev/null. It has
  left the group, so the supervisor's group signals do not reach it.

Every child closes the descriptors it inherited (the leader's listener and
the output pipes), so only its own port stays bound, except that the session
daemon keeps one leaked leader listener (port 9104): it holds that port open
after the group dies, and the next incarnation cannot bind it.

Linux semantics checked: pause_process freezes the group (state T, counters
stop, connections queue and are answered once after resume_process); stop
kills and reaps the group (no zombies; ports refuse), also when it is frozen,
while the session daemon survives; restart starts a new incarnation that
rebinds its own ports, but the survivor keeps port 9102 bound until it exits;
a crash ends every process, and the rebooted machine starts from incarnation
1 with every port free.
"""
import errno
import json
import os
import select
import socket
import sys
import time

TICK = 0.05
PORTS = {"leader": 9100, "member": 9101, "session": 9102, "orphan": 9103}
LEAKED = 9104  # A leader listener the session daemon deliberately keeps open.
GROUP = ("leader", "member", "orphan")
STATE = "/tmp/pg"


def write_file(path, text):
    with open(path + ".tmp", "w") as f:
        f.write(text)
    os.replace(path + ".tmp", path)


def read_files():
    found = {}
    for name in sorted(os.listdir(STATE)):
        if name.endswith(".tmp"):
            continue
        with open(os.path.join(STATE, name)) as f:
            found[name] = f.read()
    return found


def process_state(pid):
    """The /proc state letter, or None once the process has been reaped."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            return f.read().rsplit(")", 1)[1].split()[0]
    except FileNotFoundError:
        return None


def bind(port):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", port))
    s.listen(16)
    return s


def serve(tag, incarnation, boot, acceptor=None):
    """A single-threaded tagged listener; the counter advances only while it runs."""
    pid = os.getpid()
    write_file(f"{STATE}/pid-{tag}-{incarnation}", str(pid))
    bind_note = "bound"
    while acceptor is None:
        try:
            acceptor = bind(PORTS[tag])
        except OSError as error:
            # A survivor from an earlier incarnation may still hold the port.
            bind_note = f"retried after {errno.errorcode.get(error.errno, error.errno)}"
            write_file(f"{STATE}/bind-{tag}-{incarnation}", bind_note)
            time.sleep(TICK)
    write_file(f"{STATE}/bind-{tag}-{incarnation}", bind_note)
    counter, next_tick = 0, time.monotonic() + TICK
    while True:
        ready, _, _ = select.select([acceptor], [], [], max(0.0, next_tick - time.monotonic()))
        if time.monotonic() >= next_tick:
            counter += 1
            next_tick += TICK
            write_file(f"{STATE}/counter-{tag}-{incarnation}", str(counter))
        if not ready:
            continue
        channel, _ = acceptor.accept()
        with channel:
            command = channel.makefile("rb").readline().decode().strip()
            reply = {"tag": tag, "incarnation": incarnation, "boot": boot, "pid": pid,
                     "ppid": os.getppid(), "pgid": os.getpgid(0), "sid": os.getsid(0),
                     "counter": counter}
            if command == "FILES":
                reply["files"] = read_files()
                reply["states"] = {name: process_state(int(text)) for name, text in reply["files"].items()
                                   if name.startswith("pid-")}
            channel.sendall(json.dumps(reply).encode() + b"\n")
            if command == "EXIT":
                os._exit(0)


def detach_output():
    """Replace the inherited output pipes, as a daemon does; the caller has
    already closed the leader's listener, the only other inherited descriptor."""
    devnull = os.open("/dev/null", os.O_RDWR)
    for fd in (0, 1, 2):
        os.dup2(devnull, fd)
    os.close(devnull)


def app():
    os.makedirs(STATE, exist_ok=True)
    path = f"{STATE}/incarnation"
    incarnation = int(open(path).read()) + 1 if os.path.exists(path) else 1
    write_file(path, str(incarnation))
    boot = open("/proc/sys/kernel/random/boot_id").read().strip()
    acceptor = bind(PORTS["leader"])
    try:
        leaked = bind(LEAKED)
        write_file(f"{STATE}/bind-leaked-{incarnation}", "bound")
    except OSError as error:
        leaked = None
        write_file(f"{STATE}/bind-leaked-{incarnation}", errno.errorcode.get(error.errno, str(error.errno)))
    for tag in ("member", "session", "orphan"):
        pid = os.fork()
        if pid == 0:
            try:
                acceptor.close()
                if leaked is not None and tag != "session":
                    leaked.close()
                if tag == "session":
                    os.setsid()
                elif tag == "orphan" and os.fork() != 0:
                    os._exit(0)  # The intermediate exits; PID 1 adopts the grandchild.
                detach_output()
                serve(tag, incarnation, boot)
            finally:
                os._exit(1)
        if tag == "orphan":
            os.waitpid(pid, 0)
    print(f"APP_READY incarnation={incarnation}", flush=True)
    serve("leader", incarnation, boot, acceptor)


# --------------------------------------------------------------- oracle side

class Refused(Exception):
    pass


def open_request(tag, command="STATUS", timeout=2.0):
    s = socket.socket()
    s.settimeout(timeout)
    try:
        s.connect(("app", PORTS[tag]))
    except ConnectionRefusedError as error:
        s.close()
        raise Refused(tag) from error
    s.sendall(command.encode() + b"\n")
    return s


def read_reply(s):
    s.settimeout(5.0)
    with s:
        line = s.makefile("rb").readline()
    if not line:
        raise RuntimeError("listener closed without a reply")
    return json.loads(line)


def ask(tag, command="STATUS", timeout=2.0):
    return read_reply(open_request(tag, command, timeout))


def connects(port):
    try:
        with socket.create_connection(("app", port), timeout=1.0):
            return True
    except ConnectionRefusedError:
        return False


def refused(tag):
    try:
        ask(tag, timeout=1.0)
    except Refused:
        return True
    return False


def silent(tag):
    """Connected and queued by the kernel, but no reply from a frozen process."""
    s = open_request(tag, timeout=1.0)
    try:
        s.recv(1)
    except socket.timeout:
        return s  # Keep it open: the reply must arrive after resume.
    s.close()
    return None


def wait_for(predicate, what, timeout=20.0):
    deadline = time.monotonic() + timeout
    while True:
        try:
            if value := predicate():
                return value
        except (OSError, Refused, RuntimeError, ValueError):
            pass
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out waiting for {what}")
        time.sleep(0.1)


def counters(files, incarnation):
    return {tag: int(files.get(f"counter-{tag}-{incarnation}", "0")) for tag in PORTS}


def check(condition, message, evidence=None):
    if not condition:
        raise RuntimeError(f"{message}: {json.dumps(evidence, sort_keys=True)}")


def ready(incarnation):
    return {tag: wait_for(lambda tag=tag: (s := ask(tag))["incarnation"] == incarnation and s,
                          f"{tag} incarnation {incarnation}") for tag in PORTS}


def check_topology(statuses):
    leader = statuses["leader"]
    check(leader["pgid"] == leader["pid"], "leader leads its process group", leader)
    for tag in ("member", "orphan"):
        check(statuses[tag]["pgid"] == leader["pid"] and statuses[tag]["sid"] == leader["sid"],
              f"{tag} stays in the application group and session", statuses)
    check(statuses["member"]["ppid"] == leader["pid"], "member is the leader's child", statuses)
    check(statuses["orphan"]["ppid"] == 1, "double-forked orphan is adopted by PID 1", statuses)
    session = statuses["session"]
    check(session["sid"] == session["pid"] == session["pgid"] != leader["pgid"],
          "session daemon leads its own session and group", statuses)


def pause_phase(world, incarnation):
    """pause_process freezes the group only; queued connections are answered once."""
    before = ask("session", "FILES")
    world.pause_process("app")
    pending = {tag: silent(tag) for tag in GROUP}
    check(all(pending.values()), "frozen group members must not answer", list(pending))
    first = ask("session", "FILES")
    world.sleep(1)
    second = ask("session", "FILES")
    for tag in GROUP:
        check(first["states"][f"pid-{tag}-{incarnation}"] == "T", f"{tag} is stopped", first["states"])
    check(counters(first["files"], incarnation)["member"] == counters(second["files"], incarnation)["member"]
          and counters(first["files"], incarnation)["orphan"] == counters(second["files"], incarnation)["orphan"],
          "frozen counters stay put", [first["files"], second["files"]])
    check(second["counter"] > first["counter"] > before["counter"], "session daemon keeps running",
          [before["counter"], first["counter"], second["counter"]])
    world.resume_process("app")
    replies = {tag: read_reply(s) for tag, s in pending.items()}
    for tag, reply in replies.items():
        check(reply["tag"] == tag and reply["incarnation"] == incarnation, f"queued {tag} answered once", reply)
    after = ask("session", "FILES")
    world.sleep(0.5)
    later = ask("session", "FILES")
    check(all(counters(later["files"], incarnation)[t] > counters(after["files"], incarnation)[t]
              for t in GROUP), "group counters advance after resume", [after["files"], later["files"]])
    return {"queued_answered": sorted(replies)}


def stop_phase(world, incarnation, statuses, frozen_first, leaked_open):
    """stop kills and reaps the group; the session daemon survives with its port."""
    if frozen_first:
        world.pause_process("app")
    world.stop("app")
    for tag in GROUP:
        check(refused(tag), f"{tag} port must refuse after stop")
    # Only a descriptor the survivor inherited and kept can hold a dead leader's port.
    check(connects(LEAKED) == leaked_open, f"leaked listener open={leaked_open} after stop")
    first = ask("session", "FILES")
    world.sleep(1)
    second = ask("session", "FILES")
    check(second["incarnation"] == incarnation and second["pid"] == statuses["session"]["pid"],
          "session daemon survives stop", second)
    for tag in GROUP:
        state = second["states"][f"pid-{tag}-{incarnation}"]
        check(state is None, f"stopped {tag} is reaped, not a zombie ({state})", second["states"])
    frozen = counters(first["files"], incarnation), counters(second["files"], incarnation)
    check(all(frozen[0][t] == frozen[1][t] for t in GROUP), "dead counters stay put", frozen)
    check(frozen[1]["session"] > frozen[0]["session"], "survivor counter advances", frozen)
    return frozen[1]


def restart_phase(world, incarnation, survivor, leaked_bind):
    """restart rebinds the group's ports; the survivor holds 9102 until it exits."""
    world.restart("app")
    fresh = incarnation + 1
    for tag in GROUP:
        status = wait_for(lambda tag=tag: ask(tag), f"{tag} after restart")
        check(status["incarnation"] == fresh, f"restarted {tag} incarnation", status)
    held = ask("session", "FILES")
    check(held["incarnation"] == incarnation and held["pid"] == survivor["pid"],
          "survivor still owns its port after restart", held)
    blocked = wait_for(lambda: ask("leader", "FILES")["files"].get(f"bind-session-{fresh}"),
                       "new session daemon bind attempt")
    check(blocked.startswith("retried after"), "new session daemon cannot bind while the survivor runs", blocked)
    leaked = ask("leader", "FILES")["files"][f"bind-leaked-{fresh}"]
    check(leaked == leaked_bind, f"new leader binds the leaked port: {leaked_bind}", leaked)
    # The new incarnation sees its predecessor's group counters stopped.
    first = counters(ask("leader", "FILES")["files"], incarnation)
    world.sleep(0.5)
    second = counters(ask("leader", "FILES")["files"], incarnation)
    check(all(first[t] == second[t] for t in GROUP), "previous incarnation stays dead", [first, second])
    goodbye = ask("session", "EXIT")
    check(goodbye["pid"] == survivor["pid"], "exit reached the survivor", goodbye)
    status = wait_for(lambda: (s := ask("session"))["incarnation"] == fresh and s, "port 9102 rebound")
    files = ask("leader", "FILES")
    check(files["states"][f"pid-session-{incarnation}"] is None, "exited survivor is reaped", files["states"])
    check(files["files"][f"bind-session-{fresh}"] == "retried after EADDRINUSE",
          "rebound only after the survivor exited", files["files"])
    check(connects(LEAKED) == (leaked_bind == "bound"), "leaked port released with the survivor")
    statuses = ready(fresh)
    check_topology(statuses)
    return statuses, {"survivor_held_port": True, "rebound_by": status["pid"] != survivor["pid"]}


def crash_phase(world, statuses):
    """A crash ends every process, survivor included; the reboot starts afresh."""
    old_boot = statuses["leader"]["boot"]
    world.crash("app")
    world.sleep(0.5)
    world.reboot("app")
    fresh = ready(1)
    check(all(s["boot"] != old_boot for s in fresh.values()), "rebooted processes see a new boot", fresh)
    check_topology(fresh)
    files = ask("leader", "FILES")["files"]
    check(all(not name.endswith("-2") and not name.endswith("-3") for name in files),
          "RAM-backed state from earlier incarnations is gone", files)
    check(files["bind-leaked-1"] == "bound", "the crash released every port", files)
    return fresh


def oracle():
    from seedbox import World
    with World.connect() as world:
        statuses = ready(1)
        check_topology(statuses)
        evidence = {"pause": pause_phase(world, 1)}
        stop_phase(world, 1, statuses, frozen_first=False, leaked_open=True)
        statuses, evidence["restart"] = restart_phase(world, 1, statuses["session"], "EADDRINUSE")
        # Stopping a frozen group still kills and reaps it. Incarnation 2 never
        # bound the leaked port, so nothing holds it after this stop.
        stop_phase(world, 2, statuses, frozen_first=True, leaked_open=False)
        statuses, evidence["restart_after_frozen_stop"] = restart_phase(world, 2, statuses["session"], "bound")
        crash_phase(world, statuses)
        evidence["pause_after_reboot"] = pause_phase(world, 1)
    print("RESULT pass " + json.dumps(evidence, sort_keys=True), flush=True)
    return 0


def main():
    role = sys.argv[1]
    if role == "app":
        app()
    elif role == "oracle":
        return oracle()
    else:
        raise RuntimeError(f"unknown role {role}")


if __name__ == "__main__":
    raise SystemExit(main())
