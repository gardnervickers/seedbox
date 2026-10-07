#!/usr/bin/env python3
"""EXPERIMENT ONLY (#60 link-policy controls). Usage: analyze.py RESULTS_DIR"""
import json, re, sys
from pathlib import Path

r = Path(sys.argv[1])
ok = True
def check(cond, what):
    global ok
    ok &= bool(cond)
    print(('PASS ' if cond else 'FAIL ') + what)

def report(run):
    try: return json.loads((r / f'{run}.json').read_text())
    except Exception as e: print(f'{run}: no report ({e})'); return {}

reps = {k: report(k) for k in ['L1', 'L2', 'L3', 'P1', 'M1']}
for k, v in reps.items():
    print(f'{k}: outcome={v.get("outcome")} witness={str(v.get("world_witness"))[:16]} network_witness={str(v.get("network_witness"))[:16]}')
w = {k: v.get('world_witness') for k, v in reps.items()}
check(w['L1'] and w['L1'] == w['L2'], 'shaped replay MATCH (L1 == L2)')
check(w['L1'] and w['L1'] == w['L3'], 'shaped run with undo off MATCH (L1 == L3)')
check(w['P1'] and w['P1'] == w['M1'], 'plain world: PR engine witness == main engine witness')
check(all(v.get('outcome') == 'end_machine_exited' for v in reps.values()), 'every run ended on a')
nc = reps['L1'].get('network_counters', {})
print('L1 counters:', json.dumps(nc, sort_keys=True))
check(nc.get('c->a', {}).get('loss_dropped', 0) > 0 and nc.get('c->b', {}).get('loss_dropped', 0) > 0, 'loss_dropped on c->a and c->b')
check(nc.get('c->a', {}).get('partition_dropped', 0) > 0, 'cut over a lossy shape counted as partition_dropped')
check(all('loss_dropped' not in v for k, v in nc.items() if k not in ('c->a', 'c->b')), 'no loss elsewhere')
check(not any('loss_dropped' in v for v in reps['P1'].get('network_counters', {}).values()), 'plain run has no loss_dropped keys')
check([f['status'] for f in reps['L1'].get('faults', [])] == ['applied'] * 16, 'all 16 shape/heal/partition faults applied')

wins = {}
reorders = {}
for line in (r / 'L1').glob('*.console'):
    for l in line.read_text(errors='replace').splitlines():
        m = re.search(r'LINK-WIN (\w)<-(\w) w=(\d+) n=(\d+) min=(\d+) max=(\d+) mean=(\d+)', l)
        if m:
            dst, src, wi, n, lo, hi, mean = m.groups()
            wins[(dst, src, int(wi))] = (int(n), int(lo) / 1e6, int(hi) / 1e6, int(mean) / 1e6)
        m = re.search(r'LINK-SRC (\w)<-(\w) reorders=(\d+)', l)
        if m: reorders[(m.group(1), m.group(2))] = int(m.group(3))
check(len(reorders) == 6 and all(v == 0 for v in reorders.values()), f'no out-of-order receipts {reorders}')
# Expected (latency ms or None for a FIFO transition window, fraction received) per window 20..49.
def plan(spec):
    out = {}
    for lo, hi, lat, frac in spec:
        for wi in range(lo, hi + 1): out[wi] = (lat, frac)
    return out
expect = {
    ('b', 'a'): plan([(20, 49, 3, 1)]),
    ('a', 'b'): plan([(20, 49, 1, 1)]),
    ('c', 'a'): plan([(20, 24, 1, 1), (25, 29, 5, 1), (30, 39, 8, 1), (40, 40, None, 1), (41, 49, 1, 1)]),
    ('c', 'b'): plan([(20, 24, 1, 1), (25, 29, 5, 1), (30, 34, 8, 1), (35, 35, None, 1), (36, 39, 5, 1), (40, 40, None, 1), (41, 49, 1, 1)]),
    ('a', 'c'): plan([(20, 39, 1, 1), (40, 42, 1.25, .5), (43, 44, None, 0), (45, 46, 1.25, .5), (47, 47, None, 1), (48, 49, 1, 1)]),
    ('b', 'c'): plan([(20, 39, 1, 1), (40, 46, None, 0), (47, 49, 1, 1)]),
}
for (dst, src), spec in expect.items():
    full = max((wins.get((dst, src, wi), (0,))[0] for wi in range(20, 50)), default=0)
    bad = []
    for wi, (lat, frac) in spec.items():
        n, lo, hi, mean = wins.get((dst, src, wi), (0, 0, 0, 0))
        f = n / full if full else 0
        good = abs(f - frac) <= (0.1 if frac == .5 else 0.02)
        if lat is not None and n:
            tol = 0.3 if lat == 1.25 else 0.2   # mid-loss shape adds 0..0.5 ms jitter
            good &= lat - (0.25 if lat == 1.25 else 0) <= mean <= lat + tol
        if not good: bad.append((wi, n, round(f, 2), round(mean, 3), round(hi, 3)))
    check(not bad, f'{dst}<-{src} windows match plan' + (f'; mismatches (w, n, frac, mean_ms, max_ms): {bad}' if bad else ''))
for key in [('c', 'b', 35), ('c', 'a', 40), ('c', 'b', 40)]:
    n, lo, hi, mean = wins.get(key, (0, 0, 0, 0))
    print(f'FIFO window {key[0]}<-{key[1]} w={key[2]}: n={n} min={lo:.3f} max={hi:.3f} mean={mean:.3f} ms')
print('\nRESULT:', 'ALL PASS' if ok else 'SOME CHECKS FAILED')
