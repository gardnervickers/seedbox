#!/usr/bin/env python3
"""Reject compiled TSC reads in the x86 kernel; report CPUID-gated hardware reads.

RDTSC/RDTSCP cannot be hidden from kernel mode, so none may remain. RDRAND,
RDSEED, RDPMC, MONITOR/MWAIT(X) and TPAUSE/UMWAIT are only reached behind CPUID
feature checks; the seedbox-machine x86 worker hides those features.
"""
import argparse
from pathlib import Path
import re
import subprocess

FORBIDDEN = re.compile(r"\t(?:rdtscp?|rdpid)\b", re.I)
GATED = re.compile(r"\t(?:rdrand|rdseed|rdpmc|monitorx?|mwaitx?|tpause|umwait)\b", re.I)


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
        else:
            print(f"no TSC reads: {elf}")
        gated = sorted({line.split("\t")[-1].split()[0] for line in result.stdout.splitlines() if GATED.search(line)})
        print(f"CPUID-gated instructions (hidden by the worker): {', '.join(gated) or 'none'}")
    if failed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
