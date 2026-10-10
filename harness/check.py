#!/usr/bin/env python3
"""Run the named platform test suites; nothing is selected by probing the host.

Each suite names a platform and the Cargo test features it enables. A selected
suite whose host requirements are missing is an error before anything builds.
Tests belong to Cargo or unittest discovery, not a list of individual test names.
Application assertions remain in guests; applications.rs uses server-owned run/rerun/compare commands.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]

SUITES = {
    "portable": "Any host: fmt, clippy, workspace and standalone unit tests, Python suites, YAML validation.",
    "linux-kvm": "Linux x86-64 or ARM64 with /dev/kvm: native backend KVM tests; no root needed.",
    "linux-guests": "Fixture kernel and initrd on Linux KVM: real guest determinism and checkpoints.",
    "linux-oci": "Linux x86-64 with a running Docker-compatible engine: real image import and Dockerfile builds.",
    "linux-apps": "Linux x86-64 KVM, root, a Docker-compatible engine and Nix guest builds: every acceptance workload run twice, compared and rerun.",
    "linux-pause": "Linux x86-64 KVM, root, a Docker-compatible engine and guest builds: server pause/resume rerun.",
    "mac-hvf": "Apple Silicon with macOS 15+, Hypervisor.framework and codesign: native CPU/state and cancellation primitives.",
    "mac-guests": "mac-hvf plus the ARM fixture kernel and initrd: real guest controls.",
    "mac-apps": "mac-hvf plus a Docker-compatible engine and Nix guest builds: every acceptance workload on ARM PV.",
}
# Groups hold the suites a set-up host can run with no hand-built fixtures;
# the guest and pause suites are always named explicitly.
GROUPS = {
    "linux": ["linux-kvm", "linux-oci", "linux-apps"],
    "mac": ["mac-hvf", "mac-apps"],
}
# A suite also runs the suites its Cargo features build on.
IMPLIES = {
    "linux-guests": ["linux-kvm"],
    "mac-guests": ["mac-hvf"],
    "mac-apps": ["mac-hvf"],
}


def probe_linux():
    """Open/close KVM, never execute guests."""
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
    # test/runtime inputs, the guest asset directory and the engine socket, not
    # the user's general environment.
    inputs = [f"{key}={value}" for key, value in environment.items()
              if key.startswith(("SEEDBOX_TEST_", "SEEDBOX_ARM64_"))
              or key in ("SEEDBOX_KERNEL", "SEEDBOX_RUNTIME_ASSETS", "SEEDBOX_BUSYBOX",
                         "SEEDBOX_MKE2FS", "SEEDBOX_STATIC_CC", "DOCKER_HOST", "SEEDBOX_ASSET_DIR")]
    return [*prefix, *inputs, *command]


def build_requirements(environment, system):
    """Admission for the explicit build, never existence of an old output."""
    if system == "Linux":
        return [] if shutil.which("nix") else ["nix is required to build current guest inputs"]
    candidates = [environment["SEEDBOX_ASSET_BUILDER"]] if environment.get("SEEDBOX_ASSET_BUILDER") else ["podman", "docker"]
    return [] if any(shutil.which(tool) for tool in candidates) else ["Podman or Docker CLI is required to build Mac guest inputs"]


def select_build(environment, directory):
    """Use a completed bundle, independent of ambient runtime overrides."""
    directory = Path(directory)
    if not directory.is_absolute() or not all((directory / name).is_file() for name in ("seedbox", "kernel", "runtime.tar.gz")):
        raise ValueError("complete build did not return its executable and guest inputs")
    for key in ("SEEDBOX_ASSET_DIR", "SEEDBOX_BUILD_HELPERS", "SEEDBOX_GUEST_INIT", "SEEDBOX_MKE2FS", "SEEDBOX_VMM"):
        environment.pop(key, None)
    environment["SEEDBOX_TEST_CLI"] = str(directory / "seedbox")
    environment["SEEDBOX_KERNEL"] = str(directory / "kernel")
    environment["SEEDBOX_RUNTIME_ASSETS"] = str(directory / "runtime.tar.gz")


def engine_sockets(environment):
    """Sockets the CLI may connect to without DOCKER_HOST (cli/backends.rs)."""
    home = Path(environment.get("HOME", "~")).expanduser()
    runtime = environment.get("XDG_RUNTIME_DIR")
    sockets = [Path("/var/run/docker.sock"), home / ".docker/run/docker.sock",
               Path("/run/podman/podman.sock")]
    if runtime:
        sockets.append(Path(runtime) / "podman/podman.sock")
    if platform.system() == "Darwin":
        temporary = subprocess.run(["getconf", "DARWIN_USER_TEMP_DIR"], capture_output=True,
                                   text=True, check=False).stdout.strip()
        if temporary:
            sockets += sorted(Path(temporary, "podman").glob("*-api.sock"))
    return sockets


def container_engine(environment):
    return (bool(environment.get("DOCKER_HOST"))
            or any(shutil.which(tool) for tool in ("docker", "podman"))
            or any(path.is_socket() for path in engine_sockets(environment)))


def engine_needs_root(environment):
    """Whether only root can open the engine socket, e.g. a root:podman 0660 system socket."""
    host = environment.get("DOCKER_HOST", "")
    if host:
        sockets = [Path(host.removeprefix("unix://"))] if host.startswith("unix://") else []
    else:
        sockets = engine_sockets(environment)
    sockets = [path for path in sockets if path.is_socket()]
    return bool(sockets) and not any(os.access(path, os.R_OK | os.W_OK) for path in sockets)


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

    def engine():
        need(container_engine(environment),
             "no Docker-compatible engine (set DOCKER_HOST, or install Docker or Podman)")

    if suite == "portable":
        return missing
    if suite.startswith("linux-"):
        if not linux:
            return ["requires Linux"]
        if suite != "linux-kvm" and suite != "linux-guests":
            need(x86, "requires Linux x86-64")
        if suite != "linux-oci":  # Image import and builds need no hypervisor.
            need(not probe.get("kvm"), "/dev/kvm: " + (probe.get("kvm") or ""))
        if suite in ("linux-apps", "linux-pause"):
            need(can_root, "requires root or passwordless sudo")
        if suite == "linux-kvm":
            need(machine in ("x86_64", "aarch64"), "requires Linux x86-64 or ARM64")
        elif suite == "linux-guests":
            inputs(*(("SEEDBOX_TEST_KERNEL", "SEEDBOX_TEST_INITRD") if x86 else
                     ("SEEDBOX_ARM64_KERNEL", "SEEDBOX_ARM64_INITRD")))
        elif suite == "linux-oci":
            engine()
            need(can_root or not engine_needs_root(environment),
                 "the engine socket is not writable by this user; join its group, or allow passwordless sudo")
        elif suite in ("linux-apps", "linux-pause"):
            engine()
            missing.extend(build_requirements(environment, system))
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
        engine()
        missing.extend(build_requirements(environment, system))
    return missing


def features_for(package, machine, suites):
    """Cargo test features for one package; selection depends only on the suites."""
    def pick(pairs):
        return [feature for suite, feature in pairs if suite in suites]
    if package == "seedbox-application":
        return pick((("linux-oci", "oci-tests"),))
    if package == "seedbox":
        return pick((("linux-apps", "application-tests"),
                     ("mac-apps", "application-tests"), ("linux-pause", "retained-run-tests")))
    if package == "seedbox-vmm":
        return pick((("linux-kvm", "kvm-tests"),
                     ("linux-guests", "guest-tests"))) if machine == "x86_64" else []
    linux_arm = machine in ("arm64", "aarch64")
    features = pick((("mac-hvf", "hvf-tests"),))
    if linux_arm:
        features = pick((("linux-kvm", "kvm-tests"),)) + features
        if "linux-guests" in suites or "mac-guests" in suites:
            features.append("guest-tests")
    return features


def feature_test_targets(metadata, package, features):
    """Integration targets whose declared required features are selected."""
    selected = set(features)
    targets = next(p["targets"] for p in metadata["packages"] if p["name"] == package)
    return sorted(t["name"] for t in targets if "test" in t["kind"]
                  and t.get("required-features")
                  and set(t["required-features"]) <= selected)


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

    def cargo_tests(self, package, command, features, prefix, *, sign=False):
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
                # oci uses the engine the way the root CLI does in applications.
                needs_root = package == "seedbox" and target in ("applications", "host_pause", "oci")
                if needs_root or (package in ("seedbox-arm64", "seedbox-vmm") and "kvm-tests" in features):
                    command = privileged(prefix, command, environment)
                self.run(package + "/" + target, command, environment=environment,
                         timeout=None if target == "applications" else 1800)


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
    checks.run("fmt", ["cargo", "fmt", "--all", "--check"])
    checks.run("clippy", ["cargo", "clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"])
    checks.run("portable", ["bash", "harness/acceptance.sh", "portable"])
    # acceptance.sh already ran the root and native backend without hardware.
    if not hardware or checks.failed:
        return int(checks.failed)
    reports_root = ROOT / "target/check-reports"
    reports_root.mkdir(parents=True, exist_ok=True)
    reports = Path(tempfile.mkdtemp(prefix="run-", dir=reports_root))
    print(f"reports: {reports}")
    environment["SEEDBOX_TEST_REPORT_DIR"] = str(reports)
    for key, default in (("SEEDBOX_TEST_KERNEL", "artifacts/bzImage"),
                         ("SEEDBOX_TEST_INITRD", "artifacts/entropy-fork.cpio"),
                         ("SEEDBOX_ARM64_KERNEL", "artifacts/arm64-hres/Image"),
                         ("SEEDBOX_ARM64_INITRD", "artifacts/arm64-hres/init.cpio")):
        if (path := available_file(environment, key, ROOT / default)) is not None:
            environment[key] = str(path)
    environment["SEEDBOX_ARM64_REPORT_DIR"] = str(reports / "arm64")
    if {"linux-apps", "linux-pause", "mac-apps"} & set(suites):
        output = checks.run("application-assets", ["cargo", "xtask", "build-assets"], capture=True)
        if output is None:
            return 1
        assets = Path(output.strip())
        # Freeze both the asset paths and executable for this acceptance run.
        build_env = dict(os.environ, SEEDBOX_BUILD_KERNEL=str(assets / "kernel"),
                         SEEDBOX_BUILD_HELPERS=str(assets / "runtime.tar.gz"))
        compiled = checks.run("application-build", ["cargo", "build", "--release", "--locked", "-p", "seedbox"], environment=build_env)
        if not compiled:
            return 1
        complete = reports / "application-build"
        complete.mkdir()
        for name in ("kernel", "runtime.tar.gz"):
            shutil.copy2(assets / name, complete / name)
        shutil.copy2(ROOT / "target/release/seedbox", complete / "seedbox")
        select_build(environment, complete)
    packages = [("seedbox-arm64", ["cargo", "test-arm64"])]
    if system == "Linux" and machine == "x86_64":
        packages.append(("seedbox-vmm", ["cargo", "test-vmm"]))
    packages.extend((package, ["cargo", "test", "-p", package, "--locked"])
                    for package in ("seedbox", "seedbox-application"))
    for package, command in packages:
        features = features_for(package, machine, suites)
        if features:
            if package in ("seedbox", "seedbox-application"):
                metadata = checks.run("test-targets", ["cargo", "metadata", "--no-deps", "--format-version=1", "--locked"], capture=True)
                if metadata is None:
                    return 1
                targets = feature_test_targets(json.loads(metadata), package, features)
                if not targets:
                    raise ValueError(f"{package}: no integration targets require {features}")
                command = [*command, *(arg for target in targets for arg in ("--test", target))]
            checks.cargo_tests(package, command, features, prefix,
                               sign=package == "seedbox-arm64" and "hvf-tests" in features)
    return int(checks.failed)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"check: {error}")
