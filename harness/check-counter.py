#!/usr/bin/env python3
"""Run a discovered native test executable under the shared CPU/MSR lease."""
import argparse
import contextlib
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

import batch
import msr54


def run_leased(command, cpu=None, timeout=1800):
    batch.enable_subreaper()
    allowed = os.sched_getaffinity(0)
    candidates = [cpu] if cpu is not None else sorted(allowed - batch.siblings(0))
    previous = {}
    child = None

    def interrupted(signum, _frame):
        raise batch.Interrupted(signum)

    with contextlib.ExitStack() as stack:
        for selected in candidates:
            if selected not in allowed:
                raise ValueError(f"CPU {selected} is outside the allowed affinity")
            protected = sorted(batch.siblings(selected))
            try:
                stack.enter_context(batch.cpu_locks(protected))
                break
            except RuntimeError:
                if cpu is not None:
                    raise
        else:
            raise RuntimeError("no free CPU/SMT pair for the counter tests")
        print(f"counter CPU: {selected}; protected siblings: {protected}", flush=True)
        state = Path(tempfile.mkdtemp(prefix="seedbox-test-msr-", dir=batch.LOCK_DIR))
        snapshot = state / "saved.json"
        try:
            previous = {sig: signal.signal(sig, interrupted)
                        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
            if msr54.required():
                msr54.set_workaround(protected, snapshot)
            child = subprocess.Popen(["taskset", "-c", str(selected), *command], start_new_session=True)
            deadline = time.monotonic() + timeout
            while (status := batch.exit_status(child)) is None:
                if time.monotonic() >= deadline:
                    raise TimeoutError(f"counter tests exceeded {timeout}s")
                time.sleep(.05)
            return status if status >= 0 else 128 - status
        finally:
            # Hold locks and the exact workaround until every owned descendant
            # has been reaped, on success, failure, timeout and interruption.
            for sig in previous:
                signal.signal(sig, signal.SIG_IGN)
            try:
                if child is not None:
                    batch.stop_owned_group(child)
                if snapshot.exists():
                    try:
                        msr54.restore_snapshot(protected, snapshot)
                    except (OSError, ValueError):
                        print(f"MSR restoration failed; retained snapshot: {snapshot}", file=sys.stderr)
                        raise
                if not snapshot.exists():
                    state.rmdir()
            finally:
                for sig, handler in previous.items():
                    signal.signal(sig, handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cpu", type=int)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if sys.platform != "linux" or not command:
        parser.error("requires Linux and a test executable after --")
    return run_leased(command, args.cpu)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError, TimeoutError, batch.Interrupted) as error:
        sys.exit(f"counter tests: {error}")
