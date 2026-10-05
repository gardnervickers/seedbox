from pathlib import Path
import subprocess
import sys

import pytest

ROOT = Path(__file__).resolve().parents[1]


@pytest.fixture(scope="session")
def guests():
    subprocess.run([sys.executable, str(ROOT / "scripts/build_guests.py")], check=True)
    return {path.name: path.read_bytes() for path in (ROOT / "build/guests").iterdir() if path.suffix != ".o"}


@pytest.fixture
def assemble(tmp_path):
    def build(body, data="", bss=""):
        source = tmp_path / "guest.asm"
        obj = tmp_path / "guest.o"
        binary = tmp_path / "guest"
        source.write_text(f"bits 64\nglobal _start\nsection .text\n_start:\n{body}\nsection .data\n{data}\nsection .bss\n{bss}\n")
        subprocess.run(["nasm", "-f", "elf64", "-o", str(obj), str(source)], check=True, capture_output=True)
        subprocess.run(["ld.lld", "-m", "elf_x86_64", "-static", "-o", str(binary), str(obj)], check=True, capture_output=True)
        return binary.read_bytes()
    return build
