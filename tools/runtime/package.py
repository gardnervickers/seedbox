#!/usr/bin/env python3
"""Package Nix-built guest helpers without executing them."""
import gzip
from pathlib import Path
import sys
import tarfile

def package(assets, output):
    """Archive file roles, not the container client's local chmod/umask policy."""
    members = [assets / name for name in ('compose-init', 'mke2fs', 'NOTICE.txt', 'progress.tar')]
    if sum(path.stat().st_size for path in members) > 2 << 30:
        raise ValueError('runtime exceeds 2 GiB')
    with output.open('wb') as raw:
        with gzip.GzipFile(filename='', fileobj=raw, mode='wb', compresslevel=6, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode='w', format=tarfile.USTAR_FORMAT) as archive:
                for path in sorted(members):
                    header = tarfile.TarInfo(path.name)
                    header.size = path.stat().st_size
                    header.mode = 0o755 if path.name in {'compose-init', 'mke2fs'} else 0o644
                    with path.open('rb') as source:
                        archive.addfile(header, source)
    if output.stat().st_size > 512 << 20:
        raise ValueError('compressed runtime exceeds 512 MiB')


if __name__ == '__main__':
    package(Path(sys.argv[1]), Path(sys.argv[2]))
