#!/usr/bin/env python3
"""Bounded fresh-process TCG probe; no network, disks, KVM or replay log."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import resource
import shutil
import signal
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
KERNEL_SHA256 = "66111c2888091bd64e96e71a6a9e831ab5496d6c6272add5d81490396bfa3cdc"
PROFILE = "seedbox.mac-tcg.input-probe.v1"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def seed_bytes(seed, domain):
    return hashlib.sha256(f"{PROFILE}/{domain}/{seed}".encode()).digest()


def initramfs(probe, seed, control):
    """Canonical newc archive, including fixed ownership, times and inode order."""
    entries = [("dev", 0o040755, b""), ("proc", 0o040755, b""),
               ("init", 0o100755, probe), ("probe", 0o100755, probe),
               ("seed", 0o100400, seed_bytes(seed, "linux-pool")),
               ("mode", 0o100400, {"none": b"s", "no-entropy": b"n", "plant": b"p"}[control]),
               ("TRAILER!!!", 0, b"")]
    data = bytearray()
    for ino, (name, mode, body) in enumerate(entries, 1):
        name = name.encode() + b"\0"
        fields = [ino, mode, 0, 0, 1, 0, len(body), 0, 0, 0, 0, len(name), 0]
        data.extend(b"070701" + "".join(f"{n:08x}" for n in fields).encode() + name)
        data.extend(b"\0" * (-len(data) % 4))
        data.extend(body)
        data.extend(b"\0" * (-len(data) % 4))
    data.extend(b"\0" * (-len(data) % 512))
    return bytes(data)


def classify(console, returncode, timed_out):
    """A clean QEMU exit alone is never a successful workload result."""
    events = []
    try:
        for line in console.decode("utf-8", errors="replace").splitlines():
            if line.startswith("SBX "):
                event = json.loads(line[4:])
                if not isinstance(event, dict):
                    raise ValueError("non-object observation")
                events.append(event)
    except (ValueError, TypeError):
        return "ERROR", events, "malformed observation"
    if timed_out:
        return "TIMEOUT", events, "host wall limit"
    if returncode != 0 or any(e.get("kind") == "error" for e in events):
        return "ERROR", events, "QEMU or guest error"
    results = [e for e in events if e.get("kind") == "result"]
    if not results:
        return "INCOMPLETE", events, "missing workload result"
    if len(results) != 1 or events[-1] != results[0]:
        return "ERROR", events, "ambiguous workload result"
    result = results[0]
    if result.get("status") == "FAIL":
        return "FAIL", events, result.get("reason")
    if result.get("status") != "PASS" or result.get("reason") != "completed":
        return "ERROR", events, "invalid result"
    kinds = [e.get("kind") for e in events]
    expected = ["entropy", "entropy", "cpu", "start"] + ["cpu_work", "sleep"] * 3 + ["poll", "result"]
    if kinds != expected:
        return "ERROR", events, "missing or out-of-order checks"
    try:
        entropy = events[1]
        if entropy["phase"] != "payload" or entropy["count"] != 32 or entropy["errno"] != 0:
            raise ValueError("entropy not ready")
        if len(bytes.fromhex(entropy["bytes"])) != 32 or len(bytes.fromhex(entropy["at_random"])) != 16:
            raise ValueError("entropy evidence missing")
        if events[2]["rdrand"] != 0 or events[2]["rdseed"] != 0:
            raise ValueError("CPU RNG exposed")
        for round_no in range(3):
            cpu, sleep = events[4 + 2 * round_no:6 + 2 * round_no]
            if cpu["round"] != round_no or sleep["round"] != round_no:
                raise ValueError("round mismatch")
            if cpu["after_ns"] <= cpu["before_ns"] or cpu["tsc_after"] <= cpu["tsc_before"]:
                raise ValueError("clock did not progress")
            if sleep["requested_ns"] != (round_no + 1) * 10000000 or sleep["after_ns"] - sleep["before_ns"] < sleep["requested_ns"]:
                raise ValueError("sleep deadline violated")
        poll = events[-2]
        if not 0 < poll["count"] <= 100000 or poll["after_ns"] - poll["before_ns"] < 1000000:
            raise ValueError("clock poll stalled")
    except (KeyError, ValueError, TypeError):
        return "ERROR", events, "invalid check evidence"
    return "PASS", events, "completed"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--probe", type=Path, default=HERE / "build/probe")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--seeds", nargs="+", type=int, default=[42, 42, 42, 42, 42, 43, 43, 44, 44])
    parser.add_argument("--control", choices=["none", "no-entropy", "plant"], default="none")
    parser.add_argument("--plain-tcg", action="store_true", help="nondeterministic clock negative control")
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--budget", type=float, default=600)
    args = parser.parse_args()
    if not (math.isfinite(args.timeout) and math.isfinite(args.budget) and 0 < args.timeout <= 120 and 0 < args.budget <= 600):
        parser.error("limits must be finite, positive and <=120s/run, <=600s/batch")
    if len(args.seeds) > 20 or any(not 0 <= seed < 2**64 for seed in args.seeds):
        parser.error("use <=20 seeds, each an unsigned 64-bit integer")
    kernel, probe = args.kernel.resolve(), args.probe.resolve()
    if digest(kernel.read_bytes()) != KERNEL_SHA256:
        parser.error("kernel differs from the pinned stock-kernel profile")
    probe_data = probe.read_bytes()
    if probe_data[:6] != b"\x7fELF\x02\x01" or probe_data[18:20] != b"\x3e\x00":
        parser.error("probe must be a 64-bit little-endian x86 ELF")
    qemu_path = shutil.which("qemu-system-x86_64")
    if not qemu_path:
        parser.error("QEMU missing; use the project Nix shell")
    qemu = Path(qemu_path).resolve()
    firmware = qemu.parent.parent / "share/qemu"
    if not firmware.is_dir():
        parser.error("expected pinned QEMU firmware directory missing")
    args.output.mkdir(parents=True, exist_ok=False)
    out = args.output.resolve()
    # Keep replay inputs with the run; tags, builders and the original checkout
    # are not consulted during an offline rerun with these files.
    shutil.copyfile(kernel, out / "kernel")
    shutil.copyfile(probe, out / "probe")
    for name in ["run.py", "probe.c", "flake.nix", "flake.lock", "build-guest.sh"]:
        shutil.copyfile(HERE / name, out / name)
    build_record = probe.parent / "guest-build.json"
    if build_record.exists():
        shutil.copyfile(build_record, out / "guest-build.json")
    manifest = {
        "schema": "seedbox.mac-tcg.probe-run.v1", "profile": PROFILE,
        "host": {"system": platform.system(), "release": platform.release(), "machine": platform.machine()},
        "kernel_sha256": KERNEL_SHA256, "probe_sha256": digest(probe_data),
        "qemu": str(qemu), "qemu_sha256": digest(qemu.read_bytes()),
        "qemu_version": subprocess.check_output([str(qemu), "--version"], text=True),
        "firmware_sha256": {str(p.relative_to(firmware)): digest(p.read_bytes()) for p in sorted(firmware.rglob("*")) if p.is_file()},
        "sources_sha256": {name: digest((HERE / name).read_bytes()) for name in ["run.py", "probe.c", "flake.nix", "flake.lock", "build-guest.sh"]},
        "seeds": args.seeds, "control": args.control, "plain_tcg": args.plain_tcg,
        "timeout_seconds": args.timeout, "budget_seconds": args.budget,
        "comparison_scope": "complete console and ordered probe observations; no full-state or execution witness",
    }
    save(out / "manifest.json", manifest)
    runs = []
    spent = 0.0
    for index, seed in enumerate(args.seeds):
        if spent >= args.budget:
            break
        prefix = f"s{seed}-r{index}"
        initrd = out / f"{prefix}.cpio"
        initrd.write_bytes(initramfs(probe_data, seed, args.control))
        command = [str(qemu), "-L", str(firmware), "-machine", "pc-i440fx-11.1,hpet=off",
                   "-accel", "tcg,thread=single", "-cpu", "qemu64,-rdrand,-rdseed", "-smp", "1", "-m", "256M",
                   "-nodefaults", "-display", "none", "-monitor", "none", "-serial", "stdio", "-nic", "none",
                   "-no-reboot", "-rtc", "base=2020-01-01T00:00:00,clock=vm,driftfix=none",
                   "-seed", str(int.from_bytes(seed_bytes(seed, "qemu")[:4], "little")),
                   "-kernel", str(out / "kernel"), "-initrd", str(initrd), "-append",
                   "console=ttyS0 nokaslr random.trust_cpu=off random.trust_bootloader=off panic=-1 rdinit=/init loglevel=4"]
        if not args.plain_tcg:
            command += ["-icount", "shift=3,align=off,sleep=off"]
        started = time.monotonic()
        timed_out = False
        with (out / f"{prefix}.console").open("wb") as console, (out / f"{prefix}.stderr").open("wb") as errors:
            process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=console, stderr=errors, start_new_session=True)
            try:
                process.wait(timeout=min(args.timeout, args.budget - spent))
            except subprocess.TimeoutExpired:
                timed_out = True
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            except BaseException:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise
        elapsed = time.monotonic() - started
        spent += elapsed
        data = (out / f"{prefix}.console").read_bytes()
        status, events, reason = classify(data, process.returncode, timed_out)
        observed = (json.dumps(events, sort_keys=True, separators=(",", ":")) + "\n").encode()
        save(out / f"{prefix}.observations.json", events)
        expected = {"none": ("PASS", "completed"), "no-entropy": ("FAIL", "entropy_not_ready"), "plant": ("FAIL", "planted_failure")}[args.control]
        row = {"seed": seed, "index": index, "status": status, "reason": reason,
               "expected_control_outcome": (status, reason) == expected,
               "returncode": process.returncode, "wall_seconds": elapsed,
               "console_sha256": digest(data), "observations_sha256": digest(observed),
               "initramfs_sha256": digest(initrd.read_bytes()), "argv": command}
        runs.append(row)
        save(out / "runs.json", runs)
        print(json.dumps({k: row[k] for k in ["seed", "status", "reason", "wall_seconds", "observations_sha256"]}), flush=True)
        if not row["expected_control_outcome"]:
            break
    groups = {}
    for seed in dict.fromkeys(args.seeds):
        rows = [r for r in runs if r["seed"] == seed]
        groups[str(seed)] = {"runs": len(rows), "console_variants": len({r["console_sha256"] for r in rows}),
                             "observation_variants": len({r["observations_sha256"] for r in rows})}
    repeated = all(g["observation_variants"] == 1 and g["console_variants"] == 1 for g in groups.values())
    expected = len(runs) == len(args.seeds) and all(r["expected_control_outcome"] for r in runs)
    summary = {"runs": len(runs), "requested_runs": len(args.seeds), "aggregate_qemu_wall_seconds": spent,
               "groups": groups, "same_seed_observations_equal": repeated,
               "all_expected_outcomes": expected,
               "runner_children_peak_rss_bytes": resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss * (1 if sys.platform == "darwin" else 1024)}
    save(out / "summary.json", summary)
    return 0 if expected and (args.plain_tcg or repeated) else 1


if __name__ == "__main__":
    sys.exit(main())
