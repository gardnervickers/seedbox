#!/usr/bin/env python3
"""Build real, libc-free Linux ELF guests with the project shell's toolchain."""

from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def build() -> None:
    output = ROOT / "build" / "guests"
    output.mkdir(parents=True, exist_ok=True)
    for source in sorted((ROOT / "guests").glob("*.asm")):
        obj = output / f"{source.stem}.o"
        subprocess.run(["nasm", "-f", "elf64", "-o", str(obj), str(source)], check=True)
        subprocess.run(["ld.lld", "-m", "elf_x86_64", "-static", "-o", str(output / source.stem), str(obj)], check=True)
        if source.stem == "race":
            obj = output / "atomic.o"
            subprocess.run(["nasm", "-D", "ATOMIC=1", "-f", "elf64", "-o", str(obj), str(source)], check=True)
            subprocess.run(["ld.lld", "-m", "elf_x86_64", "-static", "-o", str(output / "atomic"), str(obj)], check=True)


if __name__ == "__main__":
    build()
