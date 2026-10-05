#!/usr/bin/env python3
"""Acceptance checks using fresh runs, independent oracles, and host CPU load."""

import argparse
from dataclasses import replace
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import time

from seedbox.config import NetworkConfig, load_manifest
from seedbox.runtime import Simulation

ROOT = Path(__file__).resolve().parents[1]


def fresh_report(manifest: str, output: Path) -> bytes:
    result = subprocess.run([sys.executable, "-m", "seedbox", "run", str(ROOT / manifest),
                             "--seed", "42", "--output", str(output)], capture_output=True, check=True)
    if result.stderr:
        raise RuntimeError(result.stderr.decode())
    return output.read_bytes()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--seeds", type=int, default=64)
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/verification.json")
    args = parser.parse_args()
    if not 1 <= args.seeds <= 10000:
        parser.error("--seeds must be in 1..10000")
    subprocess.run([sys.executable, str(ROOT / "scripts/build_guests.py")], check=True)
    directory = args.output.parent
    directory.mkdir(parents=True, exist_ok=True)
    baseline = fresh_report("examples/cluster.json", directory / "baseline.json")
    fresh = fresh_report("examples/cluster.json", directory / "fresh.json")
    if baseline != fresh:
        raise RuntimeError("fresh executions diverged")
    noise = [subprocess.Popen([sys.executable, "-c", "x=0\nwhile True:\n x=(x*1664525+1013904223)&0xffffffff"],
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) for _ in range(2)]
    try:
        loaded = fresh_report("examples/cluster.json", directory / "under-load.json")
    finally:
        for worker in noise:
            worker.terminate()
        for worker in noise:
            worker.wait(timeout=10)
    if baseline != loaded:
        raise RuntimeError("host CPU load changed the execution")

    race_config, race_nodes = load_manifest(ROOT / "examples/race.json")
    fixed_config, fixed_nodes = load_manifest(ROOT / "examples/atomic.json")
    failures, counters = [], []
    started = time.perf_counter()
    for seed in range(args.seeds):
        broken = Simulation(race_nodes, seed, race_config).run()
        fixed = Simulation(fixed_nodes, seed, fixed_config).run()
        if broken["status"] != "completed" or fixed["status"] != "completed":
            raise RuntimeError(f"guest failed during seed {seed}")
        counter = int.from_bytes(bytes.fromhex(broken["nodes"]["race"]["stdout_hex"]), "little")
        fixed_counter = int.from_bytes(bytes.fromhex(fixed["nodes"]["race"]["stdout_hex"]), "little")
        counters.append(counter)
        if counter != 64:
            failures.append(seed)
        if fixed_counter != 64:
            raise RuntimeError(f"atomic negative-control fix failed at seed {seed}: {fixed_counter}")
    elapsed = time.perf_counter() - started
    if not failures or len(set(counters)) < 2:
        raise RuntimeError("seed sweep failed to expose and vary the intentional data race")

    cfg, specs = load_manifest(ROOT / "examples/cluster.json")
    faulted = Simulation(specs, 42, replace(cfg, network=NetworkConfig(loss_ppm=1_000_000))).run()
    if faulted["status"] != "deadlock":
        raise RuntimeError("total packet loss did not expose the expected blocked protocol")
    original = json.loads(baseline)
    summary = {
        "fresh_process_reports_identical": True,
        "reports_identical_under_two_host_cpu_workers": True,
        "report_sha256": hashlib.sha256(baseline).hexdigest(),
        "instruction_sha256": original["instruction_sha256"],
        "trace_sha256": original["trace_sha256"],
        "race_seed_count": args.seeds,
        "race_failure_count": len(failures),
        "race_counter_range": [min(counters), max(counters)],
        "first_failing_seeds": failures[:10],
        "atomic_fix_pass_count": args.seeds,
        "packet_loss_oracle": faulted["status"],
        # Wall-clock timing is deliberately outside the deterministic report.
        "seed_sweep_wall_seconds": elapsed,
        "seed_sweep_executions_per_second": 2 * args.seeds / elapsed,
        "cluster_virtual_time_ns": original["virtual_time_ns"],
    }
    args.output.write_text(json.dumps(summary, sort_keys=True, indent=2) + "\n")
    print(json.dumps(summary, sort_keys=True, indent=2))


if __name__ == "__main__":
    main()
