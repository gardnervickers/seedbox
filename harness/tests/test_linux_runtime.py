"""Portable package-format tests; synthetic ELF/kernel bytes are never executed."""
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "tools/linux-runtime/package.py"
ASSETS = ("bzImage", "compose-init", "busybox", "mke2fs")
SOURCES = ("linux", "busybox", "e2fsprogs", "musl")


def static_elf():
    value = bytearray(128)
    value[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHI", value, 16, 2, 62, 1)
    struct.pack_into("<Q", value, 32, 64)
    struct.pack_into("<HHH", value, 52, 64, 56, 1)
    struct.pack_into("<IIQQQQQQ", value, 64, 1, 5, 0, 0, 0, 128, 128, 1)
    return bytes(value)


def bzimage():
    value = bytearray(4096)
    value[0x1f1] = 4
    struct.pack_into("<I", value, 0x1f4, 96)
    value[510:512] = b"\x55\xaa"
    value[514:518] = b"HdrS"
    struct.pack_into("<H", value, 518, 0x020c)
    value[0x211] = 1
    struct.pack_into("<H", value, 0x236, 3)
    return bytes(value)


def source_archive(name, with_notice=True):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        text = f"Synthetic {name} parser-test notice; no upstream code.\n".encode()
        member = tarfile.TarInfo(f"{name}-fixture/" + ("COPYING" if with_notice else "README"))
        member.size = len(text)
        member.mode = 0o644
        archive.addfile(member, io.BytesIO(text))
        if name == "linux" and with_notice:
            member = tarfile.TarInfo("linux-fixture/LICENSES/preferred/GPL-2.0")
            member.size = len(text)
            member.mode = 0o644
            archive.addfile(member, io.BytesIO(text))
    return gzip.compress(raw.getvalue(), mtime=0)


class LinuxRuntimeTests(unittest.TestCase):
    def fixture(self, root):
        assets = root / "assets"
        sources = root / "sources"
        assets.mkdir()
        sources.mkdir()
        for name in ASSETS:
            (assets / name).write_bytes(bzimage() if name == "bzImage" else static_elf())
        for name in SOURCES:
            (sources / (name + ".source")).write_bytes(source_archive(name))
        return assets, sources

    def package(self, assets, sources, output):
        return subprocess.run([sys.executable, str(PACKAGE), "--assets", str(assets),
                               "--sources", str(sources), "--output", str(output),
                               "--revision", "a" * 40, "--dirty", "0"],
                              cwd=ROOT, text=True, capture_output=True, timeout=30)

    def test_actual_builder_is_deterministic_and_binds_notices_sources_recipes_and_artifacts(self):
        with tempfile.TemporaryDirectory(prefix="seedbox-linux-package-") as directory:
            root = Path(directory)
            assets, sources = self.fixture(root)
            first, second = root / "first.tar.gz", root / "second.tar.gz"
            result = self.package(assets, sources, first)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            for path in [*assets.iterdir(), *sources.iterdir()]:
                os.utime(path, (1_000_000_000, 1_000_000_000))
            result = self.package(assets, sources, second)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            self.assertEqual(first.read_bytes()[4:8], b"\0" * 4)
            with tarfile.open(first, "r:gz") as archive:
                members = archive.getmembers()
                self.assertEqual({m.name for m in members}, {*ASSETS, "assets.json", "NOTICE.txt", "provenance.json"})
                self.assertEqual(len(members), 7)
                for member in members:
                    self.assertTrue(member.isfile())
                    self.assertEqual((member.uid, member.gid, member.mtime), (0, 0, 0))
                    if member.name in ASSETS:
                        self.assertEqual(member.mode, 0o600 if member.name == "bzImage" else 0o755)
                payload = {m.name: archive.extractfile(m).read() for m in members}
            manifest = json.loads(payload["assets.json"])
            self.assertEqual((manifest["format"], manifest["host_platform"], manifest["guest_arch"]),
                             (1, "x86_64-linux", "x86_64"))
            self.assertEqual(set(manifest["files"]), set(payload) - {"assets.json"})
            for name, record in manifest["files"].items():
                self.assertEqual(record["size"], len(payload[name]))
                self.assertEqual(record["sha256"], hashlib.sha256(payload[name]).hexdigest())
                self.assertEqual(record["execution_arch"], "x86_64" if name in ASSETS[1:] else None)
            provenance = json.loads(payload["provenance.json"])
            self.assertEqual(provenance["source_revision"], "a" * 40)
            self.assertFalse(provenance["source_dirty"])
            for name in SOURCES:
                self.assertEqual(provenance["sources"][name]["sha256"],
                                 hashlib.sha256((sources / (name + ".source")).read_bytes()).hexdigest())
                self.assertIn(f"Synthetic {name} parser-test notice".encode(), payload["NOTICE.txt"])
            self.assertEqual(provenance["notice_sha256"], hashlib.sha256(payload["NOTICE.txt"]).hexdigest())
            for name, digest in provenance["recipes"].items():
                self.assertEqual(digest, hashlib.sha256((ROOT / name).read_bytes()).hexdigest())
            self.assertEqual(provenance["flake_lock_sha256"], provenance["recipes"]["flake.lock"])
            self.assertEqual(provenance["artifacts"],
                             {name: hashlib.sha256(payload[name]).hexdigest() for name in ASSETS})

    def test_builder_rejects_missing_upstream_notice_and_invalid_helper_before_output(self):
        for failure in ("missing_notice", "wrong_arch", "interpreter"):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory(prefix="seedbox-linux-reject-") as directory:
                root = Path(directory)
                assets, sources = self.fixture(root)
                if failure == "missing_notice":
                    (sources / "linux.source").write_bytes(source_archive("linux", with_notice=False))
                else:
                    bad = bytearray(static_elf())
                    if failure == "wrong_arch":
                        struct.pack_into("<H", bad, 18, 183)
                    else:
                        struct.pack_into("<I", bad, 64, 3)  # PT_INTERP
                    (assets / "busybox").write_bytes(bad)
                output = root / "bad.tar.gz"
                result = self.package(assets, sources, output)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse(output.exists(), "failed packaging published an output")
