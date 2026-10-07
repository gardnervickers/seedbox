#!/usr/bin/env python3
"""Check guest inputs against independently reviewed hashes, never SHA256SUMS."""
import argparse
import hashlib
from pathlib import Path
import re


def check(directory, expected):
    entries = [line.split() for line in expected.read_text().splitlines()
               if line.strip() and not line.lstrip().startswith("#")]
    if (len(entries) != 2 or any(len(row) != 2 for row in entries)
            or {row[1] for row in entries} != {"Image", "init.cpio"}
            or any(not re.fullmatch(r"[0-9a-f]{64}", row[0]) for row in entries)):
        raise ValueError(f"{expected}: operator must fill reviewed Image and init.cpio SHA-256 values after the rebuild and acceptance")
    for digest, name in entries:
        actual = hashlib.sha256((directory / name).read_bytes()).hexdigest()
        if actual != digest:
            raise ValueError(f"{name}: expected {digest}, got {actual}; different source or toolchain inputs can change the witness; do not overwrite expected hashes to silence this check")
        print(f"{name}: OK")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--expected", type=Path,
                        default=Path(__file__).with_name("expected-hashes.sha256"))
    args = parser.parse_args()
    try:
        check(args.directory, args.expected)
    except (OSError, ValueError) as error:
        parser.exit(2, f"error: {error}\n")


if __name__ == "__main__":
    main()
