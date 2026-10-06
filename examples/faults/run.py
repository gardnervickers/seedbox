#!/usr/bin/env python3
"""Run/replay synthetic recovery, restart-dependent loss, and effect controls."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
from generate import generate
from verify import verify, verify_cutoff, verify_early_request

MSR_EVIDENCE = []
RUN_EVIDENCE = []
COMMAND_EVIDENCE = []
OUTPUT = None
STATUS = "running"

def reserve_output(directory, init_paths):
    """Claim fresh evidence output without changing a previous attempt."""
    candidate = Path(directory).absolute()
    if candidate.is_symlink():
        raise FileExistsError("refusing symlinked evidence output")
    candidate.mkdir(parents=True,exist_ok=True)
    reserved = {"acceptance.json","acceptance.json.tmp","projects","commands","state"}
    allowed = {Path(path).absolute() for path in init_paths if path is not None}
    for entry in candidate.iterdir():
        if entry.name in reserved or entry.absolute() not in allowed or entry.is_symlink() or not entry.is_file():
            raise FileExistsError(f"refusing output with existing evidence: {entry}")
        with entry.open("rb") as source:
            if source.read(4) != b"\x7fELF":
                raise FileExistsError(f"output may only contain supplied ELF guest-init binaries: {entry}")
    # Exclusive creation also protects against two launchers racing after the
    # directory check. OUTPUT stays unset if an existing attempt owns this file.
    with (candidate / "acceptance.json").open("x") as report:
        json.dump({"status":"running","runs":[],"msr_readback":[],"commands":[]},report)
        report.write("\n")
    return candidate

def persist_evidence(error=None):
    if OUTPUT is None:
        return
    contents = {"status":STATUS,"runs":RUN_EVIDENCE,"msr_readback":MSR_EVIDENCE,"commands":COMMAND_EVIDENCE}
    if error is not None:
        contents["error"] = str(error)
    temporary = OUTPUT / "acceptance.json.tmp"
    temporary.write_text(json.dumps(contents,indent=2)+"\n")
    temporary.replace(OUTPUT / "acceptance.json")

EARLY_SCENARIO = """import json
import time
from main import BATCH, exchange
from seedbox import World


def client(address, command):
    if command != "STATUS":
        return json.loads(exchange(address, command, timeout=12))
    deadline = time.monotonic() + 10
    while True:
        try:
            return json.loads(exchange(address, command))
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.02)


def run(address, no_faults=False):
    with World.connect() as world:
        client(address, "STATUS")
        first = client(address, "LOAD 1")
        assert first == {"generation": 1, "acked": BATCH, "total": BATCH}, first
        world.stop("server")
        world.sleep(0.1)
        world.restart("server")
        for total in (64, 96, 128):
            result = client(address, "LOAD 2")
            assert result == {"generation": 2, "acked": BATCH, "total": total}, result
        result = client(address, "VERIFY")
        assert result == dict(status="pass", acked=128, pre_restart=32,
                              post_restart=96, generation=2, missing=0), result
        print("EARLY_REQUEST_RESULT " + json.dumps(result, sort_keys=True), flush=True)
        return 0
"""

CUTOFF_SCENARIO = """from seedbox import World


def run(address, no_faults=False):
    with World.connect() as world:
        world.stop("server")
        raise RuntimeError("stop unexpectedly completed before the simulation cutoff")
