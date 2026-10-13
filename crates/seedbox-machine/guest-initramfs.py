#!/usr/bin/env python3
"""Private diskless guest archive; all executable inputs are caller supplied."""
import argparse
from pathlib import Path


def archive(init: bytes, application: bytes) -> bytes:
    def text(value: bytes) -> bytes:
        return len(value).to_bytes(4, "little") + value
    # Same private counted recipe consumed by the shared PID 1 helper.
    recipe = (b"SBXAPP2\0" + (1).to_bytes(4, "little") + text(b"/application")
              + (0).to_bytes(4, "little") + (0).to_bytes(4, "little") + text(b"/") + text(b""))
    result = bytearray()
    entries = [
        ("dev", 0o040755, b"", 0, 0),
        ("sys", 0o040755, b"", 0, 0),
        ("proc", 0o040755, b"", 0, 0),
        ("tmp", 0o041777, b"", 0, 0),
        ("dev/console", 0o020600, b"", 5, 1),
        ("init", 0o100755, init, 0, 0),
        ("seedbox", 0o040755, b"", 0, 0),
        ("seedbox/application.bin", 0o100400, recipe, 0, 0),
        ("rootfs", 0o040755, b"", 0, 0),
        ("rootfs/application", 0o100755, application, 0, 0),
        ("TRAILER!!!", 0, b"", 0, 0),
    ]
    for inode, (name, mode, data, major, minor) in enumerate(entries, 1):
        encoded_name = name.encode("ascii") + b"\0"
        fields = [inode, mode, 0, 0, 1, 0, len(data), 0, 0,
                  major, minor, len(encoded_name), 0]
        result.extend(b"070701")
        result.extend("".join(f"{field:08x}" for field in fields).encode("ascii"))
        result.extend(encoded_name)
        result.extend(b"\0" * (-len(result) % 4))
        result.extend(data)
        result.extend(b"\0" * (-len(result) % 4))
    return bytes(result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--init", required=True, type=Path)
    parser.add_argument("--application", required=True, type=Path)
    parser.add_argument("--base", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    base = args.base.read_bytes() if args.base else b""
    base += b"\0" * (-len(base) % 4)
    args.output.write_bytes(base + archive(args.init.read_bytes(), args.application.read_bytes()))


if __name__ == "__main__":
    main()
