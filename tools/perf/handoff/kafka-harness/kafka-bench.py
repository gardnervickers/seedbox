#!/usr/bin/env python3
"""Own-server Kafka benchmark. Evidence is saved before catalogue cleanup.

python3 kafka-bench.py --worktree /path/to/seedbox --state /tmp/own-state \
    --output /path/to/evidence --workers 4 --replay
Use --compare-summary OTHER/summary.json for a 1-vs-4 witness comparison.
Host receipt offsets are from submission, not inferred execution timestamps.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess as sp
import threading
import time

TERMINAL = {"completed", "failed", "stopped"}
MARKERS = {"mbean": "Log4jController", "controller": "Starting controller",
           "started": "Kafka Server started", "ready": "READY", "result": "RESULT"}
FATAL = re.compile(r"fatal error.*Java Runtime|SIGSEGV|hs_err_pid|Internal Error \(", re.I)
KRAFT = re.compile(r"\[RaftManager .*Completed transition to ", re.I)
LEADER = re.compile(r"Completed transition to Leader\(", re.I)
TIMEOUT = re.compile(r"due to request timeout|request[^\n]*timed out|TimeoutException", re.I)


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def analyze_logs(path, start_ns):
    """Reassemble lines per stream/incarnation; timestamps belong to final byte."""
    pending = {}
    result = {"milestones": {}, "fatalErrors": [], "kraftTransitions": [], "leaderTransitions": [], "requestTimeoutLines": 0,
              "omissions": []}
    def process_line(text, sim_ns, received_ns):
        stamp = {"simulatedTimeNs": sim_ns,
                 "receivedUnixNs": received_ns,
                 "receiptWallSeconds": (received_ns - start_ns) / 1e9,
                 "text": text}
        for name, marker in MARKERS.items():
            if marker in text and (name != "mbean" or "MBean" in text) and name not in result["milestones"]:
                result["milestones"][name] = stamp
        if FATAL.search(text):
            result["fatalErrors"].append(stamp)
        if KRAFT.search(text):
            result["kraftTransitions"].append(stamp)
            if LEADER.search(text):
                result["leaderTransitions"].append(stamp)
        if TIMEOUT.search(text):
            result["requestTimeoutLines"] += 1
    last_stamps = {}
    with path.open() as source:
        for raw in source:
            timed = json.loads(raw)
            record = timed["record"]
            entry = record.get("entry", {})
            if entry.get("type") != "bytes":
                if record.get("type") not in {"complete", "closed"}:
                    result["omissions"].append(record)
                continue
            key = (entry.get("stream"), entry.get("incarnation"))
            last_stamps[key] = (entry["simulatedTimeNs"], timed["receivedUnixNs"])
            data = pending.get(key, b"") + bytes(entry["bytes"])
            lines = data.split(b"\n")
            pending[key] = lines.pop()
            for line in lines:
                text = line.decode("utf-8", errors="replace")
                process_line(text, entry["simulatedTimeNs"], timed["receivedUnixNs"])
    for key, tail in pending.items():
        if tail:
            process_line(tail.decode("utf-8", errors="replace"), *last_stamps[key])
    result["unterminatedText"] = [v.decode("utf-8", errors="replace") for v in pending.values() if v]
    return result


class Bench:
    def __init__(self, args):
        self.a = args
        self.binary = str((args.binary or args.worktree / "target/release/seedbox").resolve())
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("SEEDBOX_")}
        self.env["SEEDBOX_STATE"] = str(args.state.resolve())
        self.ids = []
        self.server = None
        self.saved = set()
        self.followers = []
        self.stop = threading.Event()

    def cmd(self, *args, check=True):
        p = sp.run([self.binary, *args], env=self.env, capture_output=True, text=True)
        if check and p.returncode:
            raise RuntimeError(f"{args}: exit {p.returncode}: {p.stderr}")
        return p

    def document(self, *args):
        return json.loads(self.cmd(*args, "--json").stdout)

    def host_sample(self):
        processes = sp.check_output(["ps", "aux"], text=True)
        others = [line for line in processes.splitlines()
                  if "seedbox serve" in line and (not self.server or str(self.server.pid) not in line.split()[:2])]
        # ps does not show SEEDBOX_STATE; PID is the ownership boundary.
        return {"unixNs": time.time_ns(), "uptime": sp.check_output(["uptime"], text=True).strip(),
                "topCpu": sp.check_output(["sh", "-c", "ps -Ao pid,pcpu,pmem,command -r | head -21"], text=True),
                "otherServers": others}

    def monitor(self):
        with (self.a.output / "host-load.jsonl").open("a") as f:
            while True:
                f.write(json.dumps(self.host_sample()) + "\n")
                f.flush()
                if self.stop.wait(60):
                    break
            f.write(json.dumps(self.host_sample()) + "\n")

    def capture(self, run, machine, directory):
        err = (directory / (machine + ".stderr")).open("w")
        args = ["events", run, "--follow"] if machine == "events" else ["logs", run, machine, "--follow", "--json"]
        proc = sp.Popen([self.binary, *args], env=self.env, stdout=sp.PIPE, stderr=err, text=True)
        self.followers.append(proc)
        def read():
            with (directory / (machine + "-received.jsonl")).open("w") as timed:
                for line in proc.stdout:
                    now = time.time_ns()
                    try:
                        record = json.loads(line)
                    except json.JSONDecodeError:
                        record = {"unparsed": line}
                    timed.write(json.dumps({"receivedUnixNs": now, "record": record}) + "\n")
                    timed.flush()
            proc.wait()
            err.close()
        thread = threading.Thread(target=read)
        thread.start()
        return proc, thread

    def observe(self, run, label, start_ns):
        self.ids.append(run)
        directory = self.a.output / label
        directory.mkdir()
        resource = self.document("get", "simulation", run)
        machines = list(resource["spec"]["machines"]) + ["oracle"]
        followers = [self.capture(run, m, directory) for m in machines + ["events"]]
        deadline = time.monotonic() + self.a.monitor_timeout
        while resource["status"]["phase"] not in TERMINAL:
            if time.monotonic() > deadline:
                raise TimeoutError("monitor deadline exceeded; evidence preserved, simulation not deleted")
            time.sleep(2)
            resource = self.document("get", "simulation", run)
            save(directory / "status.json", resource)
        save(directory / "status.json", resource)
        save(directory / "world.json", self.document("get", "world", run))
        for proc, thread in followers:
            try:
                proc.wait(timeout=30)
            except sp.TimeoutExpired:
                proc.terminate()
            thread.join(timeout=10)
        outputs = list(self.a.state.glob(f"sessions/session-*/runs/{run}/attempts/*/output"))
        native = []
        for i, output in enumerate(outputs):
            dest = directory / f"native-{i}"
            shutil.copytree(output, dest)
            if (dest / "vmm.json").exists():
                native.append(json.loads((dest / "vmm.json").read_text()))
        status = resource["status"]
        sim = status.get("simulatedTimeNs")
        wall = status.get("stageTimings", {}).get("executing", {}).get("elapsedWallNs")
        summary = {"id": run, "verdict": status.get("verdict"), "phase": status["phase"],
                   "terminalSimulatedTimeNs": sim, "executionWallNs": wall,
                   "simulatedOverWall": sim / wall if sim is not None and wall else None,
                   "logs": {m: analyze_logs(directory / (m + "-received.jsonl"), start_ns) for m in machines},
                   "nativeReportsAvailable": len(native)}
        summary["witnesses"] = [{"world": n.get("world_witness"), "network": n.get("network_witness"), "horizon": n.get("horizon_witness"),
                                  "machines": {m["name"]: m.get("witness") for m in n.get("machines", [])}}
                                 for n in native]
        summary["scheduler"] = [{k: v for k, v in n.items() if "round" in k or "schedul" in k or "window" in k}
                                for n in native]
        save(directory / "summary.json", summary)
        self.saved.add(run)
        return summary

    def run(self):
        self.a.output.mkdir(parents=True, exist_ok=False)
        # Never connect to or shut down an existing server in the supplied state.
        if self.cmd("get", "simulations", check=False).returncode == 0:
            raise RuntimeError("state already has a server; refusing to take ownership")
        self.a.state.mkdir(parents=True, exist_ok=True)
        save(self.a.output / "options.json", {k: str(v) if isinstance(v, Path) else v for k, v in vars(self.a).items()})
        server_log = (self.a.output / "server.log").open("w")
        self.server = sp.Popen([self.binary, "serve", "--machine-workers", str(self.a.workers)],
                               env=self.env, stdout=server_log, stderr=sp.STDOUT)
        monitor = None
        try:
            for _ in range(120):
                if self.server.poll() is not None:
                    raise RuntimeError("own server exited; see server.log")
                if self.cmd("get", "simulations", check=False).returncode == 0:
                    break
                time.sleep(1)
            else:
                raise TimeoutError("own server did not become ready")
            monitor = threading.Thread(target=self.monitor)
            monitor.start()
            start = time.time_ns()
            save(self.a.output / "baseline-start.json", {"submissionStartUnixNs": start})
            submission = self.document("run", str(self.a.project or self.a.worktree / "examples/kafka"),
                                       "--seed", str(self.a.seed), "--wall-time", self.a.wall_time, "--detach")
            save(self.a.output / "submission.json", submission)
            baseline = self.observe(submission["metadata"]["id"], "baseline", start)
            result = {"baseline": baseline}
            if self.a.replay:
                # Replay reports its child ID immediately on stderr, before execution.
                replay_start = time.time_ns()
                save(self.a.output / "replay-start.json", {"submissionStartUnixNs": replay_start})
                replay = sp.Popen([self.binary, "replay", baseline["id"], "--wall-time", self.a.wall_time, "--json"],
                                  env=self.env, stdout=(self.a.output / "replay-comparison.json").open("w"),
                                  stderr=sp.PIPE, text=True)
                with (self.a.output / "replay.stderr").open("w") as err:
                    for line in replay.stderr:
                        err.write(line)
                        err.flush()
                        match = re.match(r"simulation: (\S+)", line)
                        if match:
                            result["replay"] = self.observe(match[1], "replay", replay_start)
                    replay.wait(timeout=60)
                result["replayExitCode"] = replay.returncode
                result["comparison"] = json.loads((self.a.output / "replay-comparison.json").read_text())
            if self.a.compare_summary:
                other = json.loads(self.a.compare_summary.read_text())["baseline"]
                result["crossRunWitnessMatches"] = baseline["witnesses"] == other["witnesses"] if baseline["witnesses"] and other["witnesses"] else None
            save(self.a.output / "summary.json", result)
            rows = ["# Kafka benchmark", "", "| Run | Verdict | Sim s | Execution wall s | S/W |", "|---|---|---:|---:|---:|"]
            for key in ("baseline", "replay"):
                if key in result:
                    r = result[key]
                    rows.append(f"| {key} | {r['verdict']} | {(r['terminalSimulatedTimeNs'] or 0)/1e9:.3f} | {(r['executionWallNs'] or 0)/1e9:.3f} | {r['simulatedOverWall']} |")
                    for machine, log in r["logs"].items():
                        rows.append(f"\n{key}/{machine}: " + json.dumps(log))
            rows.append("\nReplay comparison: " + json.dumps(result.get("comparison")))
            rows.append("\nHost load/overlap: host-load.jsonl. Receipt times include preparation and boot; simulated timestamps are log-record timestamps, not Kafka text timestamps. Counts are matching log lines, not unique protocol requests. Bounded log omissions and unterminated text are retained in JSON.")
            (self.a.output / "summary.md").write_text("\n".join(rows) + "\n")
        finally:
            for proc in self.followers:
                if proc.poll() is None:
                    proc.terminate()
            if monitor:
                self.stop.set()
                monitor.join()
            # Unfinished runs are not deleted; own shutdown cancels them. Evidence
            # collected so far remains, and the error is not disguised as success.
            with (self.a.output / "cleanup.log").open("w") as cleanup:
                for run in self.ids:
                    if run in self.saved:
                        p = self.cmd("delete", "simulation", run, check=False)
                        cleanup.write(p.stdout + p.stderr)
                if self.server.poll() is None:
                    p = self.cmd("shutdown", check=False)
                    cleanup.write(p.stdout + p.stderr)
                    try:
                        self.server.wait(timeout=120)
                    except sp.TimeoutExpired:
                        self.server.terminate()
                        self.server.wait(timeout=30)
            server_log.close()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--worktree", type=Path, required=True)
    p.add_argument("--binary", type=Path)
    p.add_argument("--state", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--project", type=Path)
    p.add_argument("--workers", type=int, choices=range(1, 65), default=4)
    p.add_argument("--seed", type=int, default=1009)
    p.add_argument("--wall-time", default="60m")
    p.add_argument("--monitor-timeout", type=float, default=7500)
    p.add_argument("--replay", action="store_true")
    p.add_argument("--compare-summary", type=Path)
    Bench(p.parse_args()).run()


if __name__ == "__main__":
    main()
