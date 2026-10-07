#!/usr/bin/env python3
"""Write deterministic newc, including device nodes, without root or cpio."""
import argparse
from pathlib import Path
import stat

def archive(init):
    out = bytearray()
    def add(name, mode, data=b"", major=0, minor=0):
        name = name.encode() + b"\0"
        # ino,mode,uid,gid,nlink,mtime,size,devmajor,devminor,rdevmajor,rdevminor,namesize,check
        fields = [1, mode, 0, 0, 1, 0, len(data), 0, 0, major, minor, len(name), 0]
        out.extend(b"070701" + b"".join(f"{n:08x}".encode() for n in fields))
        out.extend(name)
        out.extend(b"\0" * (-len(out) % 4))
        out.extend(data)
        out.extend(b"\0" * (-len(out) % 4))
    for directory in ["dev", "proc", "sys"]: add(directory, stat.S_IFDIR | 0o755)
    add("dev/console", stat.S_IFCHR | 0o600, major=5, minor=1)
    add("dev/null", stat.S_IFCHR | 0o666, major=1, minor=3)
    add("init", stat.S_IFREG | 0o755, init)
    add("TRAILER!!!", 0)
    out.extend(b"\0" * (-len(out) % 512))
    return out

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("init", type=Path)
    p.add_argument("output", type=Path)
    args = p.parse_args()
    args.output.write_bytes(archive(args.init.read_bytes()))
if __name__ == "__main__": main()
