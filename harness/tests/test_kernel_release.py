"""Kernel release addresses track the kernel recipe, independently of CLI code."""
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("kernel_release", ROOT / "kernel/release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class KernelReleaseTests(unittest.TestCase):
    def test_each_declared_input_changes_the_address(self):
        groups = json.loads((ROOT / "kernel/inputs.json").read_text())
        names = {name for group in groups.values() for name in group}
        self.assertIn("flake.lock", names)
        self.assertIn("kernel/source.json", names)
        self.assertIn("nix/kernels.nix", names)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in names:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / name, path)
            original = release.release_id(root)
            self.assertEqual(original, release.release_id(ROOT))
            for name in sorted(names - {"kernel/inputs.json"}):
                with self.subTest(input=name):
                    path = root / name
                    content = path.read_bytes()
                    path.write_bytes(content + b"\n")
                    self.assertNotEqual(release.release_id(root), original)
                    path.write_bytes(content)
            for name in ("guest/compose-init.c", "crates/seedbox/src/cli/mod.rs", "README.md"):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"unrelated changes")
            self.assertEqual(release.release_id(root), original)
