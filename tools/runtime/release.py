#!/usr/bin/env python3
"""Identify guest helper and filesystem-tool inputs independently of the kernel."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def release_id(root=ROOT):
    groups = json.loads((root / "tools/runtime/inputs.json").read_text())
    names = sorted({name for group in groups.values() for name in group})
    digest = hashlib.sha256(b"seedbox-runtime-v1\0")
    for name in names:
        content = (root / name).read_bytes()
        digest.update(name.encode() + b"\0")
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return digest.hexdigest()


if __name__ == "__main__":
    print(release_id())
