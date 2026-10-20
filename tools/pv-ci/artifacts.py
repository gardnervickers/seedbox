#!/usr/bin/env python3
"""Keep bounded diagnostics; full VM state needs an explicit separate export."""
from pathlib import Path

LIMIT = 64 * 1024 * 1024
PER_FILE = 8 * 1024 * 1024
FILES = 128


def priority(path):
    # Test-owned failure observations must precede routine successful run output.
    # Within failures, retain primary errors and guest assertion logs first.
    failure = "failure" in path.parts
    rank = 0 if path.name == "summary.txt" else (
        1 if "log" in path.name or path.name in ("replay.stdout", "replay.stderr") else 2)
    return (not failure, rank if failure else 0, path)


def collect(root):
    output = root / "target/pv-artifacts"
    output.mkdir(parents=True, exist_ok=False)
    remaining, count = LIMIT - 4096, 0
    omitted, truncated, details = 0, 0, []
    source = root / "target/check-reports"
    if not source.is_symlink() and source.is_dir():
        paths = sorted(source.rglob("*"), key=lambda path: priority(path.relative_to(source)))
        for path in paths:
            if path.is_symlink() or not path.is_file() or path.suffix not in (
                    ".json", ".txt", ".log", ".out", ".stderr", ".stdout"):
                continue
            relative = path.relative_to(source)
            if any(parent.is_symlink() for parent in path.parents if parent != source):
                continue
            size = path.stat().st_size
            if count >= FILES or remaining <= 0:
                omitted += 1
                if len(details) < 12:
                    details.append(f"omitted: {str(relative)[:200]}")
                continue
            amount = min(size, remaining, PER_FILE)
            destination = output / source.name / relative
            if amount < size:
                truncated += 1
                if len(details) < 12:
                    details.append(f"truncated: {str(relative)[:200]} ({amount}/{size} bytes)")
                destination = destination.with_name(destination.name + ".truncated")
            destination.parent.mkdir(parents=True, exist_ok=True)
            with path.open("rb") as src, destination.open("wb") as dst:
                data = src.read(amount)
                dst.write(data)
            remaining -= len(data)
            count += 1
    # The reserved 4096 bytes cover this bounded omission summary, including when
    # there are too many priority diagnostics to retain them all.
    summary = (
        "Diagnostics limited to 64 MiB total, 8 MiB/file, 128 diagnostic files; VM disks are omitted.\n"
        "Test failure directories take priority over routine output.\n"
        f"Retained files: {count}; omitted eligible files: {omitted}; truncated files: {truncated}.\n"
        "Omission paths (first 12, names bounded to 200 characters):\n" + "\n".join(details) + "\n")
    if len(summary.encode()) > 4096:
        summary = summary.encode()[:4000].decode(errors="ignore") + "\nOmission path details shortened to fit reserved budget.\n"
    (output / "LIMITS.txt").write_text(summary)


if __name__ == "__main__":
    collect(Path.cwd())
