#!/usr/bin/env python3
"""Directed-link host controls. Usage: links.py RESULTS_DIR.

Exact engine arrival bounds come from routed-frame records (network.jsonl), not
LINK-WIN receipt times. Each JSON line has src, dst, send_ns and arrival_ns;
records must include every queued frame in routing order, including boot traffic.
The VMM's --trace has no network records, so on the host the file is usually
absent: the exact bounds are then covered portably by
crates/seedbox/tests/link_worlds.rs, which drives the real links.rs and faults.rs
through shaped.json. A file that is present must still satisfy the bounds.
"""
import json
import re
import sys
from pathlib import Path

WIN = 100_000_000
RUNS = ('L1', 'L2', 'P1', 'M1', 'U1', 'U2')


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
    n, lo, hi, mean = observed  # milliseconds from the guest probe
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
            good &= (floor <= lo <= floor + .05 and center - .1 <= mean <= center + .15
                     and hi <= floor + jitter / 1e6 + .2)
        else:
            good &= floor <= lo <= floor + .05 and mean <= floor + .1
    # Fixed-delay receipt tails remain diagnostic; jittered windows keep only
    # a loose 200 us sanity allowance. Exact bounds use routed engine arrivals.
    return good


def arrival_bounds(world, records):
    """Check actual queued arrivals: max(send + latency + jitter, link_last + 1).

    Loss/cut frames reserve no FIFO slot. This fixture has no lifecycle faults,
    so every routed delivery reserves its arrival even if the NIC later drops it.
    Do not sort: routing order is part of the evidence.
    """
    names = {m['name'] for m in world['machines']}
    last = {}
    previous_send = {}
    counts = {}
    for index, record in enumerate(records):
        src, dst = record['src'], record['dst']
        send, arrival = record['send_ns'], record['arrival_ns']
        pair = (src, dst)
        if (src not in names or dst not in names or src == dst or
                type(send) is not int or type(arrival) is not int or
                send < previous_send.get(pair, -1) or send < 0):
            raise ValueError(f'route {index}: invalid pair, time or routing order')
        previous_send[pair] = send
        policy = policy_at(world, src, dst, send)
        if policy is None or policy[2] == 1_000_000:
            raise ValueError(f'route {index}: arrival on a cut or 100% loss link')
        latency, jitter, _ = policy
        fifo = last.get(pair, 0) + 1
        lower = max(send + latency, fifo)
        upper = max(send + latency + jitter, fifo)
        if not lower <= arrival <= upper:
            raise ValueError(f'route {index} {src}->{dst}: arrival {arrival} outside '
                             f'[{lower}, {upper}] (send={send}, link_last={last.get(pair, 0)})')
        last[pair] = arrival
        key = (src, dst, send // WIN)
        counts[key] = counts.get(key, 0) + 1
    if not counts:
        raise ValueError('no routed arrivals')
    return counts


def selected_runs(runs):
    runs = tuple(runs)
    if not runs or len(set(runs)) != len(runs) or any(r not in RUNS for r in runs):
        raise ValueError('select unique lanes from ' + ' '.join(RUNS))
    return runs


def analyze(directory, runs=RUNS):
    runs = selected_runs(runs)
    r = Path(directory)
    ok = True
    def check(cond, what):
        nonlocal ok
        ok &= bool(cond)
        print(('PASS ' if cond else 'FAIL ') + what)

    def report(run):
        try: return json.loads((r / f'{run}.json').read_text())
        except Exception as e: print(f'{run}: no report ({e})'); return {}

    reps = {k: report(k) for k in runs}
    for k, v in reps.items():
        stops = sum(m.get('precise_stops', 0) for m in v.get('machines', []))
        steps = sum(m.get('steps', 0) for m in v.get('machines', []))
        rollbacks = sum((m.get('undo') or {}).get('rollbacks', 0) for m in v.get('machines', []))
        print(f'{k}: outcome={v.get("outcome")} witness={str(v.get("world_witness"))[:16]} '
              f'network_witness={str(v.get("network_witness"))[:16]} host_s={v.get("host_seconds", 0):.0f} '
              f'precise_stops={stops} steps={steps} rollbacks={rollbacks}')
        if k == 'U1':
            check(rollbacks > 0, f'U1: exercised rollback (rollbacks={rollbacks})')
    w = {k: v.get('world_witness') for k, v in reps.items()}
    for a, b, label in [('L1', 'L2', 'shaped replay'),
                        ('U1', 'U2', 'short shaped undo on/off'),
                        ('P1', 'M1', 'plain PR/main engine')]:
        if a in reps and b in reps:
            check(w[a] and w[a] == w[b], f'{label} MATCH ({a} == {b})')
        else:
            print(f'SKIP {label} comparison: requires {a} and {b}')
    check(all(v.get('outcome') == 'end_machine_exited' for v in reps.values()), 'every selected run ended on a')
    for run in runs:
        nc = reps[run].get('network_counters', {})
        print(f'{run} counters:', json.dumps(nc, sort_keys=True))
        if run in ('P1', 'M1'):
            check(not any('loss_dropped' in v for v in nc.values()),
                  f'{run}: plain run has no loss_dropped keys')
            continue
        check(nc.get('c->a', {}).get('loss_dropped', 0) > 0 and nc.get('c->b', {}).get('loss_dropped', 0) > 0,
              f'{run}: loss_dropped on c->a and c->b')
        check(nc.get('c->a', {}).get('partition_dropped', 0) > 0 and nc.get('a->c', {}).get('partition_dropped', 0) > 0,
              f'{run}: cut c|a counted both ways')
        check(all('loss_dropped' not in v for k, v in nc.items() if k not in ('c->a', 'c->b')), f'{run}: no loss elsewhere')
        check(all(v.get('partition_dropped', 0) == 0 for k, v in nc.items() if k not in ('c->a', 'a->c')), f'{run}: no cuts elsewhere')
    world = json.loads((r / 'shaped.json').read_text())
    short = json.loads((r / 'shaped-tol-short.json').read_text())
    for run in (r for r in runs if r[0] in 'LU'):
        planned = (short if run.startswith('U') else world)['faults']
        faults = reps[run].get('faults', [])
        check([f.get('id') for f in faults] == list(range(1, 17)) and
              [f.get('status') for f in faults] == ['applied'] * 16 and
              [f.get('applied_at_ns') for f in faults] == [f['at_ns'] for f in planned],
              f'{run}: all 16 faults applied in order')

    for run in runs:
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
        if run not in ('L1', 'L2'):
            continue
        routed = r / run / 'network.jsonl'
        if not routed.exists():
            print(f'SKIP {run}: exact latency/jitter/FIFO bounds covered by link_worlds.rs '
                  '(no routed network.jsonl; the VMM trace has no network records)')
        else:
            try:
                with routed.open() as trace:
                    counts = arrival_bounds(world, (json.loads(line) for line in trace))
                # Require observations in the disputed routed-time window; never let
                # an empty/unrelated trace stand in for engine evidence.
                check(counts.get(('c', 'a', 45), 0) > 0,
                      f'{run}: routed c->a arrivals present in window 45')
                check(True, f'{run}: exact latency/jitter/FIFO bounds from routed arrivals')
            except (OSError, ValueError, KeyError, TypeError) as error:
                check(False, f'{run}: exact routed-arrival evidence: {error}')
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
    try:
        sys.exit(analyze(sys.argv[1], sys.argv[2:] or RUNS))
    except (IndexError, ValueError) as error:
        print(f'error: {error}; usage: links.py RESULTS_DIR [LANE ...]', file=sys.stderr)
        sys.exit(2)
