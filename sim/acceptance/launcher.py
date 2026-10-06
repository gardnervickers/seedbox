"""Runs acceptance jobs through sim/batch.sh, with campaign.sh's result rules.

Each job is one fresh VMM process with fresh output under
runs/<suite>/. Jobs that share a scenario, witness mode, performance settings
and engine go into one batch.sh invocation; batches run concurrently on
disjoint core ranges (and disjoint SMT siblings) taken from `cpus`. batch.sh
holds the shared per-core lease for every batch, so the suite cannot collide
with campaigns or other agents.
"""
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import hashlib
import json
import math
import os
from pathlib import Path
import re
import subprocess
import time

from .model import EXITS, FAIL, FULL_STATE, PASS, UNRESOLVED, RunResult

# Performance-only job settings and the variables batch.sh forwards for them.
SETTINGS = {"undo": "SEEDBOX_UNDO", "margin": "SEEDBOX_MARGIN"}
# Same margin as campaign.sh's overshoot retry.
RETRY_MARGIN = "98304"
LEASE_BUSY = "already owned"
# A workload with this name runs a checker and must print one RESULT line
# (the campaign.sh / campaign-result.py convention).
CHECKER = "client"


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def cpu_list(value):
    cpus = []
    for item in value.split(","):
        low, _, high = item.partition("-")
        cpus += range(int(low), int(high or low) + 1)
    return sorted(set(cpus))


def sysfs_siblings(cpu):
    path = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list")
    return set(cpu_list(path.read_text().strip())) if path.exists() else {cpu}


def read(path):
    try:
        return Path(path).read_text(errors="replace")
    except OSError:
        return ""


def tail(path, lines=3):
    return " | ".join(read(path).strip().splitlines()[-lines:])[:300]


