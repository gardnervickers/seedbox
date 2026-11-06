#!/usr/bin/env python3
"""Today's lockstep critical path vs whole-window critical path, on the TCG
timeline and on PV-shaped timelines (each burst keeps its start and work but
lasts c x its TCG duration in simulated time).

Today's Driver (unlimited workers, zero host overhead) runs machines in
simulated-time lockstep, so its critical path is the integral over simulated
time of the largest per-machine work rate: sum over elementary segments of
max_m(rate_m x length). Whole-window: sum over windows of max_m(work in window).
Usage: sweep.py STDERR A_NS Z_NS [label]
"""
import math, re, sys
from collections import defaultdict

path, A, Z = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
label = sys.argv[4] if len(sys.argv) > 4 else ""
KV = re.compile(r"(\w+)=(\S+)")
pos = []
for line in open(path, errors="replace"):
    if line.startswith("SEEDBOX-TRACE run "):
        d = dict(KV.findall(line))
        s, e = int(d["start"]), int(d["reached"])
        if e > s:
            pos.append((int(d["m"]), s, e))
by_m = defaultdict(list)
for r in sorted(pos, key=lambda r: (r[0], r[1])):
    by_m[r[0]].append(r)
bursts = []
for m, rs in by_m.items():
    cur = None
    for (_, s, e) in rs:
        if cur and cur[2] == s:
            cur[2] = e
        else:
            if cur:
                bursts.append(tuple(cur))
            cur = [m, s, e]
    if cur:
        bursts.append(tuple(cur))
bursts = [b for b in bursts if A <= b[1] < Z]
work = {}
total = 0
per_m = defaultdict(int)
for (m, s, e) in bursts:
    per_m[m] += e - s
    total += e - s
busiest = max(per_m.values())

def shaped(c):
    # (m, start, end, work) with duration scaled by c (at least 1 ns)
    return [(m, s, s + max(1, int(round((e - s) * c))), e - s) for (m, s, e) in bursts]

def lockstep(bs):
    pts = sorted({b[1] for b in bs} | {b[2] for b in bs})
    idx = {t: i for i, t in enumerate(pts)}
    # per segment max rate x length: accumulate with a difference array per machine
    seg_rate = defaultdict(lambda: defaultdict(float))
    # Work rate of a burst is uniform over its duration.
    events = defaultdict(list)
    for (m, s, e, w) in bs:
        events[s].append((m, w / (e - s)))
        events[e].append((m, -w / (e - s)))
    rate = defaultdict(float)
    cp = 0.0
    prev = None
    for t in pts:
        if prev is not None and rate:
            cp += max(rate.values()) * (t - prev)
        for (m, dr) in events[t]:
            rate[m] += dr
            if abs(rate[m]) < 1e-9:
                del rate[m]
        prev = t
    return cp

def windows(bs, L, off):
    W = defaultdict(lambda: defaultdict(float))
    for (m, s, e, w) in bs:
        k0, k1 = math.floor((s - off) / L), math.floor((e - 1 - off) / L)
        for k in range(k0, k1 + 1):
            a, z = max(s, off + k * L), min(e, off + (k + 1) * L)
            W[k][m] += w * (z - a) / (e - s)
    return sum(max(c.values()) for c in W.values())

print(f"== {label}: bursts={len(bursts)} total work={total/1e6:.1f} ms busiest machine={busiest/1e6:.1f} ms "
      f"span={(Z-A)/1e6:.1f} ms")
for c in (1.0, 0.25, 0.1, 0.065, 0.02):
    bs = shaped(c)
    busy = {m: sum(b[2] - b[1] for b in bs if b[0] == m) / (Z - A) for m in sorted(per_m)}
    today = lockstep(bs)
    ww0 = windows(bs, 1e6, 0.0)
    ww5 = windows(bs, 1e6, 0.5e6)
    print(f"  c={c:<6g} busy={ {m: round(v, 3) for m, v in busy.items()} } today(lockstep)={today/1e6:7.1f} ms "
          f"(par {total/today:.2f})  window1ms={ww0/1e6:7.1f}/{ww5/1e6:.1f} ms (par {total/ww0:.2f})  "
          f"gain x{today/ww0:.2f} ({100*(1-ww0/today):.0f}% less)  window/busiest x{ww0/busiest:.2f}")
