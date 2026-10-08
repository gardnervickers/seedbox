"""Capability selection and discovery controls; no hypervisor or MSR is opened."""
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
        # Checkout-local artifacts must not change these capability controls.
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = patch.object(check, "ROOT", Path(directory.name))
        root.start()
        self.addCleanup(root.stop)

    def linux(self, probe, root=True, inputs=None):
        with patch.object(check.shutil, "which", return_value="/fixture/tool"):
            return check.discover("Linux", "x86_64", inputs or {}, probe, root)

    def test_kvm_without_pmu_keeps_all_generic_kvm_tests(self):
        found = self.linux({"kvm": "", "counter": "guest PMU unavailable"})
        self.assertEqual(check.features_for("seedbox-vmm", "Linux", "x86_64", found), ["kvm-tests"])
        self.assertIn("oci-tests", check.features_for("seedbox", "Linux", "x86_64", found))
        self.assertEqual(check.features_for("seedbox-arm64", "Linux", "x86_64", found), [])

    def test_amd_and_intel_use_the_same_counter_contract(self):
        found = self.linux({"kvm": "", "counter": ""})
        self.assertEqual(check.features_for("seedbox-vmm", "Linux", "x86_64", found),
                         ["kvm-tests", "counter-tests"])

    def test_missing_privileges_cannot_select_root_tests(self):
        found = self.linux({"kvm": "", "counter": ""}, root=False)
        self.assertTrue(found["counter"])
        self.assertTrue(found["oci"])
        self.assertEqual(check.features_for("seedbox-vmm", "Linux", "x86_64", found), ["kvm-tests"])

    def test_guest_inputs_add_coverage_without_a_new_test_list(self):
        with tempfile.TemporaryDirectory() as directory:
            kernel, initrd = Path(directory) / "kernel", Path(directory) / "initrd"
            kernel.touch()
            initrd.touch()
            found = self.linux({"kvm": "", "counter": ""}, inputs={
                "SEEDBOX_TEST_KERNEL": str(kernel), "SEEDBOX_TEST_INITRD": str(initrd)})
            self.assertIn("guest-tests", check.features_for("seedbox-vmm", "Linux", "x86_64", found))

    def test_arm_kvm_does_not_select_x86_counter_tests(self):
        with patch.object(check.shutil, "which", return_value="/fixture/tool"):
            found = check.discover("Linux", "aarch64", {},
                                   {"kvm": "", "counter": "requires x86-64"}, True)
        self.assertEqual(check.features_for("seedbox-arm64", "Linux", "aarch64", found), ["kvm-tests"])
        self.assertTrue(found["oci"])
        self.assertTrue(found["retained-run"])

    def test_mac_selects_hvf_primitives_without_guest_inputs(self):
        with patch.object(check.platform, "mac_ver", return_value=("15.0", (), "")), \
             patch.object(check.shutil, "which", return_value="/fixture/tool"), \
             patch.object(check.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "1\n", "")):
            found = check.discover("Darwin", "arm64", {})
        self.assertEqual(check.features_for("seedbox-arm64", "Darwin", "arm64", found), ["hvf-tests"])
        self.assertTrue(found["guests"])
        self.assertTrue(found["counter"])

    def test_explicit_required_capability_cannot_silently_skip(self):
        unavailable = {name: "no fixture capability" for name in check.CAPABILITIES}
        with patch.object(check, "discover", return_value=unavailable), \
             patch.object(check, "root_prefix", return_value=None), \
             patch.object(check.platform, "system", return_value="Darwin"), \
             patch.object(check.Checks, "run", side_effect=AssertionError("build before admission")), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(check.main(["--list", "--require", "kvm"]), 2)
            self.assertEqual(check.main(["--require", "kvm"]), 2)


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
