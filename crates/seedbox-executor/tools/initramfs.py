#!/usr/bin/env python3
"""Wrap the libc-free proof ELF as PID 1, with only a console device."""
import pathlib
import struct
import sys

elf = pathlib.Path(sys.argv[1]).read_bytes()
if elf[:6] != b"\x7fELF\x02\x01":
    raise SystemExit("expected a little-endian ELF64")
phoff = struct.unpack_from("<Q", elf, 32)[0]
phsize, count = struct.unpack_from("<HH", elf, 54)
for index in range(count):
    if struct.unpack_from("<I", elf, phoff + index * phsize)[0] == 3:
        raise SystemExit("proof ELF must not have an interpreter")
archive = bytearray()

def entry(name, mode, data=b"", major=0, minor=0):
    encoded = name.encode() + b"\0"
    fields = [1, mode, 0, 0, 1, 0, len(data), 0, 0, major, minor, len(encoded), 0]
    archive.extend(b"070701" + "".join(f"{v:08x}" for v in fields).encode() + encoded)
    archive.extend(b"\0" * (-len(archive) % 4))
    archive.extend(data)
    archive.extend(b"\0" * (-len(archive) % 4))

entry(".", 0o40755)
entry("dev", 0o40755)
entry("dev/console", 0o20600, major=5, minor=1)
entry("init", 0o100755, elf)
entry("TRAILER!!!", 0)
with pathlib.Path(sys.argv[2]).open("xb") as output:
    output.write(archive)
