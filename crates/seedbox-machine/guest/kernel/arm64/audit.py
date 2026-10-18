#!/usr/bin/env python3
"""Reject compiled hardware-counter/RNG reads; use GNU arm64 objdump."""
import argparse
from pathlib import Path
import re
import subprocess

FORBIDDEN = re.compile(r"\bmrs\b.*\b(?:cnt[vp]ct(?:ss)?_el0|cntfrq_el0|rndr(?:rs)?|s3_3_c14_c0_[1256]|s3_3_c2_c4_[01])\b|\b(?:wfit|wfet)\b", re.I)

def violations(text):
    return [line for line in text.splitlines() if FORBIDDEN.search(line)]

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("elf", nargs="+", type=Path)
    p.add_argument("--objdump", default="objdump")
    args = p.parse_args()
    failed = False
    for elf in args.elf:
        result = subprocess.run([args.objdump, "-d", str(elf)], check=True, capture_output=True, text=True)
        bad = violations(result.stdout)
        if bad:
            failed = True
            print(f"hardware-time/entropy leak in {elf}:")
            print("\n".join(bad))
        else: print(f"no hardware counter/RNG reads: {elf}")
    if failed: raise SystemExit(1)
if __name__ == "__main__": main()
