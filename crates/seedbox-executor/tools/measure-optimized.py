#!/usr/bin/env python3
"""Frozen ARM optimized-path matrix. Never builds, tunes, or drops samples.

Uses historical A for existing rows and a same-source A-emitter build ONLY for
new call17. Native is unmetered. Matched JIT retains its four full-state checks
per round; separate long call17 amortizes the driver/completion confound.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import time

spec = importlib.util.spec_from_file_location("matched_jit", Path(__file__).with_name("measure-jit.py"))
jit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(jit)
ORDERS = [["native", "dr", "baseline", "candidate"],
          ["candidate", "baseline", "dr", "native"],
          ["dr", "candidate", "native", "baseline"],
          ["baseline", "native", "candidate", "dr"],
          ["dr", "baseline", "native", "candidate"]]
BATCHES = {"compute": 8, "short": 32, "call17": 16, "legacy-jit": 8,
           **{row: 64 for row in jit.ROWS}}


def validate_warm(text, row, mode):
    if row == "short":
        return jit.validate_short(text, mode)
    lines = [line for line in text.splitlines() if line.startswith("EXECUTOR_BENCH ")]
    if len(lines) != 1 or "EXECUTOR_FAIL" in text or "EXECUTOR_PANIC" in text:
        raise ValueError("missing/ambiguous warm result or failed fixture")
    metrics = dict(item.split("=", 1) for item in lines[0].split()[1:])
    metrics = {k: int(v) if v.lstrip("-").isdigit() else v for k, v in metrics.items()}
    count, rounds, checksum, logical, kind, translations = {
        "compute": (2000000, 1, 252000000, 256000002, "Compute", 2),
        "call17": (10000000, 1, 130000000, 170000002, "Call17", 4),
        "legacy-jit": (250000, 8, 253000000, 256000016, "Jit", 16),
    }[row]
    expected = dict(iterations=count, rounds=rounds, checksum=checksum, logical=logical,
                    kind=kind, gate="false", native="false" if mode == "candidate" else "true")
    if mode == "candidate":
        payments = (logical - 1) // 50000
        expected.update(payments=payments, remaining=50000 + payments * 50000 - logical,
                        translations=translations)
    for key, value in expected.items():
        if metrics[key] != value:
            raise ValueError(f"{row} {key}: {metrics[key]} != {value}")
    return metrics


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for arg in ["baseline", "baseline_common", "candidate", "reference", "output"]:
        p.add_argument(arg, type=Path)
    args = p.parse_args()
    for arg in ["baseline", "baseline_common", "candidate", "reference"]:
        setattr(args, arg, getattr(args, arg).resolve())
    args.output.mkdir(parents=True, exist_ok=False)
    cap = 420
    plan = dict(rows=list(BATCHES), batches=BATCHES, orders=ORDERS, cap_seconds=cap,
                inputs={k: str(v) for k, v in vars(args).items()},
                note="Existing A immutable; A-common only adds call17 fixture. All modes fixed useful work; charge-prefix differences not normalized away.")
    (args.output / "plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    deadline = time.monotonic() + cap
    with (args.output / "samples.jsonl").open("x") as saved:
        for repeat, order in enumerate(ORDERS):
            for row, batches in BATCHES.items():
                for mode in order:
                    sums = {}
                    for batch in range(batches):
                        if time.monotonic() >= deadline:
                            raise SystemExit("cap reached: partial evidence retained")
                        metered = mode in ("baseline", "candidate")
                        binary = args.candidate if mode == "candidate" else args.baseline_common if row == "call17" else args.baseline
                        if row in jit.ROWS:
                            arg = f"jit-{'linked' if metered else 'native'}-{row}"
                        else:
                            arg = f"{'bench' if metered else 'native'}-{'jit' if row == 'legacy-jit' else row}"
                        command = (jit.reference_command(args.reference) if mode == "dr" else []) + [str(binary), arg]
                        sample = dict(repeat=repeat, row=row, mode=mode, batch=batch, command=command,
                                      load_before=os.getloadavg(), started_unix=time.time())
                        start = time.monotonic_ns()
                        proc = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                                text=True, start_new_session=True)
                        try:
                            text, _ = proc.communicate(timeout=min(20, max(.001, deadline-time.monotonic())))
                            sample["exit"] = proc.returncode
                        except subprocess.TimeoutExpired:
                            os.killpg(proc.pid, signal.SIGKILL)
                            text, _ = proc.communicate()
                            sample["exit"] = 124
                        sample.update(output=text, process_ns=time.monotonic_ns()-start, load_after=os.getloadavg())
                        try:
                            if sample["exit"] != 0:
                                raise ValueError(f"exit {sample['exit']}")
                            checked_mode = "candidate" if mode == "baseline" else mode
                            sample["metrics"] = jit.validate(text, row, checked_mode) if row in jit.ROWS else validate_warm(text, row, checked_mode)
                        except (ValueError, KeyError, TypeError) as error:
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