"""


def focused_projects(projects):
    """Fresh retained inputs for readiness and cutoff checks; no host fault calls."""
    projects = Path(projects)
    for name, program in (("early-request", EARLY_SCENARIO), ("readiness-cutoff", CUTOFF_SCENARIO)):
        destination = projects / name
        shutil.copytree(projects / "fixed", destination)
        compose = destination / "compose.yaml"
        text = compose.read_text().replace("name: synthetic-fault-fixed", "name: synthetic-fault-" + name)
        server = '    command: [server, --listen, "0.0.0.0:7000", --state, /data/kv.log]\n'
        assert text.count(server) == 1, "missing canonical server command"
        text = text.replace(server, server + '    environment: {SEEDBOX_TEST_DELAY_READY: "1"}\n')
        if name == "readiness-cutoff":
            text = text.replace("simulated_time: 30s", "simulated_time: 3s")
        compose.write_text(text)
        (destination / "workload" / "scenario.py").write_text(program)


RUN = re.compile(r"^(?:PASS|FAIL|ERROR|TIMEOUT|INCOMPLETE): (.+)$", re.M)

def checked_run(command):
    """Assert scoped host-state restoration even when a run fails."""
    cpu = int(command[command.index("--cpu") + 1])
    topology = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list").read_text().strip()
    siblings = []
    for item in topology.split(","):
        span = list(map(int,item.split("-")))
        siblings.extend(range(span[0],span[-1]+1))
    def snapshot():
        values = {}
        for sibling in siblings:
            fd = os.open(f"/dev/cpu/{sibling}/msr",os.O_RDONLY)
            try:
                value = os.pread(fd,8,0xc0011020)
                assert len(value) == 8, "short MSR read"
                values[str(sibling)] = int.from_bytes(value,"little")
            finally:
                os.close(fd)
        return values
    number = len(COMMAND_EVIDENCE) + 1
    logs = OUTPUT / "commands"
    logs.mkdir(exist_ok=True)
    stdout_path, stderr_path = logs / f"{number:02}.stdout", logs / f"{number:02}.stderr"
    command_record = {"command":command,"stdout":str(stdout_path),"stderr":str(stderr_path),"status":"running"}
    COMMAND_EVIDENCE.append(command_record)
    before = snapshot()
    msr_record = {"command":command,"before":before,"after":None,"restored":None}
    MSR_EVIDENCE.append(msr_record)
    persist_evidence()
    try:
        with stdout_path.open("w") as stdout, stderr_path.open("w") as stderr:
            child = subprocess.Popen(command,stdout=stdout,stderr=stderr,text=True)
            try:
                returncode = child.wait()
            except KeyboardInterrupt:
                # Let the existing CLI stop/reap its VMM and restore its lease.
                # Never SIGKILL the CLI before it has completed that cleanup.
                child.send_signal(signal.SIGTERM)
                child.wait()
                command_record["status"] = "interrupted"
                raise
        command_record.update(status="completed",returncode=returncode)
        return subprocess.CompletedProcess(command,returncode,stdout_path.read_text(),stderr_path.read_text())
    finally:
        after = snapshot()
        msr_record.update(after=after,restored=before == after)
        persist_evidence()
        assert before == after, "host MSR restoration mismatch"

def execute(command, expected_code):
    result = checked_run(command)
    print(result.stdout, end="", flush=True)
    if result.stderr:
        print(result.stderr, end="", flush=True)
    if result.returncode != expected_code:
        raise RuntimeError(f"exit {result.returncode}, expected {expected_code}: {command}")
    match = RUN.search(result.stdout)
    if not match:
        raise RuntimeError("missing retained run path")
    return Path(match[1])

def main():
    global OUTPUT, STATUS
    p = argparse.ArgumentParser(description=__doc__)
    for flag in ("seedbox", "kernel", "vmm", "guest-init", "state", "output"):
        p.add_argument("--" + flag, required=True)
    p.add_argument("--cpu", type=int, required=True)
    p.add_argument("--busybox")
    p.add_argument("--negative-guest-init", help="same supervisor compiled with -DSEEDBOX_DROP_EFFECTS for the missing-effect control")
    p.add_argument("--rejecting-guest-init", help="same supervisor compiled with -DSEEDBOX_REJECT_EFFECTS for the explicit rejection control")
    p.add_argument("--delayed-guest-init", help="supervisor compiled with -DSEEDBOX_TEST_READY_DELAY_NS=4000000000ULL for readiness and cutoff checks")
    p.add_argument("--seed", type=int, default=1009)
    p.add_argument("--margin", default="98304")
    a = p.parse_args()
    OUTPUT = reserve_output(a.output,[a.guest_init,a.negative_guest_init,a.rejecting_guest_init,a.delayed_guest_init])
    def interrupt(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM,interrupt)
    persist_evidence()
    projects = generate(Path(a.output) / "projects", a.seed)
    if a.delayed_guest_init:
        focused_projects(projects)
    common = ["--kernel", a.kernel, "--vmm", a.vmm, "--guest-init", a.guest_init,
              "--cpu", str(a.cpu), "--state", a.state, "--memory-mb", "512", "--margin", a.margin]
    if a.busybox:
        common += ["--busybox", a.busybox]
    evidence = RUN_EVIDENCE
    for variant, status, faulted in (("fixed", "PASS", True), ("broken", "FAIL", True), ("broken-no-faults", "PASS", False)):
        run = execute([a.seedbox, "run", str(projects / variant / "compose.yaml"), *common], 0 if status == "PASS" else 1)
        evidence.append(verify(run, status, faulted))
        persist_evidence()
        replay = execute([a.seedbox, "replay", str(run), "--cpu", str(a.cpu), "--state", a.state, "--undo", "0", "--margin", a.margin], 0)
        evidence.append(verify(replay, status, faulted))
        persist_evidence()
        if variant == "fixed":
            comparison_margin = "131072" if a.margin != "131072" else "98304"
            varied = execute([a.seedbox, "replay", str(run), "--cpu", str(a.cpu), "--state", a.state, "--undo", "16", "--margin", comparison_margin], 0)
            evidence.append(dict(verify(varied, status, faulted), performance_variant="changed margin and undo"))
            persist_evidence()
    for control, init, expected_error in [("missing_effect",a.negative_guest_init,"deadline"),("rejected_effect",a.rejecting_guest_init,"guest rejected fault")]:
        if not init:
            continue
        negative = common.copy()
        negative[negative.index("--guest-init") + 1] = init
        rejected = execute([a.seedbox,"run",str(projects / "fixed" / "compose.yaml"),*negative], 2)
        record = json.loads((rejected / "manifest.json").read_text())["record"]
        assert record["outcome"] == "ERROR"
        world = json.loads((rejected / "output" / "vmm.json").read_text())
        assert expected_error in world["fault_error"] and any(f["status"] == "failed" for f in world["faults"])
        evidence.append({"run":str(rejected),control:"detected"})
        persist_evidence()
    if a.delayed_guest_init:
        delayed = common.copy()
        delayed[delayed.index("--guest-init") + 1] = a.delayed_guest_init
        for name, code, verifier in (("early-request", 0, verify_early_request),
                                     ("readiness-cutoff", 124, verify_cutoff)):
            run = execute([a.seedbox, "run", str(projects / name / "compose.yaml"), *delayed], code)
            evidence.append(verifier(run))
            persist_evidence()
            replay = execute([a.seedbox, "replay", str(run), "--cpu", str(a.cpu),
                              "--state", a.state, "--undo", "0", "--margin", a.margin], 0)
            evidence.append(verifier(replay))
            persist_evidence()
    STATUS = "passed"
    persist_evidence()

if __name__ == "__main__":
    try:
        main()
    except BaseException as error:
        STATUS = "interrupted" if isinstance(error,KeyboardInterrupt) else "failed"
        persist_evidence(error)
        raise
