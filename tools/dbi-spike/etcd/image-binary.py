#!/usr/bin/env python3
"""Read a selected binary from a Docker archive without extracting guest paths."""
import hashlib
import json
import sys
import tarfile
from pathlib import Path

target = "usr/local/bin/etcd"
binary = None
with tarfile.open(sys.argv[1]) as image:
    manifest = json.load(image.extractfile("manifest.json"))
    for layer in manifest[0]["Layers"]:
        with tarfile.open(fileobj=image.extractfile(layer), mode="r|*") as stream:
            for entry in stream:
                name = entry.name.removeprefix("./").lstrip("/")
                if name == target and entry.isfile():
                    binary = stream.extractfile(entry).read()
                if name in ("usr/local/bin/.wh.etcd", "usr/local/bin/.wh..wh..opq"):
                    binary = None
if binary is None:
    raise SystemExit("etcd regular file not found")
if len(sys.argv) == 3:
    Path(sys.argv[2]).write_bytes(binary)
print(json.dumps({"path": target, "bytes": len(binary),
                  "sha256": hashlib.sha256(binary).hexdigest()}))
