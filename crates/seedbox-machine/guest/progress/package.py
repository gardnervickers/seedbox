#!/usr/bin/env python3
"""Flatten runtime links and archive only regular owned guest files."""
from pathlib import Path
import sys
import tarfile

root, output = map(Path, sys.argv[1:])
with tarfile.open(output, 'w', format=tarfile.USTAR_FORMAT) as archive:
    for path in sorted(root.rglob('*')):
        if path.is_dir():
            continue
        name = path.relative_to(root).as_posix()
        header = tarfile.TarInfo("seedbox/progress/" + name)
        header.size = path.stat().st_size
        header.mode = 0o644 if name.endswith(('.txt', '.LIB')) else 0o755
        with path.open('rb') as stream:
            archive.addfile(header, stream)
