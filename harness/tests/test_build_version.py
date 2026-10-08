"""Exercise the actual provenance build script and Cargo's incremental cache."""
import hashlib
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
            '[package]\nname="version-probe"\nversion="0.1.0"\nedition="2021"\nbuild="build-runtime.rs"\n[workspace]\n')
        (crate / "src/main.rs").write_text(
            '''include!(concat!(env!("OUT_DIR"), "/mac-runtime.rs"));
fn main() {
    if std::env::args().any(|argument| argument == "--bundle") {
        print!("{}", std::str::from_utf8(MAC_RUNTIME.unwrap()).unwrap());
    } else { println!("{}", env!("SEEDBOX_SIMULATOR_VERSION")); }
}
''')
        (root / "tracked.txt").write_text("initial\n")
        for relative in ["crates/seedbox/build-runtime.rs", ".cargo/config.toml", "crates/seedbox/build_version.rs",
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
                     "SEEDBOX_MAC_RUNTIME_BUNDLE", "CARGO_BUILD_TARGET", "SEEDBOX_BUILD_ROOT"]:
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

    def test_shared_cache_selects_current_revision_bundle_and_supervisor_sources(self):
        with tempfile.TemporaryDirectory(prefix="seedbox-version-cache-") as directory:
            temporary = Path(directory).resolve()
            first = temporary / "first"
            self.fixture(first)
            # Create every checkout before warming the cache; timestamps alone
            # cannot distinguish their source or Git metadata.
            roots = [first, temporary / "second", temporary / "third"]
            for index, root in enumerate(roots[1:], 1):
                self.git(first, "worktree", "add", "-b", f"probe/cache-{index}", str(root), "HEAD")
            # Model the historical CLI script target with its compiled-in
            # checkout and no config watcher, then switch to the fixed target.
            legacy = first / "crates/seedbox/build-runtime.rs"
            source = legacy.read_text()
            start = source.index("    let manifest =")
            end = source.index("    simulator_version(&root);")
            source = source[:start] + '    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");\n' + source[end:]
            legacy.rename(legacy.with_name("build.rs"))
            legacy.with_name("build.rs").write_text(source)
            manifest = first / "crates/seedbox/Cargo.toml"
            manifest.write_text(manifest.read_text().replace('build="build-runtime.rs"', 'build="build.rs"'))
            (first / ".cargo/config.toml").unlink()
            for index, root in enumerate(roots):
                (root / "guest/compose-init.c").write_text(f"supervisor-{index}\n")
                (root / "guest/control.h").write_text(f"header-{index}\n")
                (root / "runtime-bundle").write_text(f"bundle-{index}")
                self.git(root, "add", ".")
                self.git(root, "commit", "-qm", f"checkout {index}")
            (roots[1] / "tracked.txt").write_text("dirty source\n")
            for root in roots:
                for path in root.rglob("*"):
                    if path.is_file() and ".git" not in path.parts:
                        os.utime(path, (1_000_000_000, 1_000_000_000))

            environment = os.environ.copy()
            for name in ["SEEDBOX_SOURCE_REVISION", "SEEDBOX_SOURCE_DIRTY",
                         "SEEDBOX_BUILD_ROOT", "CARGO_BUILD_TARGET"]:
                environment.pop(name, None)
            target = temporary / "target"
            environment["CARGO_TARGET_DIR"] = str(target)
            # Keep the bundle environment value unchanged across checkouts so
            # it cannot mask a missing checkout invalidation dependency.
            environment["SEEDBOX_MAC_RUNTIME_BUNDLE"] = "../../runtime-bundle"
            environment["SEEDBOX_STATIC_CC"] = str(temporary / "absent-compiler")
            # Cargo selects and embeds each checkout's distinct bundle while the
            # real script must select its Git state and guest inputs at runtime.
            for root in [*roots, first, roots[1], roots[2]]:
                result = subprocess.run(["cargo", "build", "--offline", "-vv"],
                                        cwd=root / "crates/seedbox", env=environment, text=True,
                                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=120)
                self.assertEqual(result.returncode, 0, result.stdout)
                expected = "0.1.0-dev+" + self.git(root, "rev-parse", "HEAD")[:12]
                if root == roots[1]:
                    expected += ".dirty"
                binary = str(target / "debug/version-probe")
                self.assertEqual(subprocess.check_output([binary], text=True).strip(), expected, result.stdout)
                self.assertEqual(subprocess.check_output([binary, "--bundle"], text=True),
                                 (root / "runtime-bundle").read_text(), result.stdout)
                outputs = list((target / "debug/build").glob("version-probe-*/output"))
                watched = [Path(line.removeprefix("cargo::rerun-if-changed=")).resolve()
                           for output in outputs for line in output.read_text().splitlines()
                           if line.startswith("cargo::rerun-if-changed=")]
                self.assertIn(root / "guest/compose-init.c", watched, result.stdout)

            # Exercise the real Linux supervisor branch on Mac with a compiler
            # stand-in. It hashes the source and headers passed by the script,
            # proving input selection without requiring a Linux toolchain.
            compiler = temporary / "source-compiler"
            compiler.write_text("#!/usr/bin/env python3\nimport hashlib, pathlib, sys\n"
                                "source = pathlib.Path(sys.argv[-1])\n"
                                "inputs = [source, source.with_name('control.h'), source.with_name('barrier.h'), "
                                "source.with_name('io.h'), source.parent.parent / 'kernel-arm64/supervisor-io.h']\n"
                                "digest = hashlib.sha256(b''.join(p.read_bytes() for p in inputs)).digest()\n"
                                "pathlib.Path(sys.argv[sys.argv.index('-o') + 1]).write_bytes(digest)\n")
            compiler.chmod(0o755)
            script = temporary / "build-script"
            compile_env = environment.copy()
            compile_env["CARGO_PKG_VERSION"] = "0.1.0"
            subprocess.run(["rustc", "--edition=2021", str(roots[1] / "crates/seedbox/build-runtime.rs"),
                            "-o", str(script)], env=compile_env, check=True, capture_output=True, timeout=120)
            for root in [*roots, first, roots[2]]:
                output = temporary / "supervisor-output"
                output.mkdir(exist_ok=True)
                runtime = environment.copy()
                runtime.update(CARGO_MANIFEST_DIR=str(root / "crates/seedbox"), OUT_DIR=str(output),
                               CARGO_CFG_TARGET_OS="linux", CARGO_CFG_TARGET_ARCH="x86_64",
                               SEEDBOX_STATIC_CC=str(compiler), SEEDBOX_BUILD_ROOT=str(first),
                               SEEDBOX_MAC_RUNTIME_BUNDLE=str(root / "runtime-bundle"))
                result = subprocess.run([str(script)], cwd=root / "crates/seedbox", env=runtime,
                                        check=True, capture_output=True, text=True, timeout=30)
                inputs = ["guest/compose-init.c", "guest/control.h", "guest/barrier.h",
                          "guest/io.h", "kernel-arm64/supervisor-io.h"]
                expected = hashlib.sha256(b"".join((root / path).read_bytes() for path in inputs)).digest()
                self.assertEqual((output / "compose-init").read_bytes(), expected, result.stdout)
                self.assertIn(f"{root}/guest/compose-init.c", result.stdout)
                if root != first:
                    self.assertIn("seedbox-unconfigured-source-cache", result.stdout)
