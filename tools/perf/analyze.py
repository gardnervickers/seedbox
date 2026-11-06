#!/usr/bin/env python3
"""Whole-window parallelism analysis of a SEEDBOX-TRACE vmm.stderr.

Usage: analyze.py STDERR [--cuts NAME=NS ...] [--fixed NS] [--json OUT]
Phases are [0, cut1), [cut1, cut2), ... named by the cuts given (first phase
is 'boot'). Times are simulation nanoseconds; walls are host nanoseconds.
"""
import argparse, json, math, re, statistics, sys
from collections import defaultdict

ap = argparse.ArgumentParser()
ap.add_argument("stderr")
ap.add_argument("--cut", action="append", default=[], help="NAME=NS: phase NAME starts at NS")
ap.add_argument("--fixed", type=float, default=None, help="fixed cost per run (ns); default: median zero-budget run wall")
ap.add_argument("--json", default=None)
ap.add_argument("--window", type=float, action="append", default=None, help="window ns (default 1e6)")
args = ap.parse_args()

KV = re.compile(r"(\w+)=(\S+)")
runs, batches, steps = [], {}, []
final_perf = None
for line in open(args.stderr, errors="replace"):
    if line.startswith("SEEDBOX-TRACE run "):
        d = dict(KV.findall(line))
        runs.append(dict(b=int(d["b"]), m=int(d["m"]), s=int(d["start"]), lim=int(d["limit"]),
                         e=int(d["reached"]), stop=d["stop"], dl=int(d["deadline"]),
                         w=int(d["wall"]), x=int(d["exec"]), xs=int(d["execs"]),
                         off=int(d["offered"]), outs=int(d["outs"]), fr=int(d["frames"])))
    elif line.startswith("SEEDBOX-TRACE batch "):
        d = dict(KV.findall(line))
        batches[int(d["b"])] = dict(kind=int(d["kind"]), n=int(d["n"]), now=int(d["now"]),
                                    upto=int(d["upto"]), w=int(d["wall"]), t=int(d["t"]))
    elif line.startswith("SEEDBOX-TRACE step "):
        d = dict(KV.findall(line))
        steps.append(dict(s=int(d["s"]), a=int(d["from"]), z=int(d["to"]), w=int(d["wall"]), t=int(d["t"])))
    elif line.startswith("SEEDBOX-PERF driver final"):
        final_perf = dict(KV.findall(line))

machines = sorted({r["m"] for r in runs})
end_sim = max(max(r["e"] for r in runs), max(s["z"] for s in steps))
cuts = [("boot", 0)] + [(c.split("=")[0], int(float(c.split("=")[1]))) for c in args.cut]
cuts.sort(key=lambda c: c[1])
phases = [(name, start, cuts[i + 1][1] if i + 1 < len(cuts) else end_sim + 1) for i, (name, start) in enumerate(cuts)]
phases.append(("all", 0, end_sim + 1))

zero = [r for r in runs if batches[r["b"]]["upto"] == batches[r["b"]]["now"]]
if args.fixed is None:
    fixed = statistics.median([r["w"] for r in zero]) if zero else 0
else:
    fixed = args.fixed

def phase_of(t):
    for name, a, z in phases[:-1]:
        if a <= t < z:
            return name
    return phases[-2][0]

out = {"machines": machines, "end_sim_ns": end_sim, "fixed_ns": fixed, "phases": {}}

# Run-level statistics.
def q(vals, p):
    if not vals:
        return 0
    vals = sorted(vals)
    k = min(len(vals) - 1, max(0, int(round(p * (len(vals) - 1)))))
    return vals[k]

kinds = defaultdict(lambda: dict(batches=0, runs=0, batch_wall=0, run_wall=0, max_wall=0))
for b, d in batches.items():
    k = kinds[d["kind"]]
    k["batches"] += 1
    k["batch_wall"] += d["w"]
for r in runs:
    k = kinds[batches[r["b"]]["kind"]]
    k["runs"] += 1
    k["run_wall"] += r["w"]
