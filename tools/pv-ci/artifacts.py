#!/usr/bin/env python3
"""Keep bounded diagnostics; full VM state needs an explicit separate export."""
from pathlib import Path

LIMIT = 64 * 1024 * 1024
PER_FILE = 8 * 1024 * 1024


def collect(root):
    output = root / "target/pv-artifacts"
    output.mkdir(parents=True, exist_ok=False)
    remaining, count = LIMIT - 4096, 0
    for source in (root / "target/check-reports", root / "target/pv-runner-diagnostics",
                   Path("/var/lib/seedbox-runner/_diag")):
        if source.is_symlink() or not source.is_dir():
            continue
        for path in sorted(source.rglob("*")):
            if count >= 128 or remaining <= 0:
                break
            if path.is_symlink() or not path.is_file() or path.suffix not in (
                    ".json", ".txt", ".log", ".out", ".stderr", ".stdout"):
                continue
            relative = path.relative_to(source)
            if any(parent.is_symlink() for parent in path.parents if parent != source):
                continue
            size = path.stat().st_size
            amount = min(size, remaining, PER_FILE)
            destination = output / source.name / relative
            if amount < size:
                destination = destination.with_name(destination.name + ".truncated")
            destination.parent.mkdir(parents=True, exist_ok=True)
            with path.open("rb") as src, destination.open("wb") as dst:
                data = src.read(amount)
                dst.write(data)
            remaining -= len(data)
            count += 1
    (output / "LIMITS.txt").write_text(
        "Diagnostics limited to 64 MiB total, 8 MiB/file, 128 files; VM disks are omitted.\n")


if __name__ == "__main__":
    collect(Path.cwd())
