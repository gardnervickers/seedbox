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
                for name in ("tools-image.tar", "Image", "compose-init", "runtime.json"):
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

    def test_tools_image_identity_comes_from_its_config_bytes(self):
        script = Path(__file__).resolve().parents[2] / "tools/runtime/package.py"
        spec = importlib.util.spec_from_file_location("runtime_build", script)
        build = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(build)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "image.tar"
            config = b'{"architecture":"arm64","rootfs":{"type":"layers","diff_ids":[]}}'
            with tarfile.open(path, "w") as archive:
                for name, content in [("manifest.json", json.dumps([{"Config": "config.json"}]).encode()), ("config.json", config)]:
                    header = tarfile.TarInfo(name)
                    header.size = len(content)
                    archive.addfile(header, io.BytesIO(content))
            self.assertEqual(build.image_digest(path), "sha256:" + hashlib.sha256(config).hexdigest())


    def test_package_accepts_read_only_nix_outputs(self):
        script = Path(__file__).resolve().parents[2] / "tools/runtime/package.py"
        spec = importlib.util.spec_from_file_location("runtime_package", script)
        package = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(package)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            guest = root / "guest"
            guest.mkdir()
            names = ["compose-init", "busybox", "mke2fs", "NOTICE.txt", "provenance.json"]
            for name in names:
                path = guest / name
                path.write_bytes(name.encode())
                path.chmod(0o444)
            guest.chmod(0o555)
            tools = root / "tools.tar"
            config = b'{"architecture":"arm64","rootfs":{"type":"layers","diff_ids":[]}}'
            with tarfile.open(tools, "w") as archive:
                for name, content in [("manifest.json", json.dumps([{"Config": "config.json"}]).encode()), ("config.json", config)]:
                    header = tarfile.TarInfo(name)
                    header.size = len(content)
                    archive.addfile(header, io.BytesIO(content))
            output = root / "runtime.tar.gz"
            try:
                package.package(guest, tools, output)
                self.assertEqual(guest.stat().st_mode & 0o777, 0o555)
            finally:
                guest.chmod(0o700)
            with tarfile.open(output) as archive:
                manifest = json.load(archive.extractfile("runtime.json"))
                self.assertEqual(manifest["image"], "sha256:" + hashlib.sha256(config).hexdigest())
                for name in [*names, "tools-image.tar"]:
                    content = archive.extractfile(name).read()
                    self.assertEqual(manifest["files"][name], hashlib.sha256(content).hexdigest())
