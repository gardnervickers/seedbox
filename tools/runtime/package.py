#!/usr/bin/env python3
"""Package Nix-built guest helpers without executing them."""
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile

def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def write_package(assets, output):
    """Archive file roles, not the container client's local chmod/umask policy."""
    if sum(path.stat().st_size for path in assets.iterdir() if path.is_file()) > 2 << 30:
        raise ValueError('runtime exceeds 2 GiB')
    with output.open('wb') as raw:
        with gzip.GzipFile(filename='', fileobj=raw, mode='wb', compresslevel=6, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode='w', format=tarfile.USTAR_FORMAT) as archive:
                for path in sorted(assets.iterdir()):
                    if path.is_file():
                        header = tarfile.TarInfo(path.name)
                        header.size = path.stat().st_size
                        header.mode = 0o755 if path.name in {'compose-init', 'mke2fs'} else 0o644
                        with path.open('rb') as source:
                            archive.addfile(header, source)
    if output.stat().st_size > 512 << 20:
        raise ValueError('compressed runtime exceeds 512 MiB')


def package(guest, output):
    with tempfile.TemporaryDirectory(prefix='seedbox-runtime-') as directory:
        assets = Path(directory) / 'assets'
        assets.mkdir()
        # Nix outputs are read-only; stage file bytes without their directory modes.
        for path in guest.iterdir():
            shutil.copyfile(path, assets / path.name)
        records = {path.name: digest(path)
                   for path in sorted(assets.iterdir()) if path.is_file()}
        (assets / 'runtime.json').write_text(json.dumps({'version': 1, 'files': records}, indent=2) + '\n')
        write_package(assets, output)


if __name__ == '__main__':
    package(Path(sys.argv[1]), Path(sys.argv[2]))
