#!/usr/bin/env python3
"""Bounded sequential matched JIT measurements, after separate correctness gates.

No build or tuning. Each batch is a fresh process/finite arena; only its inner
clock is aggregated. Native is unmetered. Preserve every output, failure and
command; reject timing on any failed version/ledger assertion. A timed-out
process group is killed and reaped. Plan is written BEFORE measurements.
"""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time

ROWS = ["zero", "zero-medium", "zero-high", "append-low", "append-medium",
        "append-high", "patch-low", "patch-medium", "patch-high"]
ORDERS = [["native", "dr", "candidate"], ["candidate", "dr", "native"],
          ["dr", "native", "candidate"], ["native", "candidate", "dr"],
          ["dr", "candidate", "native"]]


def reference_command(reference):
    return [str(reference / "lib/ld-linux-aarch64.so.1"), "--library-path",
            str(reference / "lib"), str(reference / "bin64/drrun"), "-opt_speed",
            "-disable_traces", "-no_ldstex2cas", "-no_hw_cache_consistency", "-c",
            str(reference / "libseedbox_progress.so"), "--instructions-per-progress-call",
            "50000", "--progress-points-per-block", "1", "--"]


def validate(text, row, mode):
    match = re.search(r"^JIT_RESULT (.*)$", text, re.M)
    if not match or "EXECUTOR_FAIL" in text or "EXECUTOR_PANIC" in text:
        raise ValueError("missing result or failed fixture")
    metrics = dict(item.split("=", 1) for item in match[1].split())
    for key, value in list(metrics.items()):
        if re.fullmatch(r"-?\d+", value):
            metrics[key] = int(value)
    per_target = 32 if row.endswith("high") else 512 if row.endswith("medium") else 8192
    zero = row.startswith("zero")
    calls = 128 * per_target
    checksum = per_target * (64 * 24 if zero else sum(24 + r + 1 for r in range(64)))
    expected = dict(rounds=64, calls=calls, checks=256, checksum=checksum,
                    logical=calls * 17 + 512, publications=0 if zero else 64,
                    store_width=4 if row.startswith("patch") else 56)
    for key, value in expected.items():
        if metrics[key] != value:
            raise ValueError(f"{key}: {metrics[key]} != {value}")
    if metrics["native"] != ("false" if mode == "candidate" else "true") or metrics["diagnostic"] != "false":
        raise ValueError("wrong execution mode")
    if mode == "candidate":
        expected.update(translations=5 if zero else 68 if row.startswith("append") else 69,
                        resolutions=5 if zero else 68 if row.startswith("append") else 69,
                        invalidated=0 if zero else 64 if row.startswith("patch") else 56,
                        neighbor_reuse=0 if zero else 64, pinned_bytes=0,
                        live=12 if row.startswith("append") else 5,
                        overlap_probes=0 if zero else 320 if row.startswith("patch") else 536,
                        gate_probes=0 if zero else 768)
        for key, value in expected.items():
            if metrics[key] != value:
                raise ValueError(f"candidate {key}: {metrics[key]} != {value}")
        if metrics["remaining"] != 50000 + metrics["payments"] * 50000 - metrics["logical"]:
            raise ValueError("invalid charge ledger")
        if metrics["payments"] != (metrics["logical"] - 1) // 50000:
            raise ValueError("invalid payment count")
    parts = sum(metrics[k] for k in ["write_barrier_ns", "index_gate_ns", "first_ns", "warm_ns"])
    if parts > metrics["total_ns"]:
        raise ValueError("overlapping clock buckets")
    return metrics


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("binary", type=Path)
    p.add_argument("reference", type=Path)
    p.add_argument("output", type=Path)
    p.add_argument("--batches", type=int, default=96)
    p.add_argument("--cap-seconds", type=int, default=480)
    p.add_argument("--variant", type=Path, help="Stage B binary; rotate it against the frozen baseline candidate and unchanged native/DR binary.")
    args = p.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    binary, reference = args.binary.resolve(), args.reference.resolve()
    if args.batches <= 0 or args.cap_seconds <= 0:
        p.error("positive finite batches/cap required")
    orders = ORDERS if args.variant is None else [
        ["native", "dr", "baseline", "candidate"], ["candidate", "baseline", "dr", "native"],
        ["dr", "candidate", "native", "baseline"], ["baseline", "native", "candidate", "dr"],
        ["dr", "baseline", "native", "candidate"]]
    plan = dict(rows=ROWS, orders=orders, batches=args.batches, cap_seconds=args.cap_seconds,
                variant=str(args.variant.resolve()) if args.variant else None, binary=str(binary), reference=str(reference),
                note="Separate fresh-process batches; no launch time in inner total; all raw outputs retained.")
    (args.output / "plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    deadline = time.monotonic() + args.cap_seconds
    with (args.output / "samples.jsonl").open("x") as saved:
        for repeat, order in enumerate(orders):
            for row in ROWS:
                for mode in order:
                    sums = {}
                    for batch in range(args.batches):
                        if time.monotonic() >= deadline:
                            raise SystemExit("timing cap reached; partial evidence retained, no abandoned child")
                        command = (reference_command(reference) if mode == "dr" else []) + [
                            str(args.variant.resolve() if args.variant and mode == "candidate" else binary),
                            f"jit-{'linked' if mode in ('candidate', 'baseline') else 'native'}-{row}"]
                        sample = dict(repeat=repeat, row=row, mode=mode, batch=batch, command=command,
                                      load_before=os.getloadavg(), started_unix=time.time())
                        start = time.monotonic_ns()
                        proc = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                                text=True, start_new_session=True)
                        try:
                            text, _ = proc.communicate(timeout=min(20, max(0.001, deadline-time.monotonic())))
                            sample["exit"] = proc.returncode
                        except subprocess.TimeoutExpired:
                            os.killpg(proc.pid, signal.SIGKILL)
                            text, _ = proc.communicate()
                            sample["exit"] = 124
                        sample.update(output=text, process_ns=time.monotonic_ns()-start,
                                      load_after=os.getloadavg())
                        try:
                            if sample["exit"] != 0:
                                raise ValueError(f"exit {sample['exit']}")
                            sample["metrics"] = validate(text, row, "candidate" if mode == "baseline" else mode)
                        except ValueError as error:
                            sample["failure"] = str(error)
                        saved.write(json.dumps(sample) + "\n")
                        saved.flush()
                        if "failure" in sample:
                            raise SystemExit(f"failed sample: {sample}")
                        for key, value in sample["metrics"].items():
                            if isinstance(value, int):
                                sums[key] = sums.get(key, 0) + value
                    print(json.dumps(dict(repeat=repeat, row=row, mode=mode, sums=sums)), flush=True)


if __name__ == "__main__":
    main()
