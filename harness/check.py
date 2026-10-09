#!/usr/bin/env python3
"""Run the named platform test suites; nothing is selected by probing the host.

Each suite names a platform and the Cargo test features it enables. A selected
suite whose host requirements are missing is an error before anything builds.
Tests belong to Cargo or unittest discovery, not a list of individual test names.
Application assertions remain in guests; applications.rs uses run/replay directly.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]

SUITES = {
    "portable": "Any host: fmt, clippy, workspace and standalone unit tests, Python suites, YAML validation.",
    "linux-kvm": "Linux x86-64 or ARM64 with /dev/kvm: native backend KVM tests; no PMU or root needed.",
    "linux-counter": "Linux x86-64 KVM with a guest PMU and root: counted stops, checkpoints, forks, logs and triggers on a leased core.",
    "linux-guests": "Fixture kernel and initrd on Linux KVM (x86-64 also needs linux-counter): real guest determinism and checkpoints.",
    "linux-oci": "Linux x86-64, root, Skopeo, umoci and Podman: real image import and Dockerfile builds.",
    "linux-apps": "Linux x86-64 KVM, root and OCI tools: every acceptance workload run twice, diffed and replayed (PV, plus counter with linux-counter).",
    "linux-pause": "Linux x86-64 KVM, root and a supplied retained run: pause/resume replay.",
    "mac-hvf": "Apple Silicon with macOS 15+, Hypervisor.framework and codesign: native CPU/state and cancellation primitives.",
    "mac-guests": "mac-hvf plus the ARM fixture kernel and initrd: real guest controls.",
    "mac-apps": "mac-hvf plus Docker or Podman: every acceptance workload on ARM PV.",
}
# Groups hold the suites a set-up host can run with no hand-built fixtures;
# the guest and pause suites are always named explicitly.
GROUPS = {
    "linux": ["linux-kvm", "linux-counter", "linux-oci", "linux-apps"],
    "mac": ["mac-hvf", "mac-apps"],
}
# A suite also runs the suites its Cargo features build on.
IMPLIES = {
    "linux-counter": ["linux-kvm"],
    "mac-guests": ["mac-hvf"],
    "mac-apps": ["mac-hvf"],
}


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
    # test/runtime inputs and the release download's cache and token, not the
    # user's general environment.
    inputs = [f"{key}={value}" for key, value in environment.items()
              if key.startswith(("SEEDBOX_TEST_", "SEEDBOX_ARM64_", "SEEDBOX_HOST_PAUSE_"))
              or key in ("SEEDBOX_KERNEL", "SEEDBOX_RUNTIME_ASSETS", "SEEDBOX_BUSYBOX",
                         "SEEDBOX_MKE2FS", "SEEDBOX_STATIC_CC", "XDG_CACHE_HOME",
                         "GH_TOKEN", "GITHUB_TOKEN")]
    return [*prefix, *inputs, *command]


def cache_home(environment, mac):
    """The cache directory the release CLI uses for this user (cli/backends.rs)."""
    if environment.get("XDG_CACHE_HOME"):
        return Path(environment["XDG_CACHE_HOME"])
    return Path(environment.get("HOME", "~")).expanduser() / ("Library/Caches" if mac else ".cache")


def launch_format():
    """The release cache key the CLI built from this checkout uses (src/guest.rs)."""
    source = (Path(__file__).resolve().parents[1] / "crates/seedbox/src/guest.rs").read_text()
    return re.search(r'LAUNCH_MAGIC: &\[u8; \d+\] = b"(\w+)"', source).group(1).lower()


def release_inputs(environment, mac, machine):
    """Unmet guest kernel and runtime asset sources for the release CLI."""
    if environment.get("GH_TOKEN") or environment.get("GITHUB_TOKEN"):
        return []
    architecture = "aarch64" if machine == "arm64" else machine
    cache = cache_home(environment, mac) / "seedbox"
    missing = []
    for key, kind, name in (("SEEDBOX_KERNEL", "kernels", "kernel"),
                            ("SEEDBOX_RUNTIME_ASSETS", "assets", "runtime.tar.gz")):
        ready = cache / kind / launch_format() / architecture / "ready" / name
        path = Path(environment[key]) if environment.get(key) else ready
        if not path.is_file():
            missing.append(f"{key} or {path}")
    if missing:
        return ["guest assets are not available (" + ", ".join(missing) + "); "
                "set GH_TOKEN for the private release download, or set the paths"]
    return []


def available_file(environment, key, default):
    path = Path(environment.get(key, default)).resolve()
    return path if path.is_file() else None


def expand(names, machine):
    """Expand groups and implied suites, keeping the documented suite order."""
    chosen = set()
    pending = [suite for name in names for suite in GROUPS.get(name, [name])]
    while pending:
        suite = pending.pop()
        if suite in chosen:
            continue
        chosen.add(suite)
        pending.extend(IMPLIES.get(suite, []))
        if suite == "linux-guests":
            pending.append("linux-counter" if machine == "x86_64" else "linux-kvm")
    chosen.add("portable")
    return [suite for suite in SUITES if suite in chosen]


def requirements(suite, system, machine, environment, probe=None, can_root=False):
    """Return every unmet host requirement for one suite; empty means runnable."""
    linux, mac = system == "Linux", system == "Darwin" and machine in ("arm64", "aarch64")
    x86 = linux and machine == "x86_64"
    probe = probe or {}
    missing = []

    def need(condition, reason):
        if not condition:
            missing.append(reason)

    def inputs(*keys):
        defaults = {"SEEDBOX_TEST_KERNEL": "artifacts/bzImage",
                    "SEEDBOX_TEST_INITRD": "artifacts/entropy-fork.cpio",
                    "SEEDBOX_ARM64_KERNEL": "artifacts/arm64-hres/Image",
                    "SEEDBOX_ARM64_INITRD": "artifacts/arm64-hres/init.cpio"}
        for key in keys:
            need(available_file(environment, key, ROOT / defaults[key]),
                 f"set {key} (default {defaults[key]} is missing)")

    def oci_tools():
        tools = [name for name in ("skopeo", "umoci", "podman") if not shutil.which(name)]
        need(not tools, "missing tools: " + ", ".join(tools) + "; enter nix develop")

    if suite == "portable":
        return missing
    if suite.startswith("linux-"):
        if not linux:
            return ["requires Linux"]
        if suite != "linux-kvm" and suite != "linux-guests":
            need(x86, "requires Linux x86-64")
        if suite != "linux-oci":  # Image import and builds need no hypervisor.
            need(not probe.get("kvm"), "/dev/kvm: " + (probe.get("kvm") or ""))
        if suite in ("linux-counter", "linux-oci", "linux-apps", "linux-pause"):
            need(can_root, "requires root or passwordless sudo")
        if suite == "linux-kvm":
            need(machine in ("x86_64", "aarch64"), "requires Linux x86-64 or ARM64")
        elif suite == "linux-counter":
            need(not probe.get("counter"), probe.get("counter") or "")
        elif suite == "linux-guests":
            inputs(*(("SEEDBOX_TEST_KERNEL", "SEEDBOX_TEST_INITRD") if x86 else
                     ("SEEDBOX_ARM64_KERNEL", "SEEDBOX_ARM64_INITRD")))
        elif suite == "linux-oci":
            oci_tools()
        elif suite == "linux-apps":
            oci_tools()
            missing.extend(release_inputs(environment, False, machine))
        elif suite == "linux-pause":
            for key in ("SEEDBOX_HOST_PAUSE_RUN", "SEEDBOX_HOST_PAUSE_STATE"):
                need(environment.get(key), f"set {key}")
        return [reason for reason in missing if reason]
    if not mac:
        return ["requires macOS on Apple Silicon"]
    if suite == "mac-hvf":
        version = int((platform.mac_ver()[0] or "0").split(".")[0])
        need(version >= 15, "requires macOS 15+")
        probe = subprocess.run(["sysctl", "-n", "kern.hv_support"],
                               capture_output=True, text=True, check=False)
        need(probe.returncode == 0 and probe.stdout.strip() == "1",
             "Hypervisor.framework unavailable (sysctl kern.hv_support)")
        need(shutil.which("codesign"), "codesign not found")
    elif suite == "mac-guests":
        inputs("SEEDBOX_ARM64_KERNEL", "SEEDBOX_ARM64_INITRD")
    elif suite == "mac-apps":
        need(any(shutil.which(tool) for tool in ("docker", "podman")), "Docker or Podman is required")
        missing.extend(release_inputs(environment, True, machine))
    return missing


def features_for(package, machine, suites):
    """Cargo test features for one package; selection depends only on the suites."""
    def pick(pairs):
        return [feature for suite, feature in pairs if suite in suites]
    if package == "seedbox":
        return pick((("linux-oci", "oci-tests"), ("linux-apps", "application-tests"),
                     ("mac-apps", "application-tests"), ("linux-pause", "retained-run-tests")))
    if package == "seedbox-vmm":
        return pick((("linux-kvm", "kvm-tests"), ("linux-counter", "counter-tests"),
                     ("linux-guests", "guest-tests"))) if machine == "x86_64" else []
    linux_arm = machine in ("arm64", "aarch64")
    features = pick((("mac-hvf", "hvf-tests"),))
    if linux_arm:
        features = pick((("linux-kvm", "kvm-tests"),)) + features
        if "linux-guests" in suites or "mac-guests" in suites:
            features.append("guest-tests")
    return features


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
                needs_root = package == "seedbox" and target in ("oci", "applications", "host_pause")
                if counter:
                    command = [sys.executable, str(ROOT / "harness/check-counter.py"),
                               *(["--cpu", environment["SEEDBOX_TEST_CPU"]] if environment.get("SEEDBOX_TEST_CPU") else []),
                               "--", *command]
                if counter or needs_root or (package in ("seedbox-arm64", "seedbox-vmm") and "kvm-tests" in features):
                    command = privileged(prefix, command, environment)
                self.run(package + "/" + target, command, environment=environment,
                         timeout=None if counter or target == "applications" else 1800)


def describe():
    width = max(map(len, [*SUITES, *GROUPS]))
    lines = ["suites:"] + [f"  {name:<{width}}  {text}" for name, text in SUITES.items()]
    lines += ["groups:"] + [f"  {name:<{width}}  {' '.join(members)}" for name, members in GROUPS.items()]
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog=describe(),
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("suites", nargs="*", metavar="SUITE",
                        help="Suites or groups to run [default: portable].")
    parser.add_argument("--cpu", type=int, help="Linux CPU for linux-counter and linux-apps.")
    parser.add_argument("--list", action="store_true",
                        help="Check the selected suites' requirements without building or running tests.")
    parser.add_argument("--probe-linux", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.probe_linux:
        print(json.dumps(probe_linux()))
        return 0
    unknown = [name for name in args.suites if name not in SUITES and name not in GROUPS]
    if unknown:
        parser.error("unknown suite: " + ", ".join(unknown) + "; see --help")
    system, machine = platform.system(), platform.machine()
    suites = expand(args.suites or ["portable"], machine)
    if args.cpu is not None:
        if args.cpu < 0:
            parser.error("--cpu must be nonnegative")
        if not {"linux-counter", "linux-apps"} & set(suites):
            parser.error("--cpu applies only to linux-counter and linux-apps")
    environment = os.environ.copy()
    hardware = [suite for suite in suites if suite != "portable"]
    prefix = root_prefix() if system == "Linux" and hardware else None
    probe = None
    if system == "Linux" and hardware:
        result = subprocess.run(privileged(prefix, [sys.executable, str(Path(__file__).resolve()), "--probe-linux"], environment),
                                capture_output=True, text=True, check=True)
        probe = json.loads(result.stdout)
    print(f"platform: {system}/{machine}")
    unmet = {}
    for suite in suites:
        missing = requirements(suite, system, machine, environment, probe, prefix is not None)
        print(f"suite {suite}: {'unavailable: ' + '; '.join(missing) if missing else 'ready'}")
        if missing:
            unmet[suite] = missing
    if unmet:
        print("error: selected suites cannot run on this host: " + ", ".join(unmet), file=sys.stderr)
        return 2
    if args.list:
        return 0
    checks = Checks(environment)
    checks.run("fmt", ["cargo", "fmt", "-p", "seedbox", "--check"])
    checks.run("clippy", ["cargo", "clippy", "-p", "seedbox", "--all-targets", "--locked", "--", "-D", "warnings"])
    checks.run("portable", ["bash", "harness/acceptance.sh", "portable"])
    # acceptance.sh already ran the root and native backend without hardware.
    if not hardware or checks.failed:
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
    if {"linux-apps", "mac-apps"} & set(suites):
        output = checks.run("application-cli", ["cargo", "build", "--release", "--locked", "-p", "seedbox", "--message-format=json"], capture=True)
        if output is None:
            return 1
        binaries = [json.loads(line) for line in output.splitlines() if line.strip()]
        binary = next(item["executable"] for item in binaries if item.get("executable") and item.get("target", {}).get("name") == "seedbox")
        environment["SEEDBOX_TEST_CLI"] = binary
        environment["SEEDBOX_TEST_COUNTER"] = "1" if "linux-counter" in suites else "0"
        # Under sudo, keep the cache the readiness check inspected.
        environment.setdefault("XDG_CACHE_HOME", str(cache_home(environment, system == "Darwin")))
    packages = [("seedbox-arm64", ["cargo", "test-arm64"])]
    if system == "Linux" and machine == "x86_64":
        packages.append(("seedbox-vmm", ["cargo", "test-vmm"]))
    packages.append(("seedbox", ["cargo", "test", "--workspace", "--locked"]))
    for package, command in packages:
        features = features_for(package, machine, suites)
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
