#!/usr/bin/env python3
"""EXPERIMENT ONLY (#60 ordering spike). Summarize order-trace.jsonl files.
Usage: analyze.py RESULTS_DIR   (expects RUN/order-trace.jsonl and RUN.json per run)"""
import hashlib, json, sys
from pathlib import Path

r = Path(sys.argv[1])
runs = sorted(p.name for p in r.iterdir() if (p / 'order-trace.jsonl').exists())
traces = {}
for run in runs:
    raw = (r / run / 'order-trace.jsonl').read_bytes()
    lines = [json.loads(l) for l in raw.splitlines() if l.strip()]
    traces[run] = (hashlib.sha256(raw).hexdigest(), lines)
    rep = {}
    try: rep = json.loads((r / f'{run}.json').read_text())
    except Exception as e: print(f'{run}: no report ({e})')
    s = [l['summary'] for l in lines if 'summary' in l]
    print(f'== {run}: outcome={rep.get("outcome")} witness={str(rep.get("world_witness"))[:16]} trace_sha={traces[run][0][:16]} lines={len(lines)}')
    print('   summary:', json.dumps(s[-1] if s else None))
    for m in rep.get('machines', []):
        print(f'   {m.get("name")}: net={m.get("net")} exit={m.get("exit")}')
    bounds = [l['boundary'] for l in lines if 'boundary' in l]
    routes = [l['route'] for l in lines if 'route' in l]
    for b in bounds:
        T = b['at']
        print(f'   boundary {b["kind"]} at {T}: keys={b["keys"]} times={b["times"]} faults={[(f["id"], f["action"], f["status"]) for f in b.get("faults", [])]} handles={b.get("handles")}')
        # Last route stamped before T and first stamped at/after T, per sender.
        for name in sorted({x['from'] for x in routes}):
            mine = [x for x in routes if x['from'] == name]
            before = [x for x in mine if x['send'] < T]
            after = [x for x in mine if x['send'] >= T]
            for tag, x in (('last<T ', before[-1] if before else None), ('first>=T', after[0] if after else None)):
                if x:
                    dec = ', '.join(f'{d["to"]}:{d["decision"]}' + (f'@{d["arrival"]}' if 'arrival' in d else '') for d in x['to'])
                    print(f'      {name} {tag} send={x["send"]} (T{x["send"]-T:+}) clock={x["guest_clock"]} times={x["times"]} handles={x["handles"]} -> {dec}')
    # Routed after the frontier passed the next boundary, stamped before it.
    lag = [x for x in routes if x['next_boundary'] is not None and x['send'] < x['next_boundary'] <= max(x['times'])]
    print(f'   lagging-peer routes in windows: {len(lag)}')
    for x in lag[:3]:
        print(f'      {x["from"]} send={x["send"]} next={x["next_boundary"]} times={x["times"]} clock={x["guest_clock"]} -> {[d["to"]+":"+d["decision"] for d in x["to"]]}')
    viol = [x for x in routes if x['next_boundary'] is not None and x['send'] >= x['next_boundary']]
    print(f'   violations (send >= unapplied boundary): {len(viol)}')
    # Legacy scenario partition b->c: cut decisions not from a fault cut, by stamp.
    sc = [(x['send'], d) for x in routes if x['from'] == 'b' for d in x['to'] if d['to'] == 'c']
    on_at, off_at = 2750000000, 3250000000
    wrong = [s for s, d in sc if (d['decision'] == 'cut') != (on_at <= s < off_at)]
    print(f'   scenario b->c partition [{on_at}, {off_at}): {len(sc)} traced sends, {len(wrong)} decided against their stamp' + (f' (stamps {wrong[:6]})' if wrong else ''))
    slices = [l['slice'] for l in lines if 'slice' in l]
    near = [x for x in slices if any(abs(x['limit'] - b['at']) == 0 for b in bounds)]
    print(f'   slices ending at a boundary: {len(slices)}; stops: {sorted({x["stop"] for x in slices})}; end==limit for Sent: {sum(1 for x in slices if x["stop"]=="Sent" and x["end"]==x["limit"])}')
print()
by = {}
for run in runs:
    by.setdefault(run.rsplit('-', 1)[0], []).append(run)
for k, v in by.items():
    if len(v) == 2:
        a, b = v
        print(f'replay {a} vs {b}: trace {"IDENTICAL" if traces[a][0] == traces[b][0] else "DIFFERENT"}')
