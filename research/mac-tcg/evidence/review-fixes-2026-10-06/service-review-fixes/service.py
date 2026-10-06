#!/usr/bin/env python3
"""Run the merged #10 fixture on a bounded local, single-vCPU TCG profile."""
import argparse
import fcntl
import json
import lzma
from pathlib import Path
import re
import shutil
import shlex
import subprocess

from run import HERE, KERNEL_SHA256, digest, seed_bytes

from service_process import execute, save

ROOT = HERE.parent.parent
BASE_SHA256 = "4a926d378fb651676a1c4f208fa0be0c13a098f5d94bffcbde1d6f55b1f61220"
MODULES = ["veth", "act_gact", "sch_ingress", "sch_netem", "cls_u32", "cls_matchall"]


def archive(entries):
    data = bytearray()
    for ino, (name, mode, body) in enumerate(entries + [("TRAILER!!!", 0, b"")], 1):
        name = name.encode() + b"\0"
        fields = [ino, mode, 0, 0, 1, 0, len(body), 0, 0, 0, 0, len(name), 0]
        data.extend(b"070701" + "".join(f"{n:08x}" for n in fields).encode() + name)
        data.extend(b"\0" * (-len(data) % 4))
        data.extend(body)
        data.extend(b"\0" * (-len(data) % 4))
    data.extend(b"\0" * (-len(data) % 512))
    return bytes(data)


def contents(data):
    entries, offset = {}, 0
    while True:
        if data[offset:offset + 6] != b"070701":
            raise ValueError("unsupported base archive")
        fields = [int(data[offset + 6 + n * 8:offset + 14 + n * 8], 16) for n in range(13)]
        offset += 110
        name = data[offset:offset + fields[11] - 1].decode().removeprefix("./")
        offset = (offset + fields[11] + 3) & ~3
        body = data[offset:offset + fields[6]]
        offset = (offset + fields[6] + 3) & ~3
        if name == "TRAILER!!!":
            return entries
        entries[name] = body


def fixture(mode):
    source = (ROOT / "sim/guest/single.init").read_text()
    replacements = [
        ('mount -t proc proc /proc\n', ''),
        ('mount -t devtmpfs devtmpfs /dev\n', ''),
        ('setup_network\n', ''.join(f'insmod /modules/{m}.ko\n' for m in MODULES) + 'setup_network\n'),
        ('(\n trap', 'mkdir -p /run\nmkfifo /run/seedbox-control\n(\n trap'),
        ('stty -F /dev/ttyS1 raw -echo 2>/dev/null', ':'),
        ('< /dev/ttyS1', '< /run/seedbox-control'),
    ]
    if mode == "fault":
        replacements += [
            ('echo "SEEDBOX_PHASE client_start guest_ns=$(sbx time)"',
             'client_epoch=$(sbx time)\necho "SEEDBOX_PHASE client_start guest_ns=$client_epoch"\n/bin/busybox sh /scenario.sh "$client_epoch" &'),
            ('[ ! -f /tmp/control.failed ] || exit 3\nexit "$status"',
             '[ ! -f /tmp/control.failed ] || exit 3\n[ -f /tmp/scenario.done ] || { echo "SCENARIO incomplete"; exit 3; }\nexit "$status"'),
        ]
    for old, new in replacements:
        if source.count(old) != 1:
            raise ValueError(f"merged fixture drift at {old!r}")
        source = source.replace(old, new)
    return source.encode()


def scenario(seed):
    # The KVM world's times have a different epoch. Preserve command order and
    # relative spacing, but explicitly anchor this profile to client startup.
    jitter = int.from_bytes(seed_bytes(seed, "service-fault-offset")[:4], "little") % 100000001
    events = [(700000000 + jitter, "partition n1 n2"), (1400000000 + jitter, "heal n1 n2"),
              (1800000000 + jitter, "stop n2"), (2400000000 + jitter, "start n2")]
    lines = ['set -eu', 'exec 3>/run/seedbox-control', 'epoch="$1"']
    for relative, command in events:
        lines += [f'target=$((epoch + {relative}))', '/bin/tcg-bootstrap "$target"']
        if command.startswith("heal "):
            lines += ['echo "DROP_COUNTER_BEGIN" > /dev/ttyS1',
                      'tc -s filter show dev host1 parent ffff: > /dev/ttyS1',
                      'echo "DROP_COUNTER_END" > /dev/ttyS1']
        lines += [
                  f'echo "REQUEST {command} target_ns=$target guest_ns=$(sbx time)" > /dev/ttyS1',
                  f'printf "%s\\n" "{command}" >&3']
    # Keep the reliable command stream open while the workload completes.
    lines += ['touch /tmp/scenario.done', 'while :; do sleep 1; done']
    return ("\n".join(lines) + "\n").encode(), events


