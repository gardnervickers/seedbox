#!/usr/bin/env python3
"""Unpack with umoci on Linux; export metadata without extracting onto the host."""
import os
from pathlib import Path
import stat
import subprocess


def run(*args):
    subprocess.run(args, check=True)


def prepare(directory):
    directory = Path(directory)
    source = directory / 'source'
    layout = directory / 'oci'
    bundle = directory / 'bundle'
    tools = Path('/tools/bin')
    run(str(tools / 'skopeo'), '--insecure-policy', 'copy',
        'docker-archive:' + str(directory / 'image.tar'), 'dir:' + str(source))
    run(str(tools / 'skopeo'), '--insecure-policy', 'copy', '--format', 'oci',
        'dir:' + str(source), 'oci:' + str(layout) + ':seedbox')
    run(str(tools / 'umoci'), 'unpack', '--image', str(layout) + ':seedbox', str(bundle))
    # Match the existing newc contract: do not silently discard ACLs,
    # capabilities, device nodes, timestamps, or filenames it cannot represent.
    root = bundle / 'rootfs'
    for parent, dirs, files in os.walk(root, followlinks=False):
        for path in [Path(parent), *(Path(parent) / name for name in dirs + files)]:
            info = path.lstat()
            if os.listxattr(path, follow_symlinks=False):
                raise ValueError('image extended attributes unsupported: ' + str(path))
            if not (stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode)):
                raise ValueError('special image file unsupported: ' + str(path))
            if not 0 <= info.st_mtime <= 0xffffffff or info.st_size > 0xffffffff:
                raise ValueError('image metadata exceeds newc range: ' + str(path))
            str(path).encode('utf-8', errors='strict')
    run(str(tools / 'tar'), '--sort=name', '--format=pax', '--numeric-owner',
        '--pax-option=delete=atime,delete=ctime', '-cf', str(directory / 'rootfs.tar'),
        '-C', str(bundle), 'rootfs')


if __name__ == '__main__':
    import sys
    prepare(sys.argv[1])
