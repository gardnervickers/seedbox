"""PROBE ONLY (never merged): compare two runs' raw stopped-state evidence."""
import json, os, sys

a_dir, b_dir = sys.argv[1], sys.argv[2]
PAGE = 4096


def machines(d):
    return sorted(f[: -len(".probe-parts.json")] for f in os.listdir(d) if f.endswith(".probe-parts.json"))


def load(d, m):
    with open(os.path.join(d, m + ".probe-parts.json")) as f:
        return json.load(f)


def raw_map(parts):
    out = {}
    for name, value in parts["raw"]:
        out.setdefault(name, []).append(value)
    return out


def describe(name, x, y):
    if name == "xsave_raw":
        bx, by = bytes.fromhex(x), bytes.fromhex(y)
        offs = [i for i in range(min(len(bx), len(by))) if bx[i] != by[i]]
        print(f"    xsave lengths {len(bx)} {len(by)}; {len(offs)} differing bytes; header XSTATE_BV a={bx[512:520].hex()} b={by[512:520].hex()} XCOMP_BV a={bx[520:528].hex()} b={by[520:528].hex()}")
        runs = []
        for o in offs:
            if runs and o == runs[-1][1] + 1:
                runs[-1][1] = o
            else:
                runs.append([o, o])
        for s, e in runs[:40]:
            print(f"    xsave [{s}..{e}] a={bx[s:e+1].hex()} b={by[s:e+1].hex()}")
        return
    if name == "msrs":
        dx, dy = dict(map(tuple, x)), dict(map(tuple, y))
        for k in sorted(set(dx) | set(dy)):
            if dx.get(k) != dy.get(k):
                print(f"    msr {k:#x}: a={dx.get(k)!r} b={dy.get(k)!r}")
        return
    sx, sy = json.dumps(x, sort_keys=True), json.dumps(y, sort_keys=True)
    print(f"    a={sx[:1500]}")
    print(f"    b={sy[:1500]}")


def ram(m, regions):
    pa, pb = os.path.join(a_dir, m + ".probe-ram"), os.path.join(b_dir, m + ".probe-ram")
    if not (os.path.exists(pa) and os.path.exists(pb)):
        print("    RAM images missing")
        return
    # Map file offsets to guest physical addresses.
    spans, off = [], 0
    for base, length, shared in regions:
        spans.append((off, base, length))
        off += length
    def gpa(o):
        for start, base, length in spans:
            if start <= o < start + length:
                return base + (o - start)
        return None
    diff_pages = []
    with open(pa, "rb") as fa, open(pb, "rb") as fb:
        o = 0
        while True:
            x, y = fa.read(PAGE * 256), fb.read(PAGE * 256)
            if not x and not y:
                break
            for i in range(0, max(len(x), len(y)), PAGE):
                px, py = x[i:i + PAGE], y[i:i + PAGE]
                if px != py:
                    diff_pages.append((o + i, px, py))
            o += len(x)
    print(f"    RAM: {len(diff_pages)} differing 4 KiB pages")
    for fo, px, py in diff_pages[:12]:
        offs = [i for i in range(min(len(px), len(py))) if px[i] != py[i]]
        g = gpa(fo)
        print(f"    page gpa={g:#x} ({len(offs)} bytes differ)" if g is not None else f"    page off={fo:#x}")
        runs = []
        for q in offs:
            if runs and q <= runs[-1][1] + 8:
                runs[-1][1] = q
            else:
                runs.append([q, q])
        for s, e in runs[:6]:
            s0, e0 = max(0, s - 16) & ~15, min(PAGE, (e + 32) & ~15)
            print(f"      +{s:#05x}..{e:#05x}")
            print(f"        a[{s0:#05x}]={px[s0:e0].hex()}")
            print(f"        b[{s0:#05x}]={py[s0:e0].hex()}")


for m in sorted(set(machines(a_dir)) | set(machines(b_dir))):
    try:
        A, B = load(a_dir, m), load(b_dir, m)
    except FileNotFoundError:
        print(f"machine {m}: present in only one run")
        continue
    da, db = dict(map(tuple, A["digests"])), dict(map(tuple, B["digests"]))
    differing = [k for k in sorted(set(da) | set(db)) if da.get(k) != db.get(k)]
    print(f"machine {m}: differing parts {differing or 'none'}")
    try:
        with open(os.path.join(a_dir, m + ".probe-host.json")) as f:
            HA = json.load(f)
        with open(os.path.join(b_dir, m + ".probe-host.json")) as f:
            HB = json.load(f)
        ha, hb = dict(map(tuple, HA["parts"])), dict(map(tuple, HB["parts"]))
        print(f"  host-level differing parts {[k for k in sorted(set(ha) | set(hb)) if ha.get(k) != hb.get(k)] or 'none'}")
        if HA.get("pending") != HB.get("pending"):
            describe("pending", HA.get("pending"), HB.get("pending"))
    except FileNotFoundError:
        print("  host parts missing")
    ra, rb = raw_map(A), raw_map(B)
    for name in sorted(set(ra) | set(rb)):
        if ra.get(name) != rb.get(name):
            print(f"  raw {name} differs")
            for x, y in zip(ra.get(name, []), rb.get(name, [])):
                if x != y:
                    describe(name, x, y)
    if any(k.startswith("ram") for k in differing):
        ram(m, A["regions"])
