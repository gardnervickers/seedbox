#!/usr/bin/env python3
"""Run only the ignored committed-trigger KVM unit controls under a core lease.

Not an application scenario runner: guest acceptance uses seedbox run/replay.
Usage: sudo python3 harness/test-triggers.py CPU TEST_BINARY
Build TEST_BINARY with cargo test-vmm --no-run first.
"""
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import batch
import msr54


def main():
    if os.geteuid() != 0:
        raise ValueError("root required")
    cpu, binary = int(sys.argv[1]), Path(sys.argv[2]).resolve(strict=True)
    expected = {
        "committed_trigger_world_effects_follow_arm_order_across_reversed_machine_ties",
        "committed_trigger_world_retains_already_routed_response_and_lagging_peer",
        "committed_trigger_world_snapshot_keeps_partial_match_and_scheduled_effect",
        "committed_trigger_unused_surface_preserves_witness_and_report_bytes",
        "committed_trigger_world_does_not_rescan_old_console_after_arm",
        "committed_console_discards_rolled_back_candidate_before_matcher_feed",
    }
    listed = subprocess.check_output([str(binary), "--list"], text=True)
    names = {line.removesuffix(": test").rsplit("::", 1)[-1] for line in listed.splitlines() if line.endswith(": test")}
    if not expected <= names:
        raise ValueError("test binary is missing committed-trigger controls")
    protected = sorted(batch.siblings(cpu))
    batch.enable_subreaper()
    def interrupted(signum, _frame):
        raise batch.Interrupted(signum)
    previous = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        with batch.cpu_locks(protected):
            state = Path(tempfile.mkdtemp(prefix="seedbox-trigger-msr-", dir="/tmp"))
            snapshot = state / "saved.json"
            try:
                if not msr54.required():
                    raise ValueError("these controls require an AMD Zen host")
                msr54.set_workaround(protected, snapshot)
                for selected in ("trigger_world_tests::", "committed_console_discards"):
                    child = subprocess.Popen(["taskset", "-c", str(cpu), str(binary), selected,
                                              "--ignored", "--nocapture", "--test-threads=1"], start_new_session=True)
                    try:
                        deadline = time.monotonic() + 120
                        while (status := batch.exit_status(child)) is None:
                            if time.monotonic() >= deadline:
                                raise TimeoutError("KVM trigger control exceeded 120 seconds")
                            time.sleep(0.05)
                        if status != 0:
                            raise RuntimeError(f"{selected}: test exit {status}")
                    finally:
                        batch.stop_owned_group(child)
            finally:
                if snapshot.exists():
                    msr54.restore_snapshot(protected, snapshot)
                if not snapshot.exists():
                    state.rmdir()
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, TimeoutError, batch.Interrupted) as error:
        sys.exit(f"trigger KVM controls: {error}")
