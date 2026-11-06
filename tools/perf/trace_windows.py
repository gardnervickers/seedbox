#!/usr/bin/env python3
"""Size whole-window parallel execution from SEEDBOX-TRACE lines.

Reads the temporary Driver trace (one `SEEDBOX-TRACE run ...` line per machine
run, one `batch` line per batch, one `step` line per Driver step) from a
worker's stderr and prints, as JSON:

- totals: steps, batches, runs; summed step, batch and run wall time;
- per machine: runs, summed run wall, zero-progress runs;
- today's overlap: summed run wall / summed batch wall;
- the ideal whole-window bound: simulated time is cut into aligned windows
  (default 1 ms, the default quantum and link latency, so frames sent in one
  window only arrive in a later one); each window costs its busiest machine's
  summed run wall, and the bound is the sum over windows;
- the same bound with one run per machine per window, by removing the fixed
  per-run cost (median wall of runs that made no simulated progress) from
  every run after the first in each window.

Usage: trace_windows.py TRACE_FILE [--window-ns N] [--from-ns A] [--to-ns B]
"""
import argparse
import collections
import json
import re
import statistics
import sys

FIELD = re.compile(r"(\w+)=(\S+)")


def parse(path):
    runs, batches, steps = [], [], []
    with open(path, "r", errors="replace") as handle:
        for line in handle:
            at = line.find("SEEDBOX-TRACE ")
            if at < 0:
                continue
            rest = line[at + len("SEEDBOX-TRACE "):].split(None, 1)
            if len(rest) != 2:
                continue
            kind, body = rest
            fields = dict(FIELD.findall(body))
            if kind == "run":
                runs.append(fields)
            elif kind == "batch":
                batches.append(fields)
            elif kind == "step":
                steps.append(fields)
    return runs, batches, steps


def num(value):
    try:
        return int(value)
    except (TypeError, ValueError):
        return 0


def quantiles(values):
    if not values:
        return {}
    values = sorted(values)
    pick = lambda q: values[min(len(values) - 1, int(q * len(values)))]
    return {"n": len(values), "p5": pick(0.05), "p50": pick(0.5), "p95": pick(0.95),
            "mean": int(statistics.fmean(values))}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("trace")
    parser.add_argument("--window-ns", type=int, default=1_000_000)
    parser.add_argument("--from-ns", type=int, default=0)
    parser.add_argument("--to-ns", type=int, default=2**63)
    args = parser.parse_args()

    runs, batches, steps = parse(args.trace)
    inside = lambda at: args.from_ns <= at < args.to_ns
    runs = [r for r in runs if inside(num(r.get("start")))]
    batches = [b for b in batches if inside(num(b.get("now")))]
    steps = [s for s in steps if inside(num(s.get("from")))]

    per_machine = collections.defaultdict(lambda: {"runs": 0, "wall_ns": 0, "zero_runs": 0})
    zero_walls, moving_walls = [], []
    for run in runs:
        machine = run.get("m")
        wall = num(run.get("wall"))
        zero = num(run.get("reached")) <= num(run.get("start"))
        entry = per_machine[machine]
        entry["runs"] += 1
        entry["wall_ns"] += wall
        entry["zero_runs"] += zero
        (zero_walls if zero else moving_walls).append(wall)

    fixed = statistics.median(zero_walls) if zero_walls else 0
    windows = collections.defaultdict(lambda: collections.defaultdict(lambda: [0, 0]))
    for run in runs:
        window = num(run.get("start")) // args.window_ns
        cell = windows[window][run.get("m")]
        cell[0] += num(run.get("wall"))
        cell[1] += 1

    ideal = 0
    ideal_one_run = 0
    active = collections.Counter()
    for machines in windows.values():
        ideal += max(wall for wall, _ in machines.values())
        ideal_one_run += max(max(0, wall - (count - 1) * fixed) for wall, count in machines.values())
        active[len(machines)] += 1

    run_wall = sum(num(r.get("wall")) for r in runs)
    batch_wall = sum(num(b.get("wall")) for b in batches)
    step_wall = sum(num(s.get("wall")) for s in steps)
    kinds = collections.Counter(b.get("kind") for b in batches)
    sizes = collections.Counter(num(b.get("n")) for b in batches)
    simulated = (max((num(s.get("to")) for s in steps), default=0)
                 - min((num(s.get("from")) for s in steps), default=0))

    report = {
        "filter_ns": [args.from_ns, min(args.to_ns, 2**63 - 1)],
        "window_ns": args.window_ns,
        "simulated_ns": simulated,
        "steps": len(steps),
        "batches": len(batches),
        "runs": len(runs),
        "step_wall_ns": step_wall,
        "batch_wall_ns": batch_wall,
        "run_wall_ns": run_wall,
        "outside_batches_ns": step_wall - batch_wall,
        "overlap_today": round(run_wall / batch_wall, 3) if batch_wall else None,
        "batch_kinds": dict(kinds),
        "batch_sizes": dict(sorted(sizes.items())),
        "per_machine": dict(sorted(per_machine.items())),
        "zero_progress_run_wall": quantiles(zero_walls),
        "moving_run_wall": quantiles(moving_walls),
        "fixed_per_run_ns": int(fixed),
        "windows": len(windows),
        "machines_active_per_window": dict(sorted(active.items())),
        "ideal_window_bound_ns": ideal,
        "ideal_window_bound_one_run_ns": int(ideal_one_run),
        "speedup_vs_batches": round(batch_wall / ideal, 2) if ideal else None,
        "speedup_one_run_vs_batches": round(batch_wall / ideal_one_run, 2) if ideal_one_run else None,
    }
    json.dump(report, sys.stdout, indent=1)
    print()


if __name__ == "__main__":
    main()
