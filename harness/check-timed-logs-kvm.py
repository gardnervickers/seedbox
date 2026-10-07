#!/usr/bin/env python3
"""Run only timed-log KVM unit fixtures under the existing CPU/MSR lease protocol.

Not an application scenario runner. Applications use seedbox run/replay directly.
"""
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile

import batch
import msr54


def main():
    if len(sys.argv) != 2 or not sys.argv[1].isdigit() or os.geteuid() != 0:
        raise SystemExit("usage (root): python3 harness/check-timed-logs-kvm.py CPU")
    cpu = int(sys.argv[1])
    protected = sorted(batch.siblings(cpu))
    if cpu not in os.sched_getaffinity(0) or not msr54.required():
        raise SystemExit("requires an allowed AMD KVM CPU")
    os.chdir(Path(__file__).resolve().parent.parent)
    batch.enable_subreaper()
    child = None

    def interrupted(signum, _frame):
        raise InterruptedError(f"signal {signum}")

    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, interrupted)
    with batch.cpu_locks(protected):
        state = Path(tempfile.mkdtemp(prefix="seedbox-timed-logs-", dir="/tmp"))
        snapshot = state / "saved.json"
        try:
            msr54.set_workaround(protected, snapshot)
            child = subprocess.Popen(
                ["taskset", "-c", str(cpu), "cargo", "test-vmm", "timed_log_tests",
                 "--", "--ignored", "--test-threads=1", "--nocapture"],
                start_new_session=True,
            )
            return child.wait()
        finally:
            for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
                signal.signal(sig, signal.SIG_IGN)
            if child is not None and child.returncode is None:
                batch.stop_owned_group(child)
            if snapshot.exists():
                msr54.restore_snapshot(protected, snapshot)
            state.rmdir()


if __name__ == "__main__":
    sys.exit(main())
