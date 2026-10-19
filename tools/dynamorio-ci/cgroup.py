#!/usr/bin/env python3
"""Run an ordinary-user fixture command in a uniquely owned pids delegation.

The caller must already have pids delegated by its parent. This supervisor never
changes a shared controller, sysctl, or the membership of an unrelated process.
"""
import argparse
import ctypes
import os
import pwd
from pathlib import Path
import select
import signal
import sys
import uuid


def write(path, value):
    path.write_text(value)


def wait_empty(root):
    with (root / "cgroup.events").open() as events:
        poll = select.poll()
        poll.register(events.fileno(), select.POLLPRI | select.POLLERR)
        while True:
            events.seek(0)
            state = dict(line.split() for line in events)
            if state.get("populated") == "0":
                return
            if not poll.poll(30_000):
                raise RuntimeError(f"owned cgroup did not drain: {root}")


def launch(args):
    if os.geteuid() != 0:
        raise RuntimeError("use sudo for this scoped cgroup supervisor; command runs as --uid")
    if args.uid <= 0 or args.gid <= 0:
        raise RuntimeError("fixture command must run as a non-root uid/gid")
    if not args.command or args.command[0] != "--" or len(args.command) < 2:
        raise RuntimeError("expected -- COMMAND ARGS...")
    mount = Path("/sys/fs/cgroup").resolve()
    membership = next(
        line.removeprefix("0::")
        for line in Path("/proc/self/cgroup").read_text().splitlines()
        if line.startswith("0::")
    )
    parent = (mount / membership.lstrip("/")).resolve()
    if not parent.is_relative_to(mount):
        raise RuntimeError("current cgroup is outside the visible cgroup-v2 mount")
    if "pids" not in (parent / "cgroup.subtree_control").read_text().split():
        raise RuntimeError(
            f"{parent}: parent has not delegated pids; provision runner delegation "
            "(this helper will not change shared controllers)"
        )
    root = parent / f"seedbox-dr-{uuid.uuid4().hex}"
    if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "PR_SET_CHILD_SUBREAPER")
    root.mkdir(mode=0o700)  # exclusive; never adopt an existing resource
    child = None
    reaped = False
    try:
        write(root / "cgroup.subtree_control", "+pids")
        for path in [root, *(root / name for name in (
            "cgroup.procs", "cgroup.threads", "cgroup.subtree_control"
        ))]:
            os.chown(path, args.uid, args.gid)
        controller = root / "controller"
        controller.mkdir()
        gate_read, gate_write = os.pipe2(os.O_CLOEXEC)
        child = os.fork()
        if child == 0:
            try:
                for number in [signal.SIGTERM, signal.SIGINT, signal.SIGHUP]:
                    signal.signal(number, signal.SIG_DFL)
                os.close(gate_write)
                # Only this child is moved; no command runs before the gate.
                if os.read(gate_read, 1) != b"1":
                    os._exit(125)
                os.close(gate_read)
                os.setgroups([])
                os.setgid(args.gid)
                os.setuid(args.uid)
                environment = {k: v for k, v in os.environ.items() if not k.startswith("SUDO_")}
                account = pwd.getpwuid(args.uid)
                environment.update(HOME=account.pw_dir, USER=account.pw_name, LOGNAME=account.pw_name)
                environment["SEEDBOX_DYNAMORIO_CGROUP_ROOT"] = str(root)
                os.execvpe(args.command[1], args.command[1:], environment)
            except BaseException as error:
                print(f"isolate: exec normal-user command: {error}", file=sys.stderr)
                os._exit(125)
        os.close(gate_read)
        write(controller / "cgroup.procs", str(child))
        os.write(gate_write, b"1")
        os.close(gate_write)
        _, status = os.waitpid(child, 0)
        reaped = True
        return os.waitstatus_to_exitcode(status)
    finally:
        signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM, signal.SIGINT, signal.SIGHUP})
        # Kill only this uniquely created domain, including any surviving test
        # leaves. The supervisor itself stays outside and can always reap.
        write(root / "cgroup.kill", "1")
        if child is not None and not reaped:
            os.waitpid(child, 0)
        wait_empty(root)
        try:
            while True:
                os.waitpid(-1, 0)
        except ChildProcessError:
            pass
        for path in sorted(root.rglob("*"), key=lambda p: len(p.parts), reverse=True):
            if path.is_dir():
                path.rmdir()
        root.rmdir()


def interrupted(number, _frame):
    raise InterruptedError(f"controller received signal {number}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--uid", type=int, required=True)
    parser.add_argument("--gid", type=int, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    for number in [signal.SIGTERM, signal.SIGINT, signal.SIGHUP]:
        signal.signal(number, interrupted)
    try:
        result = launch(args)
        return result if result >= 0 else 128 - result
    except (OSError, RuntimeError, StopIteration) as error:
        print(f"isolate: setup cgroup delegation: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
