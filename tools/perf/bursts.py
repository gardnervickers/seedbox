#!/usr/bin/env python3
"""Burst-level whole-window analysis of a SEEDBOX-TRACE vmm.stderr (TCG).

Work proxy: simulated busy ns of positive runs (TCG icount: 1 ns per guest
instruction at the default work factor), so TCG's fixed per-run cost drops out.
Bursts: maximal contiguous busy intervals of one machine (positive runs with
e_i == s_(i+1)). 'smeared' apportions work to windows by simulated overlap (as
TCG ran it); 'lumpy' puts each burst's whole work in the window holding the
burst's start (as if the burst took ~no simulated time, the PV shape).
Usage: bursts.py STDERR A_NS Z_NS [label]
"""
import math, re, sys
from collections import defaultdict

path, A, Z = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
label = sys.argv[4] if len(sys.argv) > 4 else ""
KV = re.compile(r"(\w+)=(\S+)")
runs = []
for line in open(path, errors="replace"):
    if line.startswith("SEEDBOX-TRACE run "):
        d = dict(KV.findall(line))
        s, e, w = int(d["start"]), int(d["reached"]), int(d["wall"])
        runs.append((int(d["m"]), s, e, w, int(d["frames"])))
pos = [r for r in runs if r[2] > r[1]]
by_m = defaultdict(list)
for r in sorted(pos, key=lambda r: (r[0], r[1])):
    by_m[r[0]].append(r)
bursts = []  # (m, start, end, work_ns, wall_ns, runs)
for m, rs in by_m.items():
    cur = None
    for (_, s, e, w, fr) in rs:
        if cur and cur[2] == s:
            cur[2] = e; cur[3] += e - s; cur[4] += w; cur[5] += 1
        else:
            if cur:
                bursts.append(tuple(cur))
            cur = [m, s, e, e - s, w, 1]
    if cur:
        bursts.append(tuple(cur))
bursts = [b for b in bursts if A <= b[1] < Z]
machines = sorted(by_m)
tot = defaultdict(int)
for b in bursts:
    tot[b[0]] += b[3]
total = sum(tot.values())
busiest = max(tot.values())
span = Z - A
print(f"== {label} span={span/1e6:.1f} ms bursts={len(bursts)} work_total={total/1e6:.1f} ms busiest={busiest/1e6:.1f} ms")
print("   work by machine (ms):", {m: round(tot[m] / 1e6, 1) for m in machines},
      " share:", {m: round(tot[m] / total, 3) for m in machines})
nb = defaultdict(int)
for b in bursts:
    nb[b[0]] += 1
print("   bursts by machine:", dict(nb), " mean burst work (us):",
      {m: round(tot[m] / nb[m] / 1e3, 1) for m in machines},
      " bursts per ms:", round(len(bursts) / (span / 1e6), 2))

def window_stats(L, off, mode):
    W = defaultdict(lambda: defaultdict(float))
    for (m, s, e, work, wall, n) in bursts:
        if mode == "lumpy":
            W[math.floor((s - off) / L)][m] += work
        else:
            k0, k1 = math.floor((s - off) / L), math.floor((e - 1 - off) / L)
            for k in range(k0, k1 + 1):
                a, z = max(s, off + k * L), min(e, off + (k + 1) * L)
                W[k][m] += z - a
    ww = sum(max(c.values()) for c in W.values())
    occ = defaultdict(int)
    for c in W.values():
        occ[sum(1 for v in c.values() if v > 0)] += 1
    return ww, dict(sorted(occ.items())), len(W)

for L in (1e6, 0.5e6, 0.25e6):
    for off in (0.0, L / 2):
        for mode in ("smeared", "lumpy"):
            ww, occ, n = window_stats(L, off, mode)
            print(f"   L={L/1e6:g}ms off={off/1e6:g}ms {mode:7s}: sum_k max_m = {ww/1e6:8.1f} ms "
                  f"R=max/sum={ww/total:.3f} vs busiest x{ww/busiest:.3f} parallelism={total/ww:.2f} "
                  f"windows={n} occupancy={occ}")

# Today's Driver on PV-shaped bursts: only bursts that start at the same
# simulated time share a batch (followers woken by one broadcast); everything
# else runs in simulated-time order, one after another.
by_start = defaultdict(lambda: defaultdict(int))
for (m, s, e, work, wall, n) in bursts:
    by_start[s][m] += work
today = sum(max(c.values()) for c in by_start.values())
shared = sum(1 for c in by_start.values() if len(c) > 1)
print(f"   today-lumpy (same-start bursts overlap, rest serial): {today/1e6:.1f} ms parallelism={total/today:.2f} "
      f"start-times={len(by_start)} shared={shared}")
for L in (1e6,):
    for off in (0.0, L / 2):
        ww, occ, n = window_stats(L, off, "lumpy")
        print(f"   gain whole-window(lumpy, L=1ms off={off/1e6:g}) vs today-lumpy: x{today/ww:.2f} "
              f"({100*(1-ww/today):.0f}% less critical-path work)")
