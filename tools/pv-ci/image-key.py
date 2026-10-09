#!/usr/bin/env python3
"""Dependency cache key, never part of a Seedbox run or replay identity."""
import hashlib
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
FILES = ["flake.nix", "flake.lock", "Cargo.lock", "crates/seedbox-vmm/Cargo.lock",
         "crates/seedbox-arm64/Cargo.lock"]
DIRECTORIES = ["nix", "kernel", "guest", "tools/runtime"]


def image_key(root=ROOT):
    paths = [root / name for name in FILES]
    # Ignore kernel build products and other local caches, just as the flake does.
    names = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others",
                                     "--exclude-standard", "--", *DIRECTORIES], cwd=root)
    paths.extend(root / name.decode() for name in names.split(b"\0") if name)
    digest = hashlib.sha256()
    for path in sorted(set(paths)):
        digest.update(str(path.relative_to(root)).encode() + b"\0")
        digest.update(path.read_bytes())
        digest.update(b"\0")
    return digest.hexdigest()[:24]


if __name__ == "__main__":
    print(image_key(Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else ROOT))
