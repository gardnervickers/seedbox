#!/usr/bin/env python3
"""Acquire only the locked official artifacts, once each; verify before use."""
import argparse
from pathlib import Path
import urllib.request
from stage_inputs import PINS, verify_input

def fetch(destination, client_only=False):
    destination = Path(destination)
    destination.mkdir(parents=True)  # Preserve earlier downloads and receipts.
    items = [(destination / "wheels" / pin["filename"], pin) for pin in PINS["wheels"]]
    if not client_only:
        items.insert(0, (destination / "minio", PINS["minio"]))
    for path, pin in items:
        path.parent.mkdir(parents=True, exist_ok=True)
        with urllib.request.urlopen(pin["url"], timeout=30) as response, path.open("xb") as output:
            if not response.url.startswith("https://"):
                raise ValueError("artifact redirect must remain HTTPS")
            remaining = pin["size"]
            while remaining:
                chunk = response.read(min(1024 * 1024, remaining))
                if not chunk:
                    raise ValueError(f"short artifact: {path}")
                output.write(chunk)
                remaining -= len(chunk)
            if response.read(1):
                raise ValueError(f"oversize artifact: {path}")
        verify_input(path, pin)
        print(f"verified {path.name} sha256={pin['sha256']}", flush=True)

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--client-only", action="store_true")
    args = parser.parse_args()
    fetch(args.destination, args.client_only)
