#!/usr/bin/env python3
"""Assemble explicit, already-built guest inputs; never build or search for them."""
import argparse
import errno
from pathlib import Path
import shutil
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("backend", choices=("pv", "tcg"))
    for name in ("kernel", "init", "command-line-file", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--progress", type=Path)
    parser.add_argument("--firmware", type=Path)
    args = parser.parse_args()
    if args.backend == "pv" and (args.progress is None or args.firmware is not None):
        parser.error("pv requires --progress and does not accept --firmware")
    if args.backend == "tcg" and (args.firmware is None or args.progress is not None):
        parser.error("tcg requires --firmware and does not accept --progress")
    inputs = {"kernel": args.kernel, "init": args.init, "command-line": args.command_line_file}
    inputs["progress.tar" if args.backend == "pv" else "firmware"] = args.progress if args.backend == "pv" else args.firmware
    for name, path in inputs.items():
        if not path.is_file() or (name != "command-line" and path.stat().st_size == 0):
            parser.error(f"{name}: {path}: missing or empty guest input")
    if args.output.exists():
        parser.error(f"{args.output}: output already exists")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="seedbox-assets-") as temporary:
        staging = Path(temporary) / "assets"
        staging.mkdir()
        for name, source in inputs.items():
            shutil.copyfile(source, staging / name)
        try:
            staging.rename(args.output)
        except OSError as error:
            if error.errno != errno.EXDEV:
                raise
            # Nix may put its build scratch and declared output on different
            # filesystems; the store publishes that output only after success.
            # Create exclusively before copying so cleanup can only remove a
            # directory this invocation owns, never a concurrent destination.
            args.output.mkdir()
            try:
                shutil.copytree(staging, args.output, dirs_exist_ok=True)
            except BaseException:
                shutil.rmtree(args.output, ignore_errors=True)
                raise


if __name__ == "__main__":
    main()
