#!/usr/bin/env python3
"""Discover host capabilities, select Cargo test features, and run native suites.

Tests belong to Cargo or unittest discovery, not a list of individual test names.
Application assertions remain in guests; applications.rs uses run/replay directly.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
CAPABILITIES = ("kvm", "counter", "hvf", "oci", "guests", "applications", "retained-run")


def probe_linux():
    """Open/close KVM and a disabled guest-only PMU event, never execute guests."""
    import fcntl
    result = {}
    try:
        fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
        try:
            if fcntl.ioctl(fd, 0xAE00) != 12:
                raise OSError("unsupported KVM API")
        finally:
            os.close(fd)
        result["kvm"] = ""
    except OSError as error:
        result["kvm"] = str(error)
    result["counter"] = "requires Linux x86-64 KVM with a guest PMU"
    if platform.machine() == "x86_64" and not result["kvm"]:
        # PERF_COUNT_HW_BRANCH_INSTRUCTIONS tests guest PMU availability. The
        # engine still selects and tests its exact vendor-specific raw event.
        attr = ctypes.create_string_buffer(128)
        struct.pack_into("IIQQQQQ", attr, 0, 0, 128, 5, 1 << 62, 0, 3,
                         1 | (1 << 2) | (1 << 19))
        libc = ctypes.CDLL(None, use_errno=True)
        fd = libc.syscall(298, ctypes.byref(attr), 0, -1, -1, 8)
        if fd >= 0:
            os.close(fd)
            result["counter"] = ""
        else:
            result["counter"] = f"guest perf_event_open: {os.strerror(ctypes.get_errno())}"
    return result


def root_prefix():
    if os.geteuid() == 0:
        return []
    if shutil.which("sudo") and subprocess.run(
            ["sudo", "-n", "true"], stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, check=False).returncode == 0:
        return ["sudo", "-n", "env", f"PATH={os.environ['PATH']}"]
    return None


def privileged(prefix, command, environment):
    if not prefix:
        return command
    # sudo discards these explicit fixture paths by default. Forward only the
    # test/runtime inputs, not the user's general environment or credentials.
    inputs = [f"{key}={value}" for key, value in environment.items()
              if key.startswith(("SEEDBOX_TEST_", "SEEDBOX_ARM64_", "SEEDBOX_HOST_PAUSE_"))
              or key in ("SEEDBOX_KERNEL", "SEEDBOX_RUNTIME_ASSETS", "SEEDBOX_BUSYBOX",
                         "SEEDBOX_MKE2FS", "SEEDBOX_STATIC_CC", "DOCKER_HOST", "SEEDBOX_ASSET_DIR")]
    return [*prefix, *inputs, *command]


def available_file(environment, key, default):
    path = Path(environment.get(key, default)).resolve()
    return path if path.is_file() else None


def discover(system, machine, environment, privileged_probe=None, can_root=False):
    """Return an explanation for every unavailable capability; empty means ready."""
    found = {name: "not supported on this host" for name in CAPABILITIES}
    linux = system == "Linux"
    if linux and machine in ("x86_64", "aarch64", "arm64"):
        found.update(privileged_probe or probe_linux())
        tools = [] if environment.get("DOCKER_HOST") or any(shutil.which(name) for name in ("docker", "podman")) else ["Docker-compatible engine"]
        found["oci"] = ("CLI image-preparation tests require Linux x86-64" if machine != "x86_64" else
                        "missing tools: " + ", ".join(tools) if tools else "")
        if machine == "x86_64" and not found["counter"] and not can_root:
            found["counter"] = "root or passwordless sudo is required for the leased counter suite"
    if system == "Darwin" and machine in ("arm64", "aarch64"):
        version = int((platform.mac_ver()[0] or "0").split(".")[0])
        probe = subprocess.run(["sysctl", "-n", "kern.hv_support"],
                               capture_output=True, text=True, check=False)
        found["hvf"] = ("requires macOS 15+, Hypervisor.framework and codesign"
                        if version < 15 or probe.stdout.strip() != "1" or not shutil.which("codesign") else "")
        if probe.returncode != 0:
            found["hvf"] = "sysctl kern.hv_support: " + probe.stderr.strip()
    if machine == "x86_64" and linux:
        kernel = available_file(environment, "SEEDBOX_TEST_KERNEL", ROOT / "artifacts/bzImage")
        initrd = available_file(environment, "SEEDBOX_TEST_INITRD", ROOT / "artifacts/entropy-fork.cpio")
        found["guests"] = (found["counter"] or
                           "set SEEDBOX_TEST_KERNEL and SEEDBOX_TEST_INITRD (entropy-fork fixture)"
                           if not kernel or not initrd else found["counter"])
    elif machine in ("arm64", "aarch64"):
        kernel = available_file(environment, "SEEDBOX_ARM64_KERNEL", ROOT / "artifacts/arm64-hres/Image")
        initrd = available_file(environment, "SEEDBOX_ARM64_INITRD", ROOT / "artifacts/arm64-hres/init.cpio")
        native = found["hvf"] if system == "Darwin" else found["kvm"]
        found["guests"] = (native or "set SEEDBOX_ARM64_KERNEL and SEEDBOX_ARM64_INITRD"
                           if not kernel or not initrd else native)
    if linux and machine == "x86_64":
        found["applications"] = found["kvm"] or found["oci"] or ("root or passwordless sudo is required" if not can_root else "")
    elif system == "Darwin" and machine in ("arm64", "aarch64"):
        found["applications"] = (found["hvf"] or
                                 ("Docker or Podman is required" if not any(
                                     shutil.which(tool) for tool in ("docker", "podman")) else ""))
    if linux and machine == "x86_64" and not found["kvm"]:
        found["retained-run"] = ("set SEEDBOX_HOST_PAUSE_RUN and SEEDBOX_HOST_PAUSE_STATE"
                                 if not all(environment.get(k) for k in ("SEEDBOX_HOST_PAUSE_RUN", "SEEDBOX_HOST_PAUSE_STATE"))
                                 else "" if can_root else "root or passwordless sudo is required")
    return found


def features_for(package, system, machine, found):
    if package == "seedbox":
        return [feature for capability, feature in (("oci", "oci-tests"), ("applications", "application-tests"),
                                                     ("retained-run", "retained-run-tests")) if not found[capability]]
    if package == "seedbox-vmm":
        return [feature for capability, feature in (("kvm", "kvm-tests"), ("counter", "counter-tests"),
                                                     ("guests", "guest-tests")) if not found[capability]]
    native = "hvf" if system == "Darwin" else "kvm"
    return ([native + "-tests"] if machine in ("arm64", "aarch64") and not found[native] else []) + (
        ["guest-tests"] if machine in ("arm64", "aarch64") and not found["guests"] else [])


def test_artifacts(output):
    return sorted({item["executable"]: item["target"]["name"]
                   for line in output.splitlines() if line.strip()
                   for item in [json.loads(line)]
                   if item.get("reason") == "compiler-artifact"
                   and item.get("profile", {}).get("test") and item.get("executable")}.items())


class Checks:
    def __init__(self, environment):
        self.environment = environment
        self.failed = False

    def run(self, name, command, *, capture=False, environment=None, timeout=None):
        started = time.monotonic()
        display = [str(value) if not str(value).startswith("PATH=") else "PATH=<development shell>"
                   for value in command]
        print(f"running: {name}: {shlex.join(display)}", file=sys.stderr, flush=True)
        output = ""
        try:
            result = subprocess.run(command, cwd=ROOT, env=environment or self.environment,
                                    stdout=subprocess.PIPE if capture else sys.stderr,
                                    text=True, check=False, timeout=timeout)
            passed = result.returncode == 0
            output = result.stdout or ""
            if capture:
                for line in output.splitlines():
                    try:
                        message = json.loads(line)
                    except json.JSONDecodeError:
                        if not passed:
                            print(line, file=sys.stderr)
                        continue
                    if message.get("reason") == "compiler-message":
                        print(message["message"].get("rendered", ""), file=sys.stderr, end="")
        except (OSError, subprocess.TimeoutExpired) as error:
            print(f"error: {error}", file=sys.stderr)
            passed = False
        self.failed |= not passed
        print(f"{name}: {'PASS' if passed else 'FAIL'} ({time.monotonic()-started:.1f}s)", flush=True)
        return output if capture and passed else passed if not capture else None

    def cargo_tests(self, package, command, features, prefix, *, counter=False, sign=False):
        environment = self.environment.copy()
        if package != "seedbox":
            environment.pop("CARGO_TARGET_DIR", None)
        if features:
            command = [*command, "--features", ",".join(features)]
        output = self.run(package + "-build", [*command, "--no-run", "--message-format=json"],
                          capture=True, environment=environment)
        if output is None:
            return
        artifacts = test_artifacts(output)
        if not artifacts:
            raise ValueError(f"{package}: Cargo produced no test executables")
        with tempfile.TemporaryDirectory(prefix="seedbox-native-tests-") as temporary:
            for index, (binary, target) in enumerate(artifacts):
                if sign:
                    copied = Path(temporary) / f"{index}-{Path(binary).name}"
                    shutil.copy2(binary, copied)
                    if not self.run("sign-" + target, ["bash", "crates/seedbox-arm64/codesign.sh", str(copied)]):
                        continue
                    binary = str(copied)
                command = [binary, "--test-threads=1"]
                if target == "applications":
                    command.append("--nocapture")
                needs_root = package == "seedbox" and target in ("applications", "host_pause")
                if counter:
                    command = [sys.executable, str(ROOT / "harness/check-counter.py"),
                               *(["--cpu", environment["SEEDBOX_TEST_CPU"]] if environment.get("SEEDBOX_TEST_CPU") else []),
                               "--", *command]
                if counter or needs_root or (package in ("seedbox-arm64", "seedbox-vmm") and "kvm-tests" in features):
                    command = privileged(prefix, command, environment)
                self.run(package + "/" + target, command, environment=environment,
                         timeout=None if counter or target == "applications" else 1800)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", nargs="?", choices=("auto", "portable"), default="auto")
    parser.add_argument("--require", action="append", choices=CAPABILITIES, default=[],
                        help="Fail if this capability is unavailable (repeatable).")
    parser.add_argument("--cpu", type=int, help="Use this Linux CPU for the leased counter tests.")
    parser.add_argument("--list", action="store_true", help="Show selected capabilities without building or running tests.")
    parser.add_argument("--probe-linux", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.probe_linux:
        print(json.dumps(probe_linux()))
        return 0
    if args.cpu is not None and args.cpu < 0:
        parser.error("--cpu must be nonnegative")
    if args.mode == "portable" and args.require:
        parser.error("--require needs auto mode")
    system, machine = platform.system(), platform.machine()
    environment = os.environ.copy()
    prefix = root_prefix() if system == "Linux" and args.mode == "auto" else None
    probe = None
    if system == "Linux" and args.mode == "auto":
        result = subprocess.run(privileged(prefix, [sys.executable, str(Path(__file__).resolve()), "--probe-linux"], environment),
                                capture_output=True, text=True, check=True)
        probe = json.loads(result.stdout)
    found = (discover(system, machine, environment, probe, prefix is not None)
             if args.mode == "auto" else {name: "portable mode" for name in CAPABILITIES})
    print(f"platform: {system}/{machine}")
    for name, reason in found.items():
        print(f"capability {name}: {'unavailable: ' + reason if reason else 'enabled'}")
    missing = [name for name in args.require if found[name]]
    if missing:
        print("error: required capabilities unavailable: " + ", ".join(missing), file=sys.stderr)
        return 2
    if args.list:
        return 0
    checks = Checks(environment)
    checks.run("fmt", ["cargo", "fmt", "-p", "seedbox", "--check"])
    checks.run("clippy", ["cargo", "clippy", "-p", "seedbox", "--all-targets", "--locked", "--", "-D", "warnings"])
    checks.run("portable", ["bash", "harness/acceptance.sh", "portable"])
    # acceptance.sh already ran the root and native backend without hardware.
    if args.mode == "portable" or checks.failed:
        return int(checks.failed)
    reports_root = ROOT / "target/check-reports"
    reports_root.mkdir(parents=True, exist_ok=True)
    reports = Path(tempfile.mkdtemp(prefix="run-", dir=reports_root))
    print(f"reports: {reports}")
    environment["SEEDBOX_TEST_REPORT_DIR"] = str(reports)
    if args.cpu is not None:
        environment["SEEDBOX_TEST_CPU"] = str(args.cpu)
    for key, default in (("SEEDBOX_TEST_KERNEL", "artifacts/bzImage"),
                         ("SEEDBOX_TEST_INITRD", "artifacts/entropy-fork.cpio"),
                         ("SEEDBOX_ARM64_KERNEL", "artifacts/arm64-hres/Image"),
                         ("SEEDBOX_ARM64_INITRD", "artifacts/arm64-hres/init.cpio")):
        if (path := available_file(environment, key, ROOT / default)) is not None:
            environment[key] = str(path)
    environment["SEEDBOX_ARM64_REPORT_DIR"] = str(reports / "arm64")
    if not found["applications"]:
        output = checks.run("application-cli", ["cargo", "build", "--release", "--locked", "-p", "seedbox", "--message-format=json"], capture=True)
        if output is not None:
            binaries = [json.loads(line) for line in output.splitlines() if line.strip()]
            binary = next(item["executable"] for item in binaries if item.get("executable") and item.get("target", {}).get("name") == "seedbox")
            environment["SEEDBOX_TEST_CLI"] = binary
            environment["SEEDBOX_TEST_COUNTER"] = "0" if found["counter"] else "1"
        else:
            found["applications"] = "application CLI build failed"
    packages = [("seedbox-arm64", ["cargo", "test-arm64"])]
    if system == "Linux" and machine == "x86_64":
        packages.append(("seedbox-vmm", ["cargo", "test-vmm"]))
    packages.append(("seedbox", ["cargo", "test", "--workspace", "--locked"]))
    for package, command in packages:
        features = features_for(package, system, machine, found)
        if features:
            checks.cargo_tests(package, command, features, prefix,
                               counter=package == "seedbox-vmm" and "counter-tests" in features,
                               sign=package == "seedbox-arm64" and "hvf-tests" in features)
    return int(checks.failed)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"check: {error}")
