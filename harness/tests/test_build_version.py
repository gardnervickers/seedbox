"""Exercise the actual provenance build script and Cargo's incremental cache."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class BuildVersionTests(unittest.TestCase):
    def git(self, root, *arguments):
        return subprocess.check_output([
            "git", "-c", "commit.gpgSign=false", "-c", "tag.gpgSign=false", "-c", "core.hooksPath=/dev/null",
            "-c", "user.name=Version Probe", "-c", "user.email=probe@example.invalid",
            "-C", str(root), *arguments,
        ], stderr=subprocess.STDOUT, text=True, timeout=30).strip()

    def fixture(self, root):
        crate = root / "crates/seedbox"
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text(
            '[package]\nname="version-probe"\nversion="0.1.0"\nedition="2021"\n[workspace]\n')
        (crate / "src/main.rs").write_text(
            'fn main() { println!("{}", env!("SEEDBOX_SIMULATOR_VERSION")); }\n')
        (root / "tracked.txt").write_text("initial\n")
        for relative in ["crates/seedbox/build.rs", "crates/seedbox/build_version.rs",
                         "guest/compose-init.c", "guest/control.h", "guest/barrier.h",
                         "guest/io.h", "kernel-arm64/supervisor-io.h"]:
            destination = root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / relative, destination)
        self.git(root, "init", "-q", "--initial-branch=probe/main")
        self.git(root, "add", ".")
        self.git(root, "commit", "-qm", "initial")

    def exercise(self, root, target):
        environment = os.environ.copy()
        for name in ["SEEDBOX_SOURCE_REVISION", "SEEDBOX_SOURCE_DIRTY",
                     "SEEDBOX_MAC_RUNTIME_BUNDLE", "CARGO_BUILD_TARGET"]:
            environment.pop(name, None)
        environment["CARGO_TARGET_DIR"] = str(target)
        crate = root / "crates/seedbox"

        def build(expected, fresh=False):
            result = subprocess.run(["cargo", "build", "--offline", "-vv"],
                                    cwd=crate, env=environment, text=True,
                                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=120)
            self.assertEqual(result.returncode, 0, result.stdout)
            if fresh:
                self.assertIn("Fresh version-probe", result.stdout, result.stdout)
                self.assertNotIn("Compiling version-probe", result.stdout, result.stdout)
            version = subprocess.check_output([str(target / "debug/version-probe")], text=True).strip()
            self.assertEqual(version, expected, result.stdout)

        def version():
            return "0.1.0-dev+" + self.git(root, "rev-parse", "HEAD")[:12]

        build(version())
        build(version(), fresh=True)  # No packed-refs file in a fresh repository.
        (root / "tracked.txt").write_text("changed " + self.git(root, "rev-parse", "HEAD") + "\n")
        build(version() + ".dirty")
        build(version() + ".dirty", fresh=True)
        self.git(root, "add", "tracked.txt")
        self.git(root, "commit", "-qm", "tracked change")
        build(version())
        build(version(), fresh=True)
        self.git(root, "pack-refs", "--all")
        build(version())
        build(version(), fresh=True)
        self.git(root, "commit", "--allow-empty", "-qm", "new revision")
        build(version())  # The branch is recreated as a loose ref.
        build(version(), fresh=True)
        self.git(root, "tag", "preview/version-probe")
        build(version())
        build(version(), fresh=True)
        self.git(root, "tag", "-d", "preview/version-probe")
        build(version())
        build(version(), fresh=True)
        self.git(root, "tag", "v0.1.0")
        build("0.1.0")
        build("0.1.0", fresh=True)
        self.git(root, "tag", "-d", "v0.1.0")
        build(version())
        build(version(), fresh=True)
        (root / "untracked.txt").write_text("unrelated scratch input\n")
        build(version(), fresh=True)  # Only tracked changes define the dirty marker.
        self.git(root, "add", "untracked.txt")
        build(version() + ".dirty")  # A newly staged source is now tracked.
        build(version() + ".dirty", fresh=True)

    def test_normal_checkout_and_linked_worktree_track_provenance_without_noop_rebuilds(self):
        with tempfile.TemporaryDirectory(prefix="seedbox-version-test-") as directory:
            temporary = Path(directory).resolve()
            root = temporary / "repository"
            self.fixture(root)
            with self.subTest(checkout="normal"):
                self.exercise(root, temporary / "normal-target")
            self.git(root, "reset", "--hard", "HEAD")
            linked = temporary / "linked"
            self.git(root, "worktree", "add", "-b", "probe/linked", str(linked), "HEAD")
            try:
                with self.subTest(checkout="linked"):
                    self.exercise(linked, temporary / "linked-target")
            finally:
                self.git(root, "worktree", "remove", "--force", str(linked))
