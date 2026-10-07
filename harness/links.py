#!/usr/bin/env python3
"""Directed-link host controls. Usage: links.py RESULTS_DIR.

The nominal arrival bound is checked against production links.rs portably in
crates/seedbox/tests/link_worlds.rs. LINK-WIN instead observes userspace receipt:
its maximum includes sendto, guest network processing and time until recv drains
that packet. No fixed 100 us bound on those costs exists in this probe.
"""
import json
import re
import sys
from pathlib import Path

WIN = 100_000_000


def policy_at(world, src, dst, at):
    """Send-time precedence: cut, highest live shape ID, baseline, global."""
    live = {}
    for fault in sorted(world['faults'], key=lambda f: (f['at_ns'], f['id'])):
        if fault['at_ns'] > at:
            break
        if fault['action'] == 'heal':
            del live[fault['handle']]
        else:
            live[fault['handle']] = fault
    for fault in live.values():
        if fault['action'] == 'partition' and (
                src in fault['left'] and dst in fault['right'] or
                dst in fault['left'] and src in fault['right']):
            return None
    candidates = [f for f in live.values() if f['action'] == 'shape' and
                  src in f['from'] and dst in f['to']]
    if candidates:
        policy = max(candidates, key=lambda f: f['id'])
    else:
        policy = next((p for p in world['network'].get('links', [])
                       if p['from'] == src and p['to'] == dst), None)
        if policy is None:
            return (world['network']['latency_ns'],
                    world['network'].get('jitter_ns', 0), 0)
    return (policy['latency_ns'], policy.get('jitter_ns', 0), policy.get('loss_ppm', 0))


def window_plan(world):
    """Full-world 100 ms sender-stamp windows; never guess a mixed window."""
    names = [m['name'] for m in world['machines']]
    out = {}
    for dst in names:
        for src in names:
            if src == dst:
                continue
            spec = {}
            for wi in range(20, 50):
                at = wi * WIN
                if any(at < f['at_ns'] < at + WIN for f in world['faults']):
                    raise ValueError(f'fault inside window {wi}; split the window')
                policy = policy_at(world, src, dst, at)
                before = policy_at(world, src, dst, at - 1)
                if policy is None:
                    spec[wi] = (None, 0, 0)
                else:
                    lat, jitter, loss = policy
                    # A delay decrease can leave a FIFO tail from the old policy.
                    transition = before is not None and before[0] + before[1] > lat + jitter
                    spec[wi] = (None if transition else lat, jitter, 1 - loss / 1_000_000)
            out[(dst, src)] = spec
    return out


