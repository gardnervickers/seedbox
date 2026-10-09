"""Suite selection and requirement controls; no hypervisor or MSR is opened."""
import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
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
        with patch.object(check.shutil, "which", return_value="/fixture/tool"):
            return check.requirements(suite, "Linux", machine, inputs or {}, probe, root)

    def test_default_is_portable_only(self):
        self.assertEqual(check.expand(["portable"], "x86_64"), ["portable"])

    def test_groups_and_implied_suites_expand_in_documented_order(self):
        self.assertEqual(check.expand(["linux"], "x86_64"),
                         ["portable", "linux-kvm", "linux-counter", "linux-oci", "linux-apps"])
        self.assertEqual(check.expand(["mac"], "arm64"), ["portable", "mac-hvf", "mac-apps"])
        self.assertEqual(check.expand(["linux-guests"], "x86_64"),
                         ["portable", "linux-kvm", "linux-counter", "linux-guests"])
        self.assertEqual(check.expand(["linux-guests"], "aarch64"), ["portable", "linux-kvm", "linux-guests"])
        self.assertEqual(check.expand(["mac-apps"], "arm64"), ["portable", "mac-hvf", "mac-apps"])

    def test_features_follow_suites_not_host_probes(self):
        self.assertEqual(check.features_for("seedbox-vmm", "x86_64", ["portable", "linux-kvm"]), ["kvm-tests"])
        self.assertEqual(check.features_for("seedbox-vmm", "x86_64", check.expand(["linux-counter"], "x86_64")),
                         ["kvm-tests", "counter-tests"])
        self.assertEqual(check.features_for("seedbox", "x86_64", ["portable", "linux-oci", "linux-pause"]),
                         ["oci-tests", "retained-run-tests"])
        self.assertEqual(check.features_for("seedbox-arm64", "x86_64", check.expand(["linux"], "x86_64")), [])
        self.assertEqual(check.features_for("seedbox-arm64", "aarch64", check.expand(["linux-guests"], "aarch64")),
                         ["kvm-tests", "guest-tests"])
        self.assertEqual(check.features_for("seedbox-arm64", "arm64", check.expand(["mac-guests"], "arm64")),
                         ["hvf-tests", "guest-tests"])
        self.assertEqual(check.features_for("seedbox-vmm", "arm64", check.expand(["mac"], "arm64")), [])

    def test_kvm_suite_needs_no_pmu_or_root(self):
        self.assertEqual(self.linux("linux-kvm", {"kvm": "", "counter": "guest PMU unavailable"}, root=False), [])

    def test_counter_suite_reports_missing_pmu_and_root(self):
        missing = self.linux("linux-counter", {"kvm": "", "counter": "guest perf_event_open: ENOENT"}, root=False)
        self.assertIn("guest perf_event_open: ENOENT", missing)
        self.assertIn("requires root or passwordless sudo", missing)

    def test_guest_suite_requires_its_inputs(self):
        self.assertTrue(self.linux("linux-guests", {"kvm": "", "counter": ""}))
        with tempfile.TemporaryDirectory() as directory:
            kernel, initrd = Path(directory) / "kernel", Path(directory) / "initrd"
            kernel.touch()
            initrd.touch()
            self.assertEqual(self.linux("linux-guests", {"kvm": "", "counter": ""}, inputs={
                "SEEDBOX_TEST_KERNEL": str(kernel), "SEEDBOX_TEST_INITRD": str(initrd)}), [])

    def test_oci_suite_needs_no_kvm(self):
        self.assertEqual(self.linux("linux-oci", {"kvm": "no such device", "counter": "no KVM"}), [])

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

    def test_apps_need_a_token_cache_or_explicit_assets(self):
        with tempfile.TemporaryDirectory() as directory:
            home = {"HOME": directory}
            missing = self.linux("linux-apps", {"kvm": ""}, inputs=home)
            self.assertEqual(len(missing), 1)
            self.assertIn("set GH_TOKEN", missing[0])
            self.assertIn(f"{directory}/.cache/seedbox/assets/x86_64/ready/runtime.tar.gz", missing[0])
            self.assertEqual(self.linux("linux-apps", {"kvm": ""}, inputs={**home, "GH_TOKEN": "fixture"}), [])
            self.assertIn("requires root or passwordless sudo",
                          self.linux("linux-apps", {"kvm": ""}, root=False, inputs={**home, "GITHUB_TOKEN": "fixture"}))
            for kind, name in (("kernels", "kernel"), ("assets", "runtime.tar.gz")):
                ready = Path(directory) / "Library/Caches/seedbox" / kind / "aarch64" / "ready"
                ready.mkdir(parents=True)
                (ready / name).touch()
            with patch.object(check.shutil, "which", side_effect=lambda tool: None):
                self.assertEqual(check.requirements("mac-apps", "Darwin", "arm64", home), ["Docker or Podman is required"])
            with patch.object(check.shutil, "which", side_effect=lambda tool: "/fixture/podman" if tool == "podman" else None):
                self.assertEqual(check.requirements("mac-apps", "Darwin", "arm64", home), [])
            kernel, assets = Path(directory) / "kernel", Path(directory) / "assets"
            kernel.touch()
            assets.touch()
            self.assertEqual(self.linux("linux-apps", {"kvm": ""}, inputs={
                **home, "SEEDBOX_KERNEL": str(kernel), "SEEDBOX_RUNTIME_ASSETS": str(assets)}), [])

    def test_privileged_execution_forwards_runtime_assets_cache_and_token(self):
        command = check.privileged(["sudo", "-n", "env"], ["/test"], {
            "SEEDBOX_RUNTIME_ASSETS": "/assets", "XDG_CACHE_HOME": "/cache", "GH_TOKEN": "fixture"})
        self.assertEqual(command, ["sudo", "-n", "env", "SEEDBOX_RUNTIME_ASSETS=/assets",
                                   "XDG_CACHE_HOME=/cache", "GH_TOKEN=fixture", "/test"])

    def test_unmet_selected_suite_fails_before_any_build(self):
        with patch.object(check, "requirements", return_value=["no fixture host"]), \
             patch.object(check, "root_prefix", return_value=None), \
             patch.object(check.platform, "system", return_value="Darwin"), \
             patch.object(check.Checks, "run", side_effect=AssertionError("build before admission")), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(check.main(["--list", "mac-hvf"]), 2)
            self.assertEqual(check.main(["mac-hvf"]), 2)

    def test_unknown_suite_and_misplaced_cpu_are_usage_errors(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as raised:
                check.main(["linux-everything"])
            self.assertEqual(raised.exception.code, 2)
            with self.assertRaises(SystemExit) as raised:
                check.main(["linux-kvm", "--cpu", "3"])
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
