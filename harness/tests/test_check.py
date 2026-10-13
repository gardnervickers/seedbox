"""Suite selection and requirement controls; no hypervisor is opened."""
import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import socket
import tempfile
import unittest
from unittest.mock import patch

HARNESS = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("platform_check", HARNESS / "check.py")
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


class SelectionTests(unittest.TestCase):
    def setUp(self):
        # Checkout-local artifacts must not change these suite controls.
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = patch.object(check, "ROOT", Path(directory.name))
        root.start()
        self.addCleanup(root.stop)

    def linux(self, suite, probe, root=True, inputs=None, machine="x86_64"):
        # Never consult the host's real engine sockets.
        with patch.object(check.shutil, "which", return_value="/fixture/tool"), \
                patch.object(check, "engine_sockets", return_value=[]):
            return check.requirements(suite, "Linux", machine, inputs or {}, probe, root)

    def test_machine_suites_are_explicit_and_require_inputs(self):
        self.assertEqual(check.expand(["machine-tcg"], "x86_64"), ["portable", "machine-tcg"])
        self.assertNotIn("machine-tcg", check.expand(["linux"], "x86_64"))
        missing = self.linux("machine-tcg", {"kvm": "unavailable"})
        self.assertTrue(any("SEEDBOX_TCG_ASSETS" in reason for reason in missing))
        self.assertFalse(any("/dev/kvm" in reason for reason in missing))
        self.assertIn("requires Linux x86-64", self.linux("machine-tcg", {}, machine="aarch64"))
        self.assertTrue(any("/dev/kvm" in reason for reason in
                            self.linux("machine-pv-x86", {"kvm": "denied"})))
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "input"
            executable.write_text("fixture")
            executable.chmod(0o755)
            inputs = {f"SEEDBOX_TCG_{key}": str(executable) for key in (
                "QEMU", "FIRMWARE", "KERNEL", "INIT", "SERVICE_INITRD", "CONTROL_INITRD",
                "DRIVER_CONTROL_APPLICATION", "PREPARED_FIXTURE")}
            inputs["SEEDBOX_TCG_ASSETS"] = directory
            self.assertEqual(self.linux("machine-tcg", {"kvm": "denied"}, inputs=inputs), [])

    def test_default_is_portable_only(self):
        self.assertEqual(check.expand(["portable"], "x86_64"), ["portable"])

    def test_groups_and_implied_suites_expand_in_documented_order(self):
        self.assertEqual(check.expand(["linux"], "x86_64"),
                         ["portable", "linux-kvm", "linux-oci", "linux-apps"])
        self.assertEqual(check.expand(["mac"], "arm64"), ["portable", "mac-hvf", "mac-apps"])
        self.assertEqual(check.expand(["linux-guests"], "x86_64"),
                         ["portable", "linux-kvm", "linux-guests"])
        self.assertEqual(check.expand(["linux-guests"], "aarch64"), ["portable", "linux-kvm", "linux-guests"])
        self.assertEqual(check.expand(["mac-apps"], "arm64"), ["portable", "mac-hvf", "mac-apps"])

    def test_features_follow_suites_not_host_probes(self):
        self.assertEqual(check.features_for("seedbox-vmm", "x86_64", ["portable", "linux-kvm"]), ["kvm-tests"])
        self.assertEqual(check.features_for("seedbox-vmm", "x86_64", check.expand(["linux-guests"], "x86_64")),
                         ["kvm-tests", "guest-tests"])
        self.assertEqual(check.features_for("seedbox", "x86_64", ["portable", "linux-oci", "linux-pause"]),
                         ["retained-run-tests"])
        self.assertEqual(check.features_for("seedbox-application", "x86_64", ["portable", "linux-oci"]),
                         ["oci-tests"])
        self.assertEqual(check.features_for("seedbox-arm64", "x86_64", check.expand(["linux"], "x86_64")), [])
        self.assertEqual(check.features_for("seedbox-arm64", "aarch64", check.expand(["linux-guests"], "aarch64")),
                         ["kvm-tests", "guest-tests"])
        self.assertEqual(check.features_for("seedbox-arm64", "arm64", check.expand(["mac-guests"], "arm64")),
                         ["hvf-tests", "guest-tests"])
        self.assertEqual(check.features_for("seedbox-vmm", "arm64", check.expand(["mac"], "arm64")), [])

    def test_hardware_targets_follow_cargo_declarations(self):
        metadata = {"packages": [{"name": "seedbox", "targets": [
            {"name": "portable", "kind": ["test"]},
            {"name": "image_engine", "kind": ["test"], "required-features": ["oci-tests"]},
            {"name": "combined", "kind": ["test"], "required-features": ["oci-tests", "application-tests"]},
            {"name": "library", "kind": ["lib"], "required-features": ["oci-tests"]},
        ]}]}
        self.assertEqual(check.feature_test_targets(metadata, "seedbox", ["oci-tests"]), ["image_engine"])
        self.assertEqual(check.feature_test_targets(metadata, "seedbox", ["application-tests", "oci-tests"]), ["combined", "image_engine"])
        self.assertEqual(check.feature_test_targets(metadata, "seedbox", []), [])

    def test_kvm_suite_needs_no_root(self):
        self.assertEqual(self.linux("linux-kvm", {"kvm": ""}, root=False), [])

    def test_apps_suite_reports_missing_root(self):
        missing = self.linux("linux-apps", {"kvm": ""}, root=False)
        self.assertIn("requires root or passwordless sudo", missing)

    def test_guest_suite_requires_its_inputs(self):
        self.assertTrue(self.linux("linux-guests", {"kvm": ""}))
        with tempfile.TemporaryDirectory() as directory:
            kernel, initrd = Path(directory) / "kernel", Path(directory) / "initrd"
            kernel.touch()
            initrd.touch()
            self.assertEqual(self.linux("linux-guests", {"kvm": ""}, inputs={
                "SEEDBOX_TEST_KERNEL": str(kernel), "SEEDBOX_TEST_INITRD": str(initrd)}), [])

    def test_oci_suite_needs_an_engine_but_no_kvm_or_root(self):
        self.assertEqual(self.linux("linux-oci", {"kvm": "no such device"}, root=False), [])
        with patch.object(check.shutil, "which", return_value=None), \
                patch.object(check, "engine_sockets", return_value=[]):
            self.assertEqual(check.requirements("linux-oci", "Linux", "x86_64", {"DOCKER_HOST": "unix:///fixture"}), [])
            missing = check.requirements("linux-oci", "Linux", "x86_64", {})
        self.assertEqual(len(missing), 1)
        self.assertIn("no Docker-compatible engine", missing[0])

    def test_an_engine_socket_counts_without_binaries(self):
        # The CLI connects to known sockets itself, e.g. a Mac Podman machine.
        with tempfile.TemporaryDirectory() as directory, socket.socket(socket.AF_UNIX) as listener:
            path = Path(directory) / "machine-api.sock"
            listener.bind(str(path))
            with patch.object(check.shutil, "which", return_value=None), \
                    patch.object(check, "engine_sockets", return_value=[path]):
                self.assertTrue(check.container_engine({}))
        home = {"HOME": "/home/fixture", "XDG_RUNTIME_DIR": "/run/user/1000"}
        self.assertIn(Path("/home/fixture/.docker/run/docker.sock"), check.engine_sockets(home))
        self.assertIn(Path("/run/user/1000/podman/podman.sock"), check.engine_sockets(home))

    def test_a_root_only_engine_socket_needs_root_for_oci(self):
        with tempfile.TemporaryDirectory() as directory, socket.socket(socket.AF_UNIX) as listener:
            path = Path(directory) / "podman.sock"
            listener.bind(str(path))
            environment = {"DOCKER_HOST": f"unix://{path}"}
            self.assertFalse(check.engine_needs_root(environment))
            with patch.object(check.os, "access", return_value=False):
                self.assertTrue(check.engine_needs_root(environment))
                self.assertEqual(check.requirements("linux-oci", "Linux", "x86_64", environment, can_root=True), [])
                missing = check.requirements("linux-oci", "Linux", "x86_64", environment)
        self.assertEqual(len(missing), 1)
        self.assertIn("not writable by this user", missing[0])
        self.assertFalse(check.engine_needs_root({"DOCKER_HOST": "tcp://127.0.0.1:2375"}))

    def test_x86_only_suites_refuse_arm_linux(self):
        self.assertIn("requires Linux x86-64", self.linux("linux-oci", {"kvm": ""}, machine="aarch64"))
        self.assertEqual(self.linux("linux-kvm", {"kvm": ""}, machine="aarch64"), [])

    def test_platform_mismatch_is_unavailable(self):
        self.assertEqual(check.requirements("mac-hvf", "Linux", "x86_64", {}), ["requires macOS on Apple Silicon"])
        self.assertEqual(check.requirements("linux-kvm", "Darwin", "arm64", {}), ["requires Linux"])
        self.assertEqual(check.requirements("portable", "Darwin", "arm64", {}), [])

    def test_mac_hvf_requirements(self):
        with patch.object(check.platform, "mac_ver", return_value=("15.0", (), "")), \
             patch.object(check.shutil, "which", return_value="/fixture/tool"), \
             patch.object(check.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "1\n", "")):
            self.assertEqual(check.requirements("mac-hvf", "Darwin", "arm64", {}), [])
        with patch.object(check.platform, "mac_ver", return_value=("14.6", (), "")), \
             patch.object(check.shutil, "which", return_value="/fixture/tool"), \
             patch.object(check.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "1\n", "")):
            self.assertEqual(check.requirements("mac-hvf", "Darwin", "arm64", {}), ["requires macOS 15+"])

    def test_build_admission_needs_tools_not_cached_assets(self):
        with patch.object(check.shutil, "which", return_value="/tools/nix"):
            self.assertEqual(check.build_requirements({}, "Linux"), [])
            self.assertEqual(check.build_requirements({}, "Darwin"), [])
        with patch.object(check.shutil, "which", return_value=None):
            self.assertIn("nix", check.build_requirements({"SEEDBOX_ASSET_DIR": "/stale"}, "Linux")[0])
            self.assertIn("CLI", check.build_requirements({}, "Darwin")[0])

    def test_complete_build_replaces_ambient_runtime_overrides(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            environment = {key: "/stale" for key in ("SEEDBOX_ASSET_DIR", "SEEDBOX_BUILD_HELPERS", "SEEDBOX_GUEST_INIT", "SEEDBOX_MKE2FS", "SEEDBOX_VMM", "SEEDBOX_KERNEL", "SEEDBOX_RUNTIME_ASSETS")}
            with self.assertRaises(ValueError): check.select_build(environment, root)
            for name in ("seedbox", "kernel", "runtime.tar.gz"): (root / name).touch()
            check.select_build(environment, root)
            self.assertEqual(environment, {"SEEDBOX_TEST_CLI": str(root / "seedbox"), "SEEDBOX_KERNEL": str(root / "kernel"), "SEEDBOX_RUNTIME_ASSETS": str(root / "runtime.tar.gz")})

    def test_privileged_execution_forwards_asset_directory_and_engine(self):
        command = check.privileged(["sudo", "-n", "env"], ["/test"], {
            "SEEDBOX_RUNTIME_ASSETS": "/assets", "SEEDBOX_ASSET_DIR": "/slot",
            "DOCKER_HOST": "unix:///engine", "GH_TOKEN": "fixture"})
        self.assertEqual(command, ["sudo", "-n", "env", "SEEDBOX_RUNTIME_ASSETS=/assets",
                                   "SEEDBOX_ASSET_DIR=/slot", "DOCKER_HOST=unix:///engine", "/test"])

    def test_unmet_selected_suite_fails_before_any_build(self):
        with patch.object(check, "requirements", return_value=["no fixture host"]), \
             patch.object(check, "root_prefix", return_value=None), \
             patch.object(check.platform, "system", return_value="Darwin"), \
             patch.object(check.Checks, "run", side_effect=AssertionError("build before admission")), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(check.main(["--list", "mac-hvf"]), 2)
            self.assertEqual(check.main(["mac-hvf"]), 2)

    def test_unknown_suite_is_a_usage_error(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as raised:
                check.main(["linux-everything"])
            self.assertEqual(raised.exception.code, 2)


class ExecutionTests(unittest.TestCase):
    def test_discovers_all_test_artifacts_and_ignores_build_outputs(self):
        def artifact(path, name, test):
            return json.dumps(dict(reason="compiler-artifact", executable=path,
                                   target=dict(name=name), profile=dict(test=test)))
        output = "\n".join([artifact("/new/test-a", "new-a", True),
                            artifact("/new/test-b", "new-b", True),
                            artifact("/cli", "cli", False),
                            artifact("/new/test-a", "new-a", True)])
        self.assertEqual(check.test_artifacts(output), [("/new/test-a", "new-a"), ("/new/test-b", "new-b")])

    def test_signed_copies_are_executed_after_build_and_signing(self):
        with tempfile.TemporaryDirectory() as directory:
            original = Path(directory) / "test-binary"
            original.write_text("fixture binary")
            output = json.dumps(dict(reason="compiler-artifact", executable=str(original),
                                     target=dict(name="new-hardware"), profile=dict(test=True)))
            checks = check.Checks(dict(os.environ, CARGO_TARGET_DIR="/workspace-cache"))
            with patch.object(checks, "run", side_effect=[output, True, True]) as run:
                checks.cargo_tests("seedbox-arm64", ["cargo", "test-arm64"], ["hvf-tests"], None, sign=True)
                build, sign, execute = run.call_args_list
                self.assertNotIn("CARGO_TARGET_DIR", build.kwargs["environment"])
                signed = sign.args[1][-1]
                self.assertNotEqual(signed, str(original))
                self.assertEqual(execute.args[1], [signed, "--test-threads=1"])
            self.assertEqual(original.read_text(), "fixture binary")

    def test_privileged_execution_forwards_only_explicit_test_inputs(self):
        command = check.privileged(["sudo", "-n", "env"], ["/test"],
                                   {"SEEDBOX_TEST_KERNEL": "/kernel", "UNRELATED_SECRET": "fixture"})
        self.assertEqual(command, ["sudo", "-n", "env", "SEEDBOX_TEST_KERNEL=/kernel", "/test"])

    def test_a_failed_step_does_not_hide_later_checks(self):
        with tempfile.TemporaryDirectory() as directory, tempfile.TemporaryFile(mode="w+") as log, \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(log):
            marker = Path(directory) / "later"
            checks = check.Checks(os.environ.copy())
            self.assertFalse(checks.run("failed", [sys.executable, "-c", "raise SystemExit(7)"]))
            self.assertTrue(checks.run("later", [sys.executable, "-c", "from pathlib import Path; import sys; Path(sys.argv[1]).touch()", str(marker)]))
            self.assertTrue(checks.failed)
            self.assertTrue(marker.exists())


if __name__ == "__main__":
    unittest.main()
