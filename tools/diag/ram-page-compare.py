#!/usr/bin/env python3
"""Compare sparse guest RAM dumps (<run>/output/machines/<machine>.ram) of two or
three runs page by page and describe every page that differs.

usage: ram-page-compare.py OUTDIR LABEL=RUNDIR LABEL=RUNDIR [LABEL=RUNDIR]
"""
import mmap, os, re, sys, string

PAGE = 4096

def pages(path):
    f = open(path, 'rb')
    m = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
    return m

def classify(page):
    zeros = page.count(0)
    kptr = len(re.findall(rb'[\x00-\xff]{5}\x80?\xff\xff\xff', page))
    text = page.hex()
    kern = text.count('ffffffff8') + text.count('ffff8880') + text.count('ffff8881') + text.count('ffff888')
    user = len(re.findall(rb'\x7f\x00\x00', page)) + len(re.findall(rb'\x00\x7f\x00\x00', page))
    printable = set(bytes(string.printable, 'ascii'))
    runs = re.findall(rb'[\x20-\x7e]{6,}', page)
    return (f"zero bytes {zeros}/{PAGE}; kernel-pointer-like words {kern}; "
            f"user-pointer-like (0x7f..) {user}; ascii runs {len(runs)}: "
            + ', '.join(r.decode('ascii', 'replace')[:40] for r in runs[:12]))

def hexline(b):
    return ' '.join(f'{x:02x}' for x in b)

def compare(out, runs):
    labels = [l for l, _ in runs]
    machines = sorted({f[:-4] for _, d in runs for f in os.listdir(os.path.join(d, 'output', 'machines')) if f.endswith('.ram')})
    report = []
    for machine in machines:
        maps = {}
        for label, d in runs:
            p = os.path.join(d, 'output', 'machines', machine + '.ram')
            if os.path.exists(p):
                maps[label] = pages(p)
        if len(maps) < 2:
            report.append(f'{machine}: only {list(maps)} have a RAM dump')
            continue
        size = min(len(m) for m in maps.values())
        n = size // PAGE
        differing = []
        for pfn in range(n):
            lo, hi = pfn * PAGE, (pfn + 1) * PAGE
            vals = {l: m[lo:hi] for l, m in maps.items()}
            first = next(iter(vals.values()))
            if any(v != first for v in vals.values()):
                differing.append((pfn, vals))
        report.append(f'== {machine}: {n} pages compared across {sorted(maps)}; {len(differing)} differ')
        for pfn, vals in differing[:32]:
            report.append(f'-- {machine} pfn {pfn} (gpa {pfn*PAGE:#x})')
            ls = sorted(vals)
            for a in range(len(ls)):
                for b in range(a + 1, len(ls)):
                    x, y = vals[ls[a]], vals[ls[b]]
                    nd = sum(1 for i in range(PAGE) if x[i] != y[i])
                    report.append(f'   {ls[a]} vs {ls[b]}: {nd} bytes differ')
            for l in ls:
                report.append(f'   {l}: {classify(vals[l])}')
                with open(os.path.join(out, f'{machine}-{pfn}-{l}.bin'), 'wb') as f:
                    f.write(vals[l])
            shown = 0
            for off in range(0, PAGE, 16):
                lines = {l: vals[l][off:off+16] for l in ls}
                if len(set(lines.values())) > 1:
                    report.append(f'   +{off:04x}: ' + ' | '.join(f'{l} {hexline(lines[l])}' for l in ls))
                    shown += 1
                    if shown >= 48:
                        report.append('   ... (more differing lines omitted)')
                        break
    text = '\n'.join(report)
    print(text)
    with open(os.path.join(out, 'pages-report.txt'), 'w') as f:
        f.write(text + '\n')

if __name__ == '__main__':
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    runs = [tuple(a.split('=', 1)) for a in sys.argv[2:]]
    compare(out, runs)