per_batch_max = defaultdict(int)
for r in runs:
    per_batch_max[r["b"]] = max(per_batch_max[r["b"]], r["w"])
for b, mx in per_batch_max.items():
    kinds[batches[b]["kind"]]["max_wall"] += mx
out["kinds"] = {str(k): v for k, v in sorted(kinds.items())}

zw = [r["w"] for r in zero]
pos = [r for r in runs if r not in zero] if len(runs) < 0 else [r for r in runs if batches[r["b"]]["upto"] > batches[r["b"]]["now"]]
out["run_stats"] = {
    "runs": len(runs), "zero_runs": len(zero), "positive_runs": len(pos),
    "zero_wall_p10_p50_p90_us": [q(zw, .1) / 1e3, q(zw, .5) / 1e3, q(zw, .9) / 1e3],
    "positive_wall_p10_p50_p90_us": [q([r["w"] for r in pos], p) / 1e3 for p in (.1, .5, .9)],
    "positive_exec_p10_p50_p90_us": [q([r["x"] for r in pos], p) / 1e3 for p in (.1, .5, .9)],
    "positive_nonexec_p10_p50_p90_us": [q([r["w"] - r["x"] for r in pos], p) / 1e3 for p in (.1, .5, .9)],
    "positive_progress_p10_p50_p90_us": [q([r["e"] - r["s"] for r in pos], p) / 1e3 for p in (.1, .5, .9)],
}
for lo, hi in ((0, 1), (1, 5000), (5000, 20000), (20000, 100000), (100000, 1000000), (1000000, 10**12)):
    sel = [r for r in pos if lo <= r["e"] - r["s"] < hi]
    out["run_stats"][f"positive_progress_{lo}_{hi}ns"] = dict(
        n=len(sel),
        wall_p50_us=q([r["w"] for r in sel], .5) / 1e3,
        exec_p50_us=q([r["x"] for r in sel], .5) / 1e3,
        wall_sum_s=sum(r["w"] for r in sel) / 1e9,
        progress_sum_ms=sum(r["e"] - r["s"] for r in sel) / 1e6,
    )

def windows_for(L, off):
    """Return dict window -> machine -> dict(cost, cost_nofixed, busy, runs, exec)."""
    W = defaultdict(lambda: defaultdict(lambda: dict(w=0.0, wf=0.0, x=0.0, busy=0.0, runs=0, pos=0)))
    for r in runs:
        s, e, w = r["s"], r["e"], r["w"]
        wf = max(0.0, w - fixed)
        if e <= s:
            k = math.floor((s - off) / L)
            c = W[k][r["m"]]
            c["w"] += w; c["wf"] += wf; c["x"] += r["x"]; c["runs"] += 1
            continue
        k0 = math.floor((s - off) / L)
        k1 = math.floor((e - 1 - off) / L)
        for k in range(k0, k1 + 1):
            a = max(s, off + k * L); z = min(e, off + (k + 1) * L)
            frac = (z - a) / (e - s)
            c = W[k][r["m"]]
            c["w"] += w * frac; c["wf"] += wf * frac; c["x"] += r["x"] * frac
            c["busy"] += z - a; c["runs"] += 1; c["pos"] += 1
    return W