def window_matches(expected, observed, sent):
    lat, jitter, frac = expected
    n, lo, _hi, mean = observed  # milliseconds from the guest probe
    if frac == .5:  # unchanged binomial, four sigma
        good = sent > 0 and abs(n - sent / 2) <= 4 * (sent / 4) ** .5 + 1
    elif frac == 1:  # a pre-edge stamp can be routed after the edge
        good = sent > 0 and sent - max(2, sent // 100) <= n <= sent
    else:
        good = sent > 0 and n <= max(2, sent // 100)
    if lat is not None and n:
        floor = lat / 1e6
        if jitter:
            center = floor + jitter / 2e6
            good &= floor <= lo <= floor + .05 and center - .1 <= mean <= center + .15
        else:
            good &= floor <= lo <= floor + .05 and mean <= floor + .1
    # hi is diagnostic, not a nominal-arrival measurement. The strict floor,
    # mean, count and FIFO checks remain; exact arrival bounds live in Rust.
    return good


def analyze(directory):
    r = Path(directory)
    ok = True
    def check(cond, what):
        nonlocal ok
        ok &= bool(cond)
        print(('PASS ' if cond else 'FAIL ') + what)

    def report(run):
        try: return json.loads((r / f'{run}.json').read_text())
        except Exception as e: print(f'{run}: no report ({e})'); return {}

    RUNS = ['L1', 'L2', 'P1', 'M1', 'U1', 'U2']
    reps = {k: report(k) for k in RUNS}
    for k, v in reps.items():
        stops = sum(m.get('precise_stops', 0) for m in v.get('machines', []))
        steps = sum(m.get('steps', 0) for m in v.get('machines', []))
        print(f'{k}: outcome={v.get("outcome")} witness={str(v.get("world_witness"))[:16]} '
              f'network_witness={str(v.get("network_witness"))[:16]} host_s={v.get("host_seconds", 0):.0f} '
              f'precise_stops={stops} steps={steps}')
    w = {k: v.get('world_witness') for k, v in reps.items()}
    check(w['L1'] and w['L1'] == w['L2'], 'shaped replay MATCH (L1 == L2)')
    check(w['U1'] and w['U1'] == w['U2'], 'shaped world with a stop tolerance: undo off MATCH (U1 == U2)')
    check(w['P1'] and w['P1'] == w['M1'], 'plain world: PR engine witness == main engine witness')
    check(all(v.get('outcome') == 'end_machine_exited' for v in reps.values()), 'every run ended on a')
    nc = reps['L1'].get('network_counters', {})
    print('L1 counters:', json.dumps(nc, sort_keys=True))
    check(nc.get('c->a', {}).get('loss_dropped', 0) > 0 and nc.get('c->b', {}).get('loss_dropped', 0) > 0, 'loss_dropped on c->a and c->b')
    check(nc.get('c->a', {}).get('partition_dropped', 0) > 0 and nc.get('a->c', {}).get('partition_dropped', 0) > 0, 'cut c|a (over a lossy shape on c->a) counted as partition_dropped both ways')
    check(all('loss_dropped' not in v for k, v in nc.items() if k not in ('c->a', 'c->b')), 'no loss elsewhere')
    check(all(v.get('partition_dropped', 0) == 0 for k, v in nc.items() if k not in ('c->a', 'a->c')), 'no cuts elsewhere')
    for run in ('P1', 'M1'):
        check(not any('loss_dropped' in v for v in reps[run].get('network_counters', {}).values()),
              f'{run}: plain run has no loss_dropped keys')
    world = json.loads((r / 'shaped.json').read_text())
    short = json.loads((r / 'shaped-tol-short.json').read_text())
    for run in ('L1', 'L2', 'U1', 'U2'):
        planned = (short if run.startswith('U') else world)['faults']
        faults = reps[run].get('faults', [])
        check([f.get('id') for f in faults] == list(range(1, 17)) and
              [f.get('status') for f in faults] == ['applied'] * 16 and
              [f.get('applied_at_ns') for f in faults] == [f['at_ns'] for f in planned],
              f'{run}: all 16 faults applied in order')

    for run in RUNS:
        wins = {}
        reorders = {}
        sent = {}
        for line in (r / run).glob('*.console'):
            for l in line.read_text(errors='replace').splitlines():
                m = re.search(r'LINK-WIN (\w)<-(\w) w=(\d+) n=(\d+) min=(\d+) max=(\d+) mean=(\d+)', l)
                if m:
                    dst, src, wi, n, lo, hi, mean = m.groups()
                    wins[(dst, src, int(wi))] = (int(n), int(lo) / 1e6, int(hi) / 1e6, int(mean) / 1e6)
                m = re.search(r'LINK-SENT (\w) w=(\d+) n=(\d+)', l)
                if m: sent[(m.group(1), int(m.group(2)))] = int(m.group(3))
                m = re.search(r'LINK-SRC (\w)<-(\w) reorders=(\d+)', l)
                if m: reorders[(m.group(1), m.group(2))] = int(m.group(3))
        check(len(reorders) == 6 and all(v == 0 for v in reorders.values()), f'{run}: no out-of-order receipts {reorders}')
        if run != 'L1':
            continue
        for (dst, src), spec in window_plan(world).items():
            bad = []
            for wi, expected in spec.items():
                if wi == 49:  # probe stops draining at 5.0 s; final arrivals truncate
                    continue
                observed = wins.get((dst, src, wi), (0, 0, 0, 0))
                count = sent.get((src, wi), 0)
                if not window_matches(expected, observed, count):
                    n, lo, hi, mean = observed
                    bad.append((wi, n, count, round(n / count, 2) if count else -1,
                                round(lo, 3), round(mean, 3), round(hi, 3)))
            check(not bad, f'{dst}<-{src} windows match plan' +
                  (f'; mismatches (w, n, sent, frac, min_ms, mean_ms, max_ms): {bad}' if bad else ''))
        for key in [('c', 'b', 35), ('c', 'a', 40), ('c', 'b', 40), ('a', 'c', 45)]:
            n, lo, hi, mean = wins.get(key, (0, 0, 0, 0))
            print(f'receipt window {key[0]}<-{key[1]} w={key[2]}: n={n} '
                  f'min={lo:.3f} max={hi:.3f} mean={mean:.3f} ms (userspace receipt)')
    print('\nRESULT:', 'ALL PASS' if ok else 'SOME CHECKS FAILED')
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(analyze(sys.argv[1]))
