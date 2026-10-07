"""Portable packaging controls; no Cargo, kernel, application or Nix build."""

import hashlib
import json
import os
from pathlib import Path
import tarfile
import tempfile
import unittest

from package import REQUIRED, package


class Packaging(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.store = self.root / "store"
        self.store.mkdir()
        self.dep = self.store / ("0" * 32 + "-fixture")
        self.dep.mkdir()
        (self.dep / "lib").write_bytes(b"runtime bytes")
        (self.dep / "link").symlink_to("lib")
        os.link(self.dep / "lib", self.dep / "same-inode")

    def tearDown(self):
        self.temp.cleanup()

    def payload(self, name, reverse=False):
        payload = self.root / name
        for path in sorted(REQUIRED, reverse=reverse):
            entry = payload / path
            entry.parent.mkdir(parents=True, exist_ok=True)
            entry.write_bytes(path.encode())
            entry.chmod(0o755 if path.startswith(("bin/", "libexec/")) else 0o644)
        (payload / "provenance.json").write_text(json.dumps({
            "status": "UNQUALIFIED_DRAFT", "sourceRevision": "a" * 40,
        }, sort_keys=True) + "\n")
        (payload / "runtime-store-paths").write_text(str(self.dep) + "\n")
        for path in [payload] + list(payload.rglob("*")):
            os.utime(path, (17 if reverse else 19000, 17 if reverse else 19000))
        return payload

    def build(self, payload, name):
        output = self.root / name
        package(payload, payload / "runtime-store-paths", output, store_root=self.store)
        return output

    def test_clean_directories_creation_order_and_mtime_do_not_change_archive(self):
        a = self.build(self.payload("independent-long-path-a"), "a")
        b = self.build(self.payload("b", reverse=True), "b-output")
        archive_name = "seedbox-x86_64-linux-nix.tar.gz"
        self.assertEqual((a / archive_name).read_bytes(), (b / archive_name).read_bytes())
        with tarfile.open(a / archive_name) as tar:
            for entry in tar:
                self.assertEqual((entry.uid, entry.gid, entry.mtime), (0, 0, 0))
            data = tar.extractfile("payload/bin/seedbox").read()
            self.assertEqual(data, b"bin/seedbox")
            self.assertEqual(tar.getmember("payload/bin/seedbox").mode, 0o755)
            self.assertEqual(tar.getmember("nix/store/" + self.dep.name + "/link").linkname, "lib")
            manifest = json.load(tar.extractfile("SHA256SUMS.json"))
            self.assertEqual(manifest["payload/bin/seedbox"], hashlib.sha256(data).hexdigest())
            self.assertIn("nix/store/" + self.dep.name + "/lib", manifest)
            self.assertEqual(tar.extractfile("nix/store/" + self.dep.name + "/same-inode").read(), b"runtime bytes")
        self.assertEqual((a / archive_name).read_bytes()[:10], b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x02\xff")

    def test_changed_payload_or_executability_changes_archive(self):
        original = self.build(self.payload("original"), "original-output")
        changed = self.payload("changed")
        (changed / "bin/seedbox").write_bytes(b"different real bytes")
        altered = self.build(changed, "changed-output")
        self.assertNotEqual((original / "archive.sha256").read_bytes(), (altered / "archive.sha256").read_bytes())
        mode = self.payload("mode")
        (mode / "bin/seedbox").chmod(0o644)
        altered = self.build(mode, "mode-output")
        self.assertNotEqual((original / "archive.sha256").read_bytes(), (altered / "archive.sha256").read_bytes())

    def test_missing_bootstrap_invalid_revision_or_closure_rejected_before_output(self):
        for case in ("bootstrap", "revision", "closure", "special"):
            with self.subTest(case=case):
                payload = self.payload(case)
                if case == "bootstrap":
                    (payload / "libexec/compose-init").unlink()
                elif case == "revision":
                    (payload / "provenance.json").write_text('{"status":"UNQUALIFIED_DRAFT","sourceRevision":"uncommitted"}')
                elif case == "closure":
                    (payload / "runtime-store-paths").write_text("/etc\n")
                else:
                    os.mkfifo(payload / "unexpected-pipe")
                output = self.root / (case + "-output")
                with self.assertRaises(ValueError):
                    package(payload, payload / "runtime-store-paths", output, store_root=self.store)
                self.assertFalse(output.exists())

    def test_symlinked_required_parent_refused_before_output(self):
        payload = self.payload("symlink-parent")
        outside = self.root / "outside-bin"
        (payload / "bin").rename(outside)
        (payload / "bin").symlink_to(outside, target_is_directory=True)
        # Real binaries exist through the link, but are not archive members.
        self.assertTrue((payload / "bin/seedbox").is_file())
        output = self.root / "symlink-parent-output"
        with self.assertRaisesRegex(ValueError, "missing regular archive member: payload/bin/"):
            package(payload, payload / "runtime-store-paths", output, store_root=self.store)
        self.assertFalse(output.exists())

    def test_existing_output_and_recursive_output_refused(self):
        payload = self.payload("payload")
        with self.assertRaisesRegex(ValueError, "outside"):
            package(payload, payload / "runtime-store-paths", payload / "output", store_root=self.store)
        output = self.root / "output"
        output.mkdir()
        (output / "receipt").write_bytes(b"retain me")
        with self.assertRaisesRegex(ValueError, "empty"):
            package(payload, payload / "runtime-store-paths", output, store_root=self.store)
        self.assertEqual((output / "receipt").read_bytes(), b"retain me")


if __name__ == "__main__":
    unittest.main()
