#!/usr/bin/env python3
"""Sequential ordinary-Linux fixed-work samples, never a PV-time benchmark.

Run in the existing Linux development shell, after correctness gates. The
reference directory must contain the reviewed/corrected matching ARM DR client.
No builds, containers, global tuning or simulation creation occur here.
"""
import argparse
import json
import os
import pathlib
import re
import subprocess
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("binary", type=pathlib.Path)
p.add_argument("reference", type=pathlib.Path)
p.add_argument("output", type=pathlib.Path)
args = p.parse_args()
args.output.mkdir(parents=True, exist_ok=False)
binary = str(args.binary.resolve())
reference = args.reference.resolve()
dr = [str(reference / "lib/ld-linux-aarch64.so.1"), "--library-path", str(reference / "lib"),
      str(reference / "bin64/drrun"), "-opt_speed", "-disable_traces", "-no_ldstex2cas",
      "-no_hw_cache_consistency", "-c", str(reference / "libseedbox_progress.so"),
      "--instructions-per-progress-call", "50000", "--progress-points-per-block", "1", "--"]
checksums = {"compute": 252000000, "short": 40000000, "jit": 253000000}
rows = []
for repeat, order in enumerate([
    ["native", "dr", "candidate"],
    ["candidate", "dr", "native"],
    ["dr", "native", "candidate"],
]):
    for kind in checksums:
        for mode in order:
            command = ([*dr] if mode == "dr" else []) + [binary, f"{'bench' if mode == 'candidate' else 'native'}-{kind}"]
            row = {"repeat": repeat, "mode": mode, "kind": kind, "command": command,
                   "load_before": os.getloadavg(), "started_unix": time.time()}
            start = time.monotonic_ns()
            try:
                run = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     text=True, timeout=30, check=False)
                row.update(exit=run.returncode, output=run.stdout)
            except subprocess.TimeoutExpired as error:
                row.update(exit=124, output=str(error.output))
            row.update(process_wall_ns=time.monotonic_ns() - start, load_after=os.getloadavg())
            text = row["output"]
            match = re.search(r"^EXECUTOR_BENCH (.*)$", text, re.M)
            if match:
                row["metrics"] = dict(item.split("=", 1) for item in match[1].split())
            rows.append(row)
            (args.output / f"{repeat}-{kind}-{mode}.log").write_text(text)
            (args.output / "measurements.json").write_text(json.dumps(rows, indent=2) + "\n")
            assert row["exit"] == 0 and match, row
            assert int(row["metrics"]["checksum"]) == checksums[kind], row
            print(repeat, kind, mode, row["metrics"]["warm_ns"], flush=True)
