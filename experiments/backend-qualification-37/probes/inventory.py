#!/usr/bin/env python3
"""Research inventory only: no VM, CPU lease, MSR write or admission decision."""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import struct
import subprocess


def read(path):
    try:
        return {"value": Path(path).read_text().strip()}
    except OSError as error:
        return {"error": str(error)}


def command(argv):
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=10,
                                check=False)
        return {"argv": argv, "returncode": result.returncode,
                "stdout": result.stdout, "stderr": result.stderr}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"argv": argv, "error": str(error)}


def file_identity(path):
    try:
        path = Path(path)
        digest = hashlib.sha256()
        with path.open("rb") as source:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(block)
        return {"resolved_path": str(path.resolve()), "sha256": digest.hexdigest()}
    except OSError as error:
        return {"error": str(error)}


def kvm_observations():
    """Linux x86 KVM UAPI: GET_API_VERSION and GET_SUPPORTED_CPUID only.

    The supported table is not the actual guest-exposed table. No VM is created.
    ioctl numbers encode type 0xae, commands 0 and 5, and cpuid2 header size 8.
    """
    try:
        descriptor = os.open("/dev/kvm", os.O_RDONLY | os.O_CLOEXEC)
    except OSError as error:
        return {"error": str(error)}
    try:
        api = fcntl.ioctl(descriptor, 0xAE00)
        capacity = 1024
        buffer = bytearray(8 + capacity * 40)
        struct.pack_into("II", buffer, 0, capacity, 0)
        try:
            fcntl.ioctl(descriptor, 0xC008AE05, buffer, True)
        except OSError as error:
            return {"api_version": api, "supported_cpuid_error": str(error),
                    "returned_nent": struct.unpack_from("I", buffer)[0]}
        count = struct.unpack_from("I", buffer)[0]
        if count > capacity:
            return {"api_version": api, "supported_cpuid_error": "capacity exceeded"}
        keys = ("function", "index", "flags", "eax", "ebx", "ecx", "edx")
        entries = [dict(zip(keys, struct.unpack_from("10I", buffer, 8 + i * 40)[:7]))
                   for i in range(count)]
        entries.sort(key=lambda entry: (entry["function"], entry["index"], entry["flags"]))
        canonical = json.dumps(entries, sort_keys=True, separators=(",", ":")).encode()
        return {"api_version": api, "supported_cpuid": entries,
                "supported_cpuid_sha256": hashlib.sha256(canonical).hexdigest(),
                "actual_guest_cpuid": "NOT_OBSERVED", "vm_created": False}
    except OSError as error:
        return {"error": str(error)}
    finally:
        os.close(descriptor)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--checkout", type=Path, required=True,
                        help="Checkout whose source identities are recorded.")
    args = parser.parse_args()
    root = args.checkout.resolve()
    if not (root / "Cargo.toml").is_file():
        parser.error("--checkout must name a Seedbox checkout")
    report = {
        "schema": "seedbox.research.host-inventory.v1",
        "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "scope": "read-only inventory; never host qualification",
        "platform": {"system": platform.system(), "release": platform.release(),
                     "machine": platform.machine(), "node": platform.node()},
        "effective_uid": os.geteuid(),
        "source": {"checkout": str(root), "git": command(["git", "-C", str(root),
                    "rev-parse", "HEAD"])},
    }
    sources = ("flake.nix", "flake.lock", "Cargo.lock",
               "crates/seedbox-vmm/Cargo.toml", "crates/seedbox-vmm/Cargo.lock",
               "crates/seedbox-vmm/src/counter.rs", "crates/seedbox-vmm/src/machine.rs",
               "crates/seedbox/src/host.rs", "harness/msr54.py", "kernel/config.options",
               "guest/compose-init.c")
    report["source"]["files"] = {name: file_identity(root / name) for name in sources}
    if platform.system() == "Linux" and platform.machine() == "x86_64":
        report["kvm"] = kvm_observations()
        report["lscpu"] = command(["lscpu", "--json"])
        report["cpu_topology"] = command(["lscpu", "--json", "--extended=CPU,CORE,SOCKET,NODE,ONLINE"])
        report["kernel_inputs"] = {name: file_identity(name) for name in
                                   ("/run/booted-system/kernel", "/run/current-system/kernel",
                                    "/proc/config.gz")}
        report["booted_system"] = str(Path("/run/booted-system").resolve())
        report["current_system"] = str(Path("/run/current-system").resolve())
        report["cpuinfo"] = read("/proc/cpuinfo")
        report["memory"] = read("/proc/meminfo")
        report["kernel_command_line"] = read("/proc/cmdline")
        paths = ("/proc/sys/kernel/nmi_watchdog", "/proc/sys/kernel/perf_event_paranoid",
                 "/proc/sys/kernel/perf_event_max_sample_rate",
                 "/sys/devices/system/cpu/online", "/sys/devices/system/cpu/smt/active")
        report["settings"] = {name: read(name) for name in paths}
        report["smt_siblings"] = {str(path.parent.parent.name): read(path) for path in
                                 sorted(Path("/sys/devices/system/cpu").glob("cpu[0-9]*/topology/thread_siblings_list"))}
        report["kvm_module_parameters"] = {str(path): read(path) for path in
                                           sorted(Path("/sys/module").glob("kvm*/parameters/*"))}
        pmu_paths = []
        for pmu in sorted(Path("/sys/bus/event_source/devices").glob("cpu*")):
            pmu_paths.extend(path for path in (pmu / "type", pmu / "cpus", pmu / "cpumask")
                             if path.is_file())
            for name in ("format", "caps"):
                pmu_paths.extend(path for path in sorted((pmu / name).glob("*")) if path.is_file())
        report["pmu_sysfs"] = {str(path): read(path) for path in pmu_paths}
        locks = command(["lslocks", "--json", "--notruncate", "-o", "PID,COMMAND,TYPE,MODE,PATH"])
        if locks.get("returncode") == 0:
            try:
                rows = json.loads(locks.pop("stdout")).get("locks", [])
                locks["seedbox_locks"] = [row for row in rows if
                                          "seedbox-msr-cpu-" in (row.get("path") or "")]
            except (ValueError, TypeError) as error:
                locks["parse_error"] = str(error)
        report["lease_snapshot"] = locks
        report["active_processes"] = command(["ps", "-eo", "pid,ppid,psr,pcpu,pmem,etime,comm", "--sort=-pcpu"])
    else:
        report["kvm"] = {"status": "NOT_APPLICABLE", "reason": "not Linux x86_64"}
        report["os_build"] = command(["sw_vers"])
    print(json.dumps(report, sort_keys=True, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
