#!/usr/bin/env python3
"""Summarize the diagnostic `spin` section of seedbox-vmm reports.

Usage: summary.py REPORT.json [...]
"""
import json
import sys


def main(paths):
    for path in paths:
        report = json.load(open(path))
        for m in report.get('machines', report.get('report', [])):
            spin = m.get('spin')
            if spin is None:
                continue
            print(f"== {path} {m['name']} branches={m['branches']}")
            for h in spin['hist_log2']:
                print(f"  2^{h['log2']:<2} stretches={h['count']:<8} branches={h['branches']}")
            inc = spin['incidents']
            print(f"  incidents>={spin['log_min']}: {len(inc)} (+{spin['incidents_dropped']} dropped)")
            for i in sorted(inc, key=lambda i: -i['length'])[:20]:
                rips = sorted({s['rip'] for s in i['samples']})
                code = i['samples'][-1]['code'] if i['samples'] else ''
                print(f"  len={i['length']} end={i['ended_by']} irqs={i['irqs_inside']} "
                      f"cpl={[s['cpl'] for s in i['samples']][-1:]} cr3 {i['cr3_start']}->{i['cr3_end']} "
                      f"span={i['rip_span']} repeat={i['repeat']} rips={rips[:6]} code={code}")


if __name__ == '__main__':
    main(sys.argv[1:])
