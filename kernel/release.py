#!/usr/bin/env python3
"""Identify the complete kernel recipe, independently of the Seedbox executable."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def release_id(root=ROOT):
    groups = json.loads((root / "kernel/inputs.json").read_text())
    names = sorted({name for group in groups.values() for name in group})
    digest = hashlib.sha256(b"seedbox-kernel-v1\0")
    for name in names:
        content = (root / name).read_bytes()
        digest.update(name.encode() + b"\0")
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


if __name__ == "__main__":
    print(release_id())