def classify(console, management, returncode, timeout, mode):
    if timeout:
        return "TIMEOUT", "host wall limit"
    terminal = re.findall(r"^INIT exit=(\d+)\r?$", console, re.M)
    if not terminal:
        return "INCOMPLETE", "missing guest completion"
    if len(terminal) != 1 or returncode != ((int(terminal[0]) << 1) | 1):
        return "ERROR", "guest/QEMU completion mismatch"
    code = int(terminal[0])
    if code == 4 and re.search(r"^RESULT incomplete ", console, re.M):
        return "INCOMPLETE", "fixed work did not finish before the guest deadline"
    if code not in (0, 1):
        return "ERROR", f"fixture exit {code}"
    if mode == "controls":
        if code == 0 and re.search(r"^NETWORK controls-pass\r?$", console, re.M) and "pass=0" not in console:
            return "PASS", "packet controls"
        return "ERROR", "packet control failed"
    results = [r.strip() for r in re.findall(r"^RESULT (.*)\r?$", console, re.M)]
    if len(results) != 1:
        return "INCOMPLETE", "missing or ambiguous checker result"
    if results[0].startswith("incomplete"):
        return "INCOMPLETE", results[0]
    if results[0].startswith("fail") and code == 1:
        return "FAIL", results[0]
    if not results[0].startswith("pass") or code != 0:
        return "ERROR", results[0]
    if mode == "fault":
        commands = ["partition n1 n2", "heal n1 n2", "stop n2 stopped", "start n2 launched"]
        applied = re.findall(r"^FAULT (.*?) guest_ns=(\d+)\r?$", management, re.M)
        requested = re.findall(r"^REQUEST (.*?) target_ns=(\d+) guest_ns=(\d+)\r?$", management, re.M)
        if [a[0] for a in applied] != commands or [r[0] for r in requested] != ["partition n1 n2", "heal n1 n2", "stop n2", "start n2"]:
            return "INCOMPLETE", "missing fault acknowledgments"
        if any(int(a[1]) < int(r[2]) or int(r[2]) < int(r[1]) for a, r in zip(applied, requested)):
            return "ERROR", "fault timestamps violate target/request/application order"
        # Requests can queue while an earlier action is running. Preserve causal
        # order without requiring an action to finish before the next deadline.
        records = re.findall(r"^(REQUEST|FAULT) (.*?) guest_ns=(\d+)\r?$", management, re.M)
        pending, last_time = [], -1
        for kind, command, timestamp in records:
            timestamp = int(timestamp)
            if timestamp < last_time:
                return "ERROR", "nonmonotonic fault log"
            last_time = timestamp
            if kind == "REQUEST":
                pending.append(command.split(" target_ns=")[0])
            elif not pending or not (command == pending[0] or command == pending[0] + " stopped" or command == pending[0] + " launched"):
                return "ERROR", "fault acknowledgment precedes its request"
            else:
                pending.pop(0)
        if pending or any(int(b[1]) <= int(a[1]) for a, b in zip(requested, requested[1:])):
            return "ERROR", "unordered fault targets or unacknowledged request"
        counters = re.search(r"DROP_COUNTER_BEGIN(.*?)DROP_COUNTER_END", management, re.S)
        if not counters or not any(int(n) > 0 for n in re.findall(r"dropped (\d+)", counters[1])):
            return "INCOMPLETE", "no observed live packet drops"
    return "PASS", results[0]


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--mode", choices=["controls", "healthy", "fault", "negative"], required=True)
    ap.add_argument("--seeds", nargs="+", type=int, default=[1009])
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--timeout", type=int, default=120)
    ap.add_argument("--shift", type=int, choices=range(0, 5), default=3)
    ap.add_argument("--etcd-version", choices=["3.5.2", "3.5.3"], default="3.5.2")
    ap.add_argument("--witness", action="store_true", help="Mac QEMU guest-boundary execution witness")
    args = ap.parse_args()
    if not 0 < args.timeout <= 120 or len(args.seeds) > 10 or any(not 0 <= s < 2**64 for s in args.seeds):
        ap.error("use <=10 unsigned seeds and <=120s per run")
    # Keep the persistent budget coherent if another batch is invoked by mistake.
    # Hold this descriptor until process exit, including compilation and QEMU.
    budget_lock = (HERE / "build/service-budget.lock").open("a")
    try:
        fcntl.flock(budget_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        ap.error("another service batch owns the execution budget")
    base = (HERE / "build/single-merged.cpio").read_bytes()
    kernel = HERE / "build/bzImage"
    if digest(base) != BASE_SHA256 or digest(kernel.read_bytes()) != KERNEL_SHA256:
        ap.error("base image/kernel identity mismatch")
    original = contents(base)
    for path, name in [("init", "single.init"), ("bin/network.sh", "network.sh"), ("bin/members.sh", "members.sh")]:
        if original[path] != (ROOT / "sim/guest" / name).read_bytes():
            ap.error(f"merged source does not match image: {name}")
    fixed = None
    if args.etcd_version == "3.5.3":
        fixed = (HERE / "build/etcd-3.5.3").read_bytes()
        if digest(fixed) != "79e672f61c4ae4197cee976fc4e6ab94cf42b40124a42e1c3774dc34c8d01c5e":
            ap.error("etcd 3.5.3 identity mismatch")
    helper = HERE / "build/service-bootstrap"
    subprocess.run(["zig", "cc", "-target", "x86_64-linux-musl", "-O2", "-static", "-Wall", "-Wextra", "-Werror",
                    "-o", str(helper), str(HERE / "service-bootstrap.c")], check=True)
    qemu = Path(shutil.which("qemu-system-x86_64")).resolve()
    plugin = HERE / "build/witness.dylib"
    if args.witness:
        includes = shlex.split(subprocess.check_output(["pkg-config", "--cflags", "glib-2.0"], text=True))
        subprocess.run(["clang", "-O2", "-dynamiclib", "-undefined", "dynamic_lookup", "-Wno-deprecated-declarations",
                        "-Wall", "-Wextra", "-Werror", "-I", str(qemu.parent.parent / "include"),
                        *includes, "-o", str(plugin), str(HERE / "witness.c")], check=True)
    args.output.mkdir(parents=True, exist_ok=False)
    out = args.output.resolve()
    for name in ["service.py", "service_process.py", "service-bootstrap.c", "witness.c", "witness-decode.h", "run.py", "flake.nix", "flake.lock"]:
        shutil.copyfile(HERE / name, out / name)
    budget_path = HERE / "build/service-budget.json"
    budget = json.loads(budget_path.read_text()) if budget_path.exists() else {"limit_seconds": 600, "spent_seconds": 0, "runs": []}
    if budget.get("pending"):
        ap.error("unreconciled pending attempt; do not reset the budget")
    rows = []
    for index, seed in enumerate(args.seeds):
        remaining = budget["limit_seconds"] - budget["spent_seconds"]
        if remaining <= 0:
            raise SystemExit("600-second aggregate service probe budget exhausted")
        script, events = scenario(seed)
        entries = [("init", 0o100755, helper.read_bytes()), ("bin/tcg-bootstrap", 0o100755, helper.read_bytes()),
                   ("fixture.init", 0o100755, fixture(args.mode)), ("seed", 0o100400, seed_bytes(seed, "service-linux-pool")),
                   ("scenario.sh", 0o100755, script), ("modules", 0o040755, b"")]
        if fixed is not None:
            entries.append(("bin/etcd", 0o100755, fixed))
        for module in MODULES:
            entries.append((f"modules/{module}.ko", 0o100400, lzma.decompress((HERE / "build/modules" / f"{module}.ko.xz").read_bytes())))
        prefix = f"s{seed}-r{index}"
        initrd = out / f"{prefix}.cpio"
        initrd.write_bytes(base + archive(entries))
        cmdline = "console=ttyS0 nokaslr random.trust_cpu=off random.trust_bootloader=off panic=-1 rdinit=/init loglevel=4 "
        cmdline += ("seedbox.mode=controls" if args.mode == "controls" else
                    "seedbox.delay=1.5 seedbox.load=" + ("3s" if args.mode == "fault" else "10s")
                    + " seedbox.concurrency=2 seedbox.writes=" + ("0" if args.mode == "fault" else "64") + " seedbox.net_delay=1ms")
        if args.mode == "negative":
            cmdline += " seedbox.plant=missing"
        command = [str(qemu), "-L", str(qemu.parent.parent / "share/qemu"), "-machine", "pc-i440fx-11.1,hpet=off",
                   "-accel", "tcg,thread=single", "-cpu", "qemu64,-rdrand,-rdseed", "-smp", "1", "-m", "1024M",
                   "-nodefaults", "-display", "none", "-monitor", "none", "-serial", "stdio", "-serial", f"file:{out / (prefix + '.mgmt')}",
                   "-nic", "none", "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04", "-no-reboot",
                   "-rtc", "base=2020-01-01T00:00:00,clock=vm,driftfix=none", "-seed", str(int.from_bytes(seed_bytes(seed, "service-qemu")[:4], "little")),
                   "-icount", f"shift={args.shift},align=off,sleep=off", "-kernel", str(kernel), "-initrd", str(initrd), "-append", cmdline]
        if args.witness:
            command += ["-plugin", f"{plugin},output={out / (prefix + '.witness.json')}"]
        manifest = {"profile": "seedbox.mac-tcg.services.v1", "seed": seed, "mode": args.mode, "shift": args.shift, "etcd_version": args.etcd_version,
                    "source": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
                    "base_sha256": BASE_SHA256, "kernel_sha256": KERNEL_SHA256, "initrd_sha256": digest(initrd.read_bytes()),
                    "qemu_sha256": digest(qemu.read_bytes()), "argv": command, "scenario_offsets": events,
                    "overlay_sha256": {name: digest(data) for name, _, data in entries},
                    "witness_plugin_sha256": digest(plugin.read_bytes()) if args.witness else None,
                    "adapter_sha256": {name: digest((HERE / name).read_bytes()) for name in ["service.py", "service_process.py", "service-bootstrap.c", "witness.c", "witness-decode.h"]}}
        save(out / f"{prefix}.manifest.json", manifest)
        for name, _, data in entries:
            if name in ["fixture.init", "scenario.sh"]:
                (out / f"{prefix}.{name}").write_bytes(data)
        attempt = execute(command, out / prefix, budget_path, budget, seed, args.timeout)
        timeout = attempt["process_status"] == "TIMEOUT"
        elapsed = attempt["wall_seconds"]
        returncode = attempt["returncode"]
        console = (out / f"{prefix}.console").read_bytes()
        management = (out / f"{prefix}.mgmt").read_bytes() if (out / f"{prefix}.mgmt").exists() else b""
        status, reason = classify(console.decode(errors="replace"), management.decode(errors="replace"), returncode, timeout, args.mode)
        witness = None
        witness_diagnostics = None
        if args.witness and attempt["process_status"] == "EXITED":
            try:
                witness = json.loads((out / f"{prefix}.witness.json").read_text())
                if witness["schema"] != "seedbox.tcg.boundaries.v2" or not witness["terminal"] or witness["events"] <= 0 or len(bytes.fromhex(witness["sha256"])) != 32:
                    raise ValueError("invalid witness")
                if returncode != ((witness["terminal_code"] << 1) | 1):
                    raise ValueError("witness terminal disagrees with QEMU")
                witness_diagnostics = {"post_terminal_events": witness.pop("post_terminal_events")}
            except (OSError, ValueError, KeyError, TypeError):
                status, reason = "ERROR", "missing or malformed requested witness"
        if attempt["process_status"] == "INTERRUPTED":
            status, reason = "INTERRUPTED", f"host signal {attempt['signal']}"
        row = {"seed": seed, "status": status, "reason": reason, "returncode": returncode, "wall_seconds": elapsed,
               "console_sha256": digest(console), "management_sha256": digest(management), "witness": witness,
               "witness_diagnostics": witness_diagnostics}
        rows.append(row)
        save(out / "runs.json", rows)
        budget["runs"][-1].update(row)
        save(budget_path, budget)
        print(json.dumps(row), flush=True)
        expected = status == "PASS" if args.mode != "negative" else status == "FAIL" and "k/99/000000" in reason
        if not expected:
            return 1
    groups = {}
    for seed in dict.fromkeys(args.seeds):
        group = [r for r in rows if r["seed"] == seed]
        groups[str(seed)] = {"runs": len(group), "console_variants": len({r["console_sha256"] for r in group}),
                             "management_variants": len({r["management_sha256"] for r in group}),
                             "witness_variants": len({json.dumps(r["witness"], sort_keys=True) for r in group}) if args.witness else None}
    equal = all(g["console_variants"] == g["management_variants"] == 1 and
                (g["witness_variants"] is None or g["witness_variants"] == 1) for g in groups.values())
    save(out / "summary.json", {"groups": groups, "same_seed_equal": equal,
                                "comparison_scope": "console, management log and optional selected guest-boundary SHA256; no full-state digest"})
    return 0 if equal else 1


if __name__ == "__main__":
    raise SystemExit(main())
