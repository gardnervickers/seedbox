"""Identical guest bytes must not inherit Docker/Podman export permissions."""
import hashlib
import io
import json
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
                for name in ("Image", "compose-init", "runtime.json"):
                    path = assets / name
                    path.write_bytes(name.encode())
                    path.chmod(mode)
                    os.utime(path, (timestamp, timestamp))
                output = root / (str(mode) + ".tar.gz")
                build.write_package(assets, output)
                packages.append(output)
            self.assertEqual(packages[0].read_bytes(), packages[1].read_bytes())
            with tarfile.open(packages[0]) as archive:
                for entry in archive:
                    self.assertEqual(entry.mode, 0o755 if entry.name == "compose-init" else 0o644)
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
            names = ["compose-init", "mke2fs", "NOTICE.txt", "provenance.json"]
            for name in names:
                path = guest / name
                path.write_bytes(name.encode())
                path.chmod(0o444)
            guest.chmod(0o555)
            output = root / "runtime.tar.gz"
            try:
                package.package(guest, output)
                self.assertEqual(guest.stat().st_mode & 0o777, 0o555)
            finally:
                guest.chmod(0o700)
            with tarfile.open(output) as archive:
                manifest = json.load(archive.extractfile("runtime.json"))
                self.assertNotIn("image", manifest)
                for name in names:
                    content = archive.extractfile(name).read()
                    self.assertEqual(manifest["files"][name], hashlib.sha256(content).hexdigest())
