"""Offline packer publication/ownership checks; no guest execution."""
import errno
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("guest_assets", Path(__file__).with_name("guest-assets.py"))
packer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(packer)


class Publication(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.output = self.root / "output"
        self.inputs = {}
        for name in ("kernel", "init", "command-line", "firmware"):
            path = self.root / name
            path.write_bytes(b"" if name == "command-line" else name.encode())
            self.inputs[name] = path
        self.argv = ["guest-assets.py", "tcg", "--kernel", str(self.inputs["kernel"]), "--init", str(self.inputs["init"]), "--command-line-file", str(self.inputs["command-line"]), "--firmware", str(self.inputs["firmware"]), "--output", str(self.output)]

    def run_packer(self):
        with patch.object(sys, "argv", self.argv):
            packer.main()

    def test_existing_destination_is_preserved(self):
        self.output.mkdir()
        (self.output / "sentinel").write_bytes(b"other owner")
        with self.assertRaises(SystemExit):
            self.run_packer()
        self.assertEqual((self.output / "sentinel").read_bytes(), b"other owner")

    def test_concurrent_destination_is_preserved_during_cross_filesystem_fallback(self):
        def publish(_, destination):
            destination.mkdir()
            (destination / "sentinel").write_bytes(b"concurrent owner")
            raise OSError(errno.EXDEV, "different filesystem")
        with patch.object(Path, "rename", publish), self.assertRaises(FileExistsError):
            self.run_packer()
        self.assertEqual((self.output / "sentinel").read_bytes(), b"concurrent owner")

    def test_failed_copy_removes_only_its_owned_output(self):
        def copy(_, destination, **kwargs):
            (destination / "partial").write_bytes(b"partial")
            raise OSError("copy failed")
        with patch.object(Path, "rename", side_effect=OSError(errno.EXDEV, "different filesystem")), patch.object(packer.shutil, "copytree", copy), self.assertRaises(OSError):
            self.run_packer()
        self.assertFalse(self.output.exists())
        self.assertEqual(self.inputs["kernel"].read_bytes(), b"kernel")

    def test_cross_filesystem_copy_keeps_exact_inputs_and_empty_arguments(self):
        with patch.object(Path, "rename", side_effect=OSError(errno.EXDEV, "different filesystem")):
            self.run_packer()
        for name, source in self.inputs.items():
            self.assertEqual((self.output / name).read_bytes(), source.read_bytes())


if __name__ == "__main__":
    unittest.main()
