#!/usr/bin/env python3
"""Compare witnesses without hiding divergent exits, console, or RAM."""
import json
import sys
from itertools import zip_longest

def main():
    if len(sys.argv) != 3: raise SystemExit(f"usage: {sys.argv[0]} LEFT.json RIGHT.json")
    with open(sys.argv[1]) as f: a = json.load(f)
    with open(sys.argv[2]) as f: b = json.load(f)
    for key in ["arch", "timebase", "format", "seed", "kernel_sha256", "initrd_sha256"]:
        if a[key] != b[key]: raise SystemExit(f"NOT_COMPARABLE: {key}: {a[key]} != {b[key]}")
    if a["witness"] == b["witness"]:
        print("MATCH:", a["witness"]); return
    for i, (x, y) in enumerate(zip_longest(a["trace"], b["trace"])):
        if x != y:
            print(f"first divergent exit {i}:\nleft:  {x}\nright: {y}"); break
    else: print("all recorded exits match")
    for key in ["trace_hash", "console_hash", "ram_hash", "witness"]:
        print(f"{key}: {a[key]} / {b[key]}")
    if a["console"] != b["console"]:
        for i, (x, y) in enumerate(zip_longest(a["console"], b["console"])):
            if x != y:
                print(f"first differing console character {i}: {x!r} / {y!r}"); break
    raise SystemExit(1)
if __name__ == "__main__": main()
