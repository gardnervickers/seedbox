"""Identical guest bytes must not inherit Docker/Podman export permissions."""
import importlib.util
import os
from pathlib import Path
import tarfile
import tempfile
import unittest


class RuntimePackageTests(unittest.TestCase):
    def test_export_modes_and_timestamps_do_not_change_package_bytes(self):
        script = Path(__file__).resolve().parents[2] / "tools/runtime/package.py"
        spec = importlib.util.spec_from_file_location("runtime_build", script)
        build = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(build)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            packages = []
            for mode, timestamp in ((0o600, 1234), (0o755, 5678)):
                assets = root / str(mode)
                assets.mkdir()
                for name in ("NOTICE.txt", "compose-init", "mke2fs"):
                    path = assets / name
                    path.write_bytes(name.encode())
                    path.chmod(mode)
                    os.utime(path, (timestamp, timestamp))
                output = root / (str(mode) + ".tar.gz")
                build.package(assets, output)
                packages.append(output)
            self.assertEqual(packages[0].read_bytes(), packages[1].read_bytes())
            with tarfile.open(packages[0]) as archive:
                for entry in archive:
                    self.assertEqual(entry.mode, 0o755 if entry.name in {"compose-init", "mke2fs"} else 0o644)
                    self.assertEqual((entry.uid, entry.gid, entry.mtime), (0, 0, 0))
                    self.assertEqual(archive.extractfile(entry).read(), entry.name.encode())

    def test_package_accepts_read_only_nix_outputs(self):
        script = Path(__file__).resolve().parents[2] / "tools/runtime/package.py"
        spec = importlib.util.spec_from_file_location("runtime_package", script)
        package = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(package)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            guest = root / "guest"
            guest.mkdir()
            names = ["compose-init", "mke2fs", "NOTICE.txt"]
            for name in names:
                path = guest / name
                path.write_bytes(name.encode())
                path.chmod(0o444)
            (guest / "runtime.json").write_text("obsolete hash manifest")
            (guest / "provenance.json").write_text("obsolete provenance")
            guest.chmod(0o555)
            output = root / "runtime.tar.gz"
            try:
                package.package(guest, output)
                self.assertEqual(guest.stat().st_mode & 0o777, 0o555)
            finally:
                guest.chmod(0o700)
            with tarfile.open(output) as archive:
                self.assertEqual(set(archive.getnames()), set(names))
                for name in names:
                    self.assertEqual(archive.extractfile(name).read(), name.encode())
