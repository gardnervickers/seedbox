"""Identical guest bytes must not inherit Docker/Podman export permissions."""
import hashlib
import io
import json
import importlib.util
import os
from pathlib import Path
import shutil
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


    def test_kernel_changes_do_not_change_the_runtime_release(self):
        root = Path(__file__).resolve().parents[2]
        spec = importlib.util.spec_from_file_location("runtime_release", root / "tools/runtime/release.py")
        release = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(release)
        names = json.loads((root / "tools/runtime/inputs.json").read_text())["common"]
        with tempfile.TemporaryDirectory() as directory:
            checkout = Path(directory)
            for name in names:
                destination = checkout / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(root / name, destination)
            initial = release.release_id(checkout)
            (checkout / "nix/kernels.nix").write_text("changed kernel recipe")
            (checkout / "kernel").mkdir()
            (checkout / "kernel/config.options").write_text("changed kernel configuration")
            self.assertEqual(initial, release.release_id(checkout))
            for name in names:
                if name == "tools/runtime/inputs.json":
                    continue
                with self.subTest(input=name):
                    path = checkout / name
                    content = path.read_bytes()
                    path.write_bytes(content + b"\n")
                    self.assertNotEqual(initial, release.release_id(checkout))
                    path.write_bytes(content)
