#!/usr/bin/env python3
"""Interleaved native / empty / budget DBI, optionally with real progress syscalls."""
import argparse
import hashlib
import json
import statistics
import subprocess
import time
from pathlib import Path

p = argparse.ArgumentParser()
p.add_argument("--drrun", type=Path, required=True)
p.add_argument("--client", type=Path, required=True)
p.add_argument("--output", type=Path, required=True)
p.add_argument("--trials", type=int, default=5)
p.add_argument("--include-progress", action="store_true")
p.add_argument("command", nargs=argparse.REMAINDER)
a = p.parse_args()
command = a.command[1:] if a.command[:1] == ["--"] else a.command
if not command or a.trials < 1:
    p.error("provide a command and a positive trial count")
prefixes = {
    "native": [],
    "empty_dbi": [str(a.drrun.resolve()), "--"],
    "budget_dbi": [str(a.drrun.resolve()), "-c", str(a.client.resolve()), "--"],
}
if a.include_progress:
    prefixes["progress_dbi"] = [str(a.drrun.resolve()), "-c", str(a.client.resolve()),
                                "--progress", "--"]
records = []
for trial in range(a.trials):
    modes = list(prefixes)
    if trial % 2:
        modes.reverse()
    for mode in modes:
        start = time.monotonic()
        try:
            result = subprocess.run(prefixes[mode] + command, capture_output=True,
                                    text=True, timeout=60)
            record = dict(trial=trial, mode=mode, seconds=time.monotonic() - start,
                          returncode=result.returncode, stdout=result.stdout,
                          stderr=result.stderr)
        except subprocess.TimeoutExpired:
            record = dict(trial=trial, mode=mode, seconds=time.monotonic() - start,
                          returncode=124, stdout="", stderr="experiment timeout")
        records.append(record)
        a.output.write_text(json.dumps(dict(command=command, records=records), indent=2) + "\n")
        print(mode, trial, round(record["seconds"], 4), record["returncode"], flush=True)
        if record["returncode"]:
            raise SystemExit("Execution failed; partial evidence saved in " + str(a.output))
native_output = records[0]["stdout"]
summary = {
    "output_matches": all(r["stdout"] == native_output for r in records),
    "stdout_sha256": hashlib.sha256(native_output.encode()).hexdigest(),
    "median_seconds": {m: statistics.median(r["seconds"] for r in records if r["mode"] == m)
                       for m in prefixes},
    "budget_stderr_identical": len({r["stderr"] for r in records if r["mode"] == "budget_dbi"}) == 1,
    "budget_callbacks_present": all("budget: checkpoints=" in r["stderr"]
                                    for r in records if r["mode"] in ("budget_dbi", "progress_dbi")),
}
if a.include_progress:
    summary["progress_stderr_identical"] = len({r["stderr"] for r in records
                                               if r["mode"] == "progress_dbi"}) == 1
a.output.write_text(json.dumps(dict(command=command, summary=summary, records=records), indent=2) + "\n")
print(json.dumps(summary, indent=2))
if not summary["output_matches"] or not summary["budget_callbacks_present"]:
    raise SystemExit("Output mismatch or missing instrumentation")
