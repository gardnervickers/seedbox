"""Host acceptance adapter over the public seedbox run/replay/inspect commands."""
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import threading
import time

from .model import FAIL, FULL_STATE, PASS, UNRESOLVED, RunResult

RUN = re.compile(r"^(?:PASS|FAIL|ERROR|TIMEOUT|INCOMPLETE): (.+)$", re.M)


def sha256(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def cpu_list(value):
    cpus = []
    for item in value.split(","):
        low, _, high = item.partition("-")
        cpus += range(int(low), int(high or low) + 1)
    if not cpus or min(cpus) < 0:
        raise ValueError("select at least one nonnegative CPU")
    return sorted(set(cpus))


def sysfs_siblings(cpu):
    path = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list")
    return set(cpu_list(path.read_text().strip())) if path.exists() else {cpu}


class CliLauncher:
    def __init__(self, seedbox, scenarios, cpus, out, engines, kernel, busybox,
                 lease_wait=0, log=print, prefix=("sudo", "-n"), siblings=sysfs_siblings):
        self.seedbox, self.scenarios = str(Path(seedbox).resolve()), scenarios
        self.out, self.engines = Path(out), engines
        self.kernel, self.busybox = str(kernel), str(busybox)
        self.lease_wait, self.log, self.prefix = lease_wait, log, list(prefix)
        self.state = self.out / "state"
        self.commands = self.out / "commands"
        self.commands.mkdir(parents=True)
        self.cpus = queue.Queue()
        protected = set()
        for cpu in cpus:
            family = siblings(cpu)
            if not family & protected:
                self.cpus.put(cpu)
                protected.update(family)
        self.width = self.cpus.qsize()
        if not self.width:
            raise ValueError("no disjoint CPU cores selected")
        self.references = {}
        self.lock = threading.Lock()
        self.sequence = 0
        self.children = set()
        self.cancelled = False

    def _command(self, arguments):
        # Only paths are inherited; ambient execution settings must not affect a job.
        env = {k: v for k, v in os.environ.items() if not k.startswith("SEEDBOX_")}
        with self.lock:
            if self.cancelled:
                raise RuntimeError("acceptance cancelled")
            self.sequence += 1
            stem = self.commands / f"{self.sequence:03}"
            command = self.prefix + [self.seedbox, *arguments]
            stem.with_suffix(".command.json").write_text(json.dumps(command) + "\n")
            stdout = stem.with_suffix(".stdout").open("w")
            stderr = stem.with_suffix(".stderr").open("w")
            try:
                child = subprocess.Popen(command, stdout=stdout, stderr=stderr, env=env)
            except BaseException:
                stdout.close()
                stderr.close()
                raise
            self.children.add(child)
        try:
            code = child.wait()
        finally:
            stdout.close()
            stderr.close()
            with self.lock:
                self.children.remove(child)
        return code, stem.with_suffix(".stdout").read_text(), stem.with_suffix(".stderr").read_text()

    def run(self, jobs):
        groups = {}
        for index, job in enumerate(jobs):
            groups.setdefault((job.scenario, job.seed, job.witness), []).append((index, job))
        results = {}
        pool = ThreadPoolExecutor(max_workers=self.width)
        try:
            for group in pool.map(self._group, groups.items()):
                results.update(group)
        except BaseException:
            with self.lock:
                self.cancelled = True
                for child in self.children:
                    if child.poll() is None:
                        child.terminate()  # sudo forwards TERM; CLI reaps its VMM and restores its lease.
            raise
        finally:
            pool.shutdown(wait=True)
        return [results[i] for i in range(len(jobs))]

    def _group(self, entry):
        key, jobs = entry
        cpu = self.cpus.get()
        results = {}
        try:
            reference = self.references.get(key)
            for index, job in jobs:
                self.log(f"start {job.describe()} cpu {cpu} ({'replay' if reference else 'run'})")
                try:
                    result = self._attempt(job, cpu, reference)
                    if result.status == UNRESOLVED and "overshoot" in result.detail and "clock bound" not in result.detail:
                        first = {"evidence": result.evidence, "detail": result.detail}
                        attempts = result.attempts + [first]
                        result = self._attempt(job, cpu, reference, margin="98304")
                        result.retries, result.attempts = 1, attempts + result.attempts
                    if reference is None and result.completed:
                        reference = result.evidence
                        self.references[key] = reference
                except Exception as error:
                    result = RunResult(job, UNRESOLVED, detail=str(error))
                self.log(f"done  {job.describe()}: {result.status} {result.detail[:160]}")
                results[index] = result
            return results
        finally:
            self.cpus.put(cpu)

    def _attempt(self, job, cpu, reference, margin=None):
        settings = dict(job.performance)
        common = ["--cpu", str(cpu), "--state", str(self.state), "--undo", settings.get("undo", "256"),
                  "--margin", margin or settings.get("margin", "adaptive")]
        engine = str(self.engines[job.engine])
        if reference:
            command = ["replay", reference, *common, "--vmm", engine, "--allow-engine-change"]
        else:
            command = ["run", self.scenarios[job.scenario], "--seed", str(job.seed), *common,
                       "--vmm", engine, "--kernel", self.kernel, "--busybox", self.busybox]
            if job.witness == FULL_STATE:
                command += ["--full-state"]
        started = time.monotonic()
        deadline = None
        attempts = []
        while True:
            code, stdout, stderr = self._command(command)
            result = RunResult(job, UNRESOLVED, detail=stderr.strip()[-1000:] or stdout.strip()[-1000:])
            result.host_seconds = time.monotonic() - started
            matches = RUN.findall(stdout)
            if len(matches) == 1:
                path = Path(matches[0])
                result.evidence = str(path)
                inspected, record, error = self._command(["inspect", str(path), "--json"])
                if inspected:
                    result.detail = "inspect failed: " + error
                else:
                    result = parse_record(result, json.loads(record), path, code,
                                          replay=bool(reference), read_bytes=self._read_output)
            busy = "CPU lease unavailable" in result.detail and "another run holds it" in result.detail
            if result.status != UNRESOLVED or not busy:
                break
            if deadline is None:
                deadline = time.monotonic() + self.lease_wait
            if time.monotonic() >= deadline:
                break
            attempts.append({"evidence": result.evidence, "detail": result.detail})
            time.sleep(min(0.2, max(0, deadline - time.monotonic())))
        result.attempts = attempts
        return result

    def _read_output(self, path):
        try:
            return path.read_bytes()
        except PermissionError:
            # CLI run directories are private to the privileged executor.
            return subprocess.run(self.prefix + ["cat", "--", str(path)], capture_output=True, check=True).stdout


def parse_record(result, record, path, code, replay=False, read_bytes=Path.read_bytes):
    """Read verified public records; never infer a fault from an intended schedule."""
    def retained(name):
        file = path / "output" / name
        data = read_bytes(file)
        if record["logs"].get(name) != hashlib.sha256(data).hexdigest():
            raise ValueError(f"retained output hash mismatch: {name}")
        return data.decode(errors="replace")

    report = record.get("report") or {}
    result.detail = record.get("error") or report.get("detail") or result.detail
    if record.get("error") == "execution: VMM failed; see vmm.stderr" and "vmm.stderr" in record.get("logs", {}):
        result.detail += ": " + retained("vmm.stderr")[-1000:]
    if (record.get("identity", {}).get("seed") != result.job.seed or report.get("reason") != "COMPLETED"
            or record.get("outcome") not in ("PASS", "FAIL")):
        return result
    expected = (0 if record["outcome"] == "PASS" else 1)
    comparison = record.get("replay") or {}
    expected_cli = (0 if comparison.get("status") == "MATCH" else 1) if replay else expected
    if code != expected_cli:
        result.detail = "CLI exit contradicts completed record"
        return result
    if replay and comparison.get("status") not in ("MATCH", "MISMATCH"):
        result.detail = "replay is not comparable"
        return result
    workload = report.get("workload_exit") or {}
    if workload.get("code") not in (0, 1):
        result.detail = f"scenario did not produce a verdict: {workload}"
        return result
    if workload["code"] != expected:
        result.detail = "scenario exit contradicts recorded outcome"
        return result

    world = json.loads(retained("vmm.json"))
    text = retained(f"machines/{workload['service']}.console")
    if workload["service"] == "client":
        lines = re.findall(r"^RESULT (?:pass|fail) .+$", text, re.M)
        if len(lines) != 1 or lines[0].split()[1] != record["outcome"].lower():
            result.detail = "missing or contradictory workload RESULT"
            return result
        result.result_line = lines[0]
    result.status = PASS if expected == 0 else FAIL
    result.detail = result.result_line or f"{workload['service']} exited {expected}"
    result.workload_exit = workload["code"]
    result.world_witness = report["witness"]
    result.state_digests = report.get("full_state") or {}
    result.machine_witnesses = {m["name"]: m["witness"] for m in world["machines"]}
    result.exits = {m["name"]: m["exits"] for m in world["machines"]}
    result.sim_seconds = world["sim_seconds"]
    result.logs = {workload["service"]: text}
    result.faults = report["faults"]
    result.network_counters = world.get("network_counters", {})
    result.replay_matches = comparison.get("status") != "MISMATCH"
    return result