class BatchLauncher:
    def __init__(self, sim, root, scenarios, cpus, out, engines, artifacts=None,
                 timeout=1800, lease_wait=0, siblings=sysfs_siblings, log=print):
        self.sim, self.root, self.out = Path(sim), Path(root).resolve(), Path(out).resolve()
        self.scenarios, self.cpus, self.engines = scenarios, sorted(cpus), engines
        self.artifacts = Path(artifacts).resolve() if artifacts else None
        self.timeout, self.lease_wait, self.log = timeout, lease_wait, log
        self.siblings = {cpu: siblings(cpu) for cpu in self.cpus}
        self.prefix = self.out.relative_to(self.root / "runs")
        self.batches = 0
        self._worlds = {}

    # Inputs -----------------------------------------------------------------

    def _source(self, scenario):
        return self.sim / self.scenarios[scenario]

    def _resolve(self, scenario):
        """World config plus absolute kernel/initrd paths (optionally from --artifacts)."""
        source = self._source(scenario)
        world = json.loads(source.read_text())
        def place(rel):
            path = source.parent / rel
            return str(self.artifacts / path.name if self.artifacts else path.resolve())
        world["kernel"] = place(world["kernel"])
        for machine in world["machines"]:
            machine["initrd"] = place(machine["initrd"])
        return world

    def world(self, scenario):
        if scenario not in self._worlds:
            path = self.out / "worlds" / f"{scenario}.json"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(self._resolve(scenario), indent=2))
            self._worlds[scenario] = path
        return self._worlds[scenario]

    def identity(self, scenario):
        """Hashes of the inputs that define run identity (the VMM is recorded separately)."""
        world = self._resolve(scenario)
        ids = {"world": sha256(self._source(scenario)), "kernel": sha256(world["kernel"])}
        for path in sorted({m["initrd"] for m in world["machines"]}):
            ids[f"initrd:{Path(path).name}"] = sha256(path)
        return ids

    def engine_hashes(self):
        return {name or "candidate": sha256(path) for name, path in self.engines.items()}

    def missing_inputs(self, scenarios):
        missing = [str(path) for path in self.engines.values() if not Path(path).is_file()]
        for scenario in scenarios:
            if not self._source(scenario).is_file():
                missing.append(str(self._source(scenario)))
                continue
            world = self._resolve(scenario)
            missing += [p for p in [world["kernel"], *(m["initrd"] for m in world["machines"])]
                        if not Path(p).is_file()]
        return sorted(set(missing))

    # Execution ----------------------------------------------------------------

    def run(self, jobs):
        results = self._round(list(enumerate(jobs)), retry=False)
        overshoots = [(i, job) for i, job in enumerate(jobs)
                      if results[i].status == UNRESOLVED and "overshoot" in read(f"{results[i].evidence}.err")]
        if overshoots:
            # campaign.sh: retry overshoots once with a larger margin; never a
            # clock bound error. A retry that still fails stays UNRESOLVED.
            for i, result in self._round(overshoots, retry=True).items():
                result.retries = 1
                first = {"evidence": results[i].evidence, "detail": results[i].detail}
                result.attempts = results[i].attempts + [first] + result.attempts
                if result.status == UNRESOLVED:
                    result.detail = f"after overshoot retry: {result.detail}"
                results[i] = result
        return [results[i] for i in range(len(jobs))]

    def _round(self, indexed, retry):
        groups = {}
        for i, job in indexed:
            groups.setdefault((job.scenario, job.witness, job.performance, job.engine), []).append((i, job))
        width = self._widest()
        chunks = [items[n:n + width] for items in groups.values() for n in range(0, len(items), width)]
        chunks.sort(key=len, reverse=True)
        for _, job in indexed:
            self.world(job.scenario)
        results, running = {}, {}
        with ThreadPoolExecutor(max_workers=len(self.cpus)) as pool:
            while chunks or running:
                for chunk in list(chunks):
                    cpus = self._allocate(len(chunk), [c for cs in running.values() for c in cs])
                    if cpus is not None:
                        chunks.remove(chunk)
                        self.batches += 1
                        name = f"b{self.batches:03d}" + ("-retry" if retry else "")
                        running[pool.submit(self._batch, name, chunk, cpus[0], retry)] = cpus
                if not running:
                    raise RuntimeError("no core range can hold the next batch")
                done, _ = wait(running, return_when=FIRST_COMPLETED)
                for future in done:
                    del running[future]
                    results.update(future.result())
        return results

    def _widest(self):
        best = run = 0
        for n, cpu in enumerate(self.cpus):
            run = run + 1 if n and cpu == self.cpus[n - 1] + 1 else 1
            best = max(best, run)
        return best

    def _allocate(self, count, busy):
        """First run of `count` consecutive free cores whose SMT siblings are all idle."""
        taken = set().union(*(self.siblings[c] for c in busy)) if busy else set()
        for start in self.cpus:
            span = list(range(start, start + count))
            if all(c in self.siblings for c in span) and not taken & set().union(*(self.siblings[c] for c in span)):
                return span
        return None

    def _batch(self, name, chunk, cpu0, retry):
        _, first = chunk[0]
        env = {k: v for k, v in os.environ.items() if k not in SETTINGS.values()}
        env.update(SEEDBOX_ROOT=str(self.root), CPU0=str(cpu0), SEEDBOX_TIMEOUT=str(self.timeout),
                   VMM=str(self.engines[first.engine]))
        for key, value in first.performance:
            env[SETTINGS[key]] = value
        if retry:
            env["SEEDBOX_MARGIN"] = RETRY_MARGIN
        seeds = " ".join(str(job.seed) for _, job in chunk)
        extra = ["--digest"] if first.witness == FULL_STATE else []
        label = f"{name} {first.describe().split(' seed=')[0]} seeds [{seeds}] cpus {cpu0}-{cpu0 + len(chunk) - 1}"
        deadline = time.monotonic() + self.lease_wait
        attempt, waited = 0, []
        while True:
            prefix = f"{self.prefix}/{name}" + (f"-w{attempt}" if attempt else "")
            log = self.root / "runs" / f"{prefix}.batch.log"
            log.parent.mkdir(parents=True, exist_ok=True)
            self.log(f"start {label}")
            started = time.monotonic()
            with log.open("w") as stream:
                code = subprocess.run([str(self.sim / "batch.sh"), str(self.world(first.scenario)), prefix, seeds, *extra],
                                      env=env, stdout=stream, stderr=subprocess.STDOUT).returncode
            if code and LEASE_BUSY in log.read_text() and time.monotonic() < deadline:
                self.log(f"lease busy for {label}; waiting")
                waited.append({"evidence": str(log), "detail": tail(log)})
                time.sleep(30)
                attempt += 1
                continue
            break
        results = {}
        for n, (i, job) in enumerate(chunk):
            out = self.root / "runs" / f"{prefix}-s{job.seed}-r{n}"
            result = self.parse(job, out, log)
            result.attempts = list(waited)
            if code and result.completed:
                # batch.py reports MSR readback/restore failures after its
                # runs finish; like campaign.sh, never accept such a batch.
                result.status, result.detail = UNRESOLVED, f"batch.sh exited {code}: {tail(log)}"
            results[i] = result
        self.log(f"done  {label} in {time.monotonic() - started:.0f}s: "
                 + " ".join(r.status for r in results.values()))
        return results

    @staticmethod
    def acknowledged(out, schedule):
        """The VMM reports the kill schedule; a kill counts as applied only when
        the member's guest acknowledged it on COM2 ("<name> stopped/started")."""
        faults, seen = [], {}
        for kill in sorted(schedule, key=lambda k: k["stop_ns"]):
            member = kill["member"]
            n = seen[member] = seen.get(member, 0) + 1
            lines = read(out / f"{member}.mgmt").splitlines()
            faults.append({**kill, "stopped": lines.count(f"{member} stopped") >= n,
                           "restarted": lines.count(f"{member} started") >= n})
        return faults

    def parse(self, job, out, batch_log):
        """Accept one completed attempt, mirroring campaign-result.py for any workload."""
        result = RunResult(job, UNRESOLVED, evidence=str(out))
        err = Path(f"{out}.err")
        try:
            status = Path(f"{out}.status").read_text().strip()
        except OSError:
            result.detail = f"batch did not run: {tail(batch_log)}"
            return result
        try:
            report = json.loads(Path(f"{out}.json").read_text())
        except (OSError, ValueError):
            report = None
        if status != "0" or not isinstance(report, dict):
            result.detail = f"exit status {status}: {tail(err) or 'no terminal JSON'}"
            return result
        machines = {m.get("name"): m for m in report.get("machines", [])}
        workload = json.loads(self._source(job.scenario).read_text()).get("end_on_exit")
        host = report.get("host_seconds")
        state = machines.get(workload, {}).get("state", "")
        exited = re.fullmatch(r"Exited\((\d+)\)", state)
        if report.get("seed") != job.seed or type(report.get("seed")) is not int:
            result.detail = "result belongs to a different seed"
        elif report.get("outcome") != "end_machine_exited":
            result.detail = f"missing terminal outcome ({report.get('outcome')})"
        elif type(host) not in (int, float) or not math.isfinite(host) or host < 0:
            result.detail = "missing valid completion timing"
        elif not exited:
            result.detail = f"workload {workload} did not exit ({state or 'missing'})"
        if result.detail:
            return result
        console = (out / f"{workload}.console")
        text = console.read_text(errors="replace") if console.exists() else ""
        lines = [l for l in text.splitlines() if l.split()[:2] in (["RESULT", "pass"], ["RESULT", "fail"])]
        code = int(exited[1])
        result.workload_exit = code
        if workload == CHECKER:
            if len(lines) != 1:
                other = [l for l in text.splitlines() if l.startswith("RESULT")]
                result.detail = (f"expected one client RESULT pass|fail, client exited {code}"
                                 + (f": {other[-1][:200]}" if other else ""))
                return result
            verdict = lines[0].split()[1]
            if code != (0 if verdict == "pass" else 1):
                result.detail = "client RESULT contradicts checker exit status"
                return result
            result.status, result.result_line, result.detail = (PASS if verdict == "pass" else FAIL), lines[0], lines[0]
        else:
            result.status, result.detail = (PASS if code == 0 else FAIL), f"{workload} exited {code}"
        result.world_witness = report.get("world_witness")
        result.machine_witnesses = {n: m.get("witness") for n, m in machines.items()}
        result.state_digests = {n: m["state_digest"] for n, m in machines.items() if m.get("state_digest")}
        result.exits = {n: m.get("exits") for n, m in machines.items()}
        result.sim_seconds, result.host_seconds = report.get("sim_seconds"), host
        result.logs = {workload: text}
        result.faults = self.acknowledged(out, report.get("kills", []))
        return result