for L in (args.window or [1e6]):
    for off in (0.0, L / 2):
        W = windows_for(L, off)
        res = {}
        for name, a, z in phases:
            ks = [k for k in W if a <= max(off + k * L, 0) < z]
            tot = sum(c["w"] for k in ks for c in W[k].values())
            ww_max = sum(max(c["w"] for c in W[k].values()) for k in ks)
            ww_max_nf = sum(max(c["wf"] + fixed for c in W[k].values()) for k in ks)
            ww_max_nf_pos = sum(max((c["wf"] + fixed) if c["pos"] else c["wf"] for c in W[k].values()) for k in ks)
            ww_max_nf_nofix = sum(max(c["wf"] for c in W[k].values()) for k in ks)
            ww_max_x = sum(max(c["x"] for c in W[k].values()) for k in ks)
            per_m = defaultdict(float); per_m_nf = defaultdict(float); busy = defaultdict(float)
            nwork = defaultdict(int)
            for k in ks:
                n = 0
                for m, c in W[k].items():
                    per_m[m] += c["w"]; per_m_nf[m] += c["wf"]; busy[m] += c["busy"]
                    if c["pos"]:
                        n += 1
                nwork[n] += 1
            span = (min(z, end_sim) - a)
            nwin = math.ceil(span / L)
            res[name] = dict(
                sim_span_ms=span / 1e6,
                windows_total=nwin,
                windows_with_any_run=len(ks),
                windows_by_machines_with_positive_work={str(n): v for n, v in sorted(nwork.items())},
                total_run_wall_s=tot / 1e9,
                whole_window_max_s=ww_max / 1e9,
                whole_window_one_fixed_per_window_s=ww_max_nf / 1e9,
                whole_window_one_fixed_per_busy_window_s=ww_max_nf_pos / 1e9,
                whole_window_no_fixed_s=ww_max_nf_nofix / 1e9,
                whole_window_exec_only_s=ww_max_x / 1e9,
                busiest_machine_total_s=max(per_m.values()) / 1e9 if per_m else 0,
                busiest_machine_nofixed_s=max(per_m_nf.values()) / 1e9 if per_m_nf else 0,
                per_machine_wall_s={str(m): per_m[m] / 1e9 for m in sorted(per_m)},
                per_machine_wall_nofixed_s={str(m): per_m_nf[m] / 1e9 for m in sorted(per_m_nf)},
            )
        out.setdefault("windows", {})[f"L={L/1e6:g}ms off={off/1e6:g}ms"] = res

# Today's schedule, per phase (batch belongs to the phase of its 'now').
for name, a, z in phases:
    bs = [b for b, d in batches.items() if a <= d["now"] < z]
    st = [s for s in steps if a <= s["a"] < z]
    rs = [r for r in runs if a <= batches[r["b"]]["now"] < z]
    posb = [b for b in bs if batches[b]["upto"] > batches[b]["now"]]
    out["phases"][name] = dict(
        sim_start_ms=a / 1e6, sim_end_ms=min(z, end_sim) / 1e6,
        steps=len(st), step_wall_s=sum(s["w"] for s in st) / 1e9,
        batches=len(bs), positive_batches=len(posb),
        batch_wall_s=sum(batches[b]["w"] for b in bs) / 1e9,
        sum_of_slowest_run_s=sum(per_batch_max[b] for b in bs) / 1e9,
        sum_of_slowest_run_positive_s=sum(per_batch_max[b] for b in posb) / 1e9,
        sum_run_wall_s=sum(r["w"] for r in rs) / 1e9,
        sum_run_wall_positive_s=sum(r["w"] for r in rs if batches[r["b"]]["upto"] > batches[r["b"]]["now"]) / 1e9,
        sum_exec_s=sum(r["x"] for r in rs) / 1e9,
        runs=len(rs), zero_runs=sum(1 for r in rs if batches[r["b"]]["upto"] == batches[r["b"]]["now"]),
        frames_sent=sum(r["fr"] for r in rs),
        machines_per_positive_batch=(sum(batches[b]["n"] for b in posb) / len(posb)) if posb else 0,
        busy_fraction={str(m): sum(max(0, min(r["e"], z) - max(r["s"], a)) for r in runs if r["m"] == m) / max(1, min(z, end_sim) - a) for m in machines},
        frames_sent_by_machine={str(m): sum(r["fr"] for r in rs if r["m"] == m) for m in machines},
        run_wall_by_machine_s={str(m): sum(r["w"] for r in rs if r["m"] == m) / 1e9 for m in machines},
        positive_runs_by_machine={str(m): sum(1 for r in rs if r["m"] == m and r["e"] > r["s"]) for m in machines},
    )
if final_perf:
    out["perf_final"] = final_perf
json.dump(out, open(args.json, "w") if args.json else sys.stdout, indent=1)
print(file=sys.stderr)
