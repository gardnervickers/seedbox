"""Bounded process lifetime and crash-visible accounting (caller holds flock)."""
import contextlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time


def save(path, value):
    """Replace a complete JSON document; failed writes preserve the old file."""
    fd, temporary = tempfile.mkstemp(dir=path.parent, prefix=path.name + ".")
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temporary)


def execute(command, prefix, budget_path, budget, seed, timeout):
    """Account even interrupted/failed attempts. A hard crash leaves pending
    set, refusing another launch until the attempt is explicitly reconciled.
    SIGKILL/power loss cannot run cleanup; the pending receipt is fail-closed.
    """
    if budget.get("pending"):
        raise ValueError("unreconciled pending attempt; do not reset the budget")
    remaining = min(timeout, budget["limit_seconds"] - budget["spent_seconds"])
    if remaining <= 0:
        raise ValueError("aggregate service probe budget exhausted")
    interrupted = 0

    def request_stop(signum, _frame):
        nonlocal interrupted
        interrupted = interrupted or signum

    previous = {s: signal.signal(s, request_stop) for s in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    process = None
    start = time.monotonic()
    status, error = "EXITED", None
    try:
        budget["pending"] = {"path": str(prefix), "seed": seed, "timeout_seconds": remaining}
        save(budget_path, budget)
        with Path(str(prefix) + ".console").open("wb") as log, Path(str(prefix) + ".stderr").open("wb") as err:
            if not interrupted:
                process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log, stderr=err, start_new_session=True)
            while process is not None and process.poll() is None:
                if interrupted:
                    break
                left = remaining - (time.monotonic() - start)
                if left <= 0:
                    status = "TIMEOUT"
                    break
                try:
                    process.wait(timeout=min(left, 0.05))
                except subprocess.TimeoutExpired:
                    pass
    except BaseException as exc:
        status, error = "ERROR", exc
    finally:
        if process is not None:
            # Also reap descendants if the process exited between poll and kill.
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        elapsed = time.monotonic() - start
        if interrupted:
            status = "INTERRUPTED"
        record = {"path": str(prefix), "seed": seed, "wall_seconds": elapsed,
                  "process_status": status, "returncode": process.returncode if process else None,
                  "signal": interrupted}
        try:
            # A receipt survives a crash between the two ledger updates.
            save(Path(str(prefix) + ".attempt.json"), record)
            budget["spent_seconds"] += elapsed
            budget["runs"].append(record)
            budget.pop("pending", None)
            save(budget_path, budget)
        finally:
            for sig, handler in previous.items():
                signal.signal(sig, handler)
    if error is not None:
        raise error
    return record
