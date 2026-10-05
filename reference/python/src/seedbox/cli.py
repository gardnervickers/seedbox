import argparse
import json
from pathlib import Path
import sys

from .config import load_manifest
from .errors import SimulationError
from .runtime import Simulation, canonical


def main() -> int:
    parser = argparse.ArgumentParser(description="Seedbox deterministic execution prototype")
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="execute a fresh simulation from immutable inputs and a seed")
    run.add_argument("manifest", type=Path)
    run.add_argument("--seed", type=int, default=0)
    run.add_argument("--output", type=Path)
    run.add_argument("--instruction-trace", action="store_true")
    args = parser.parse_args()
    try:
        config, specs = load_manifest(args.manifest)
        report = Simulation(specs, args.seed, config, args.instruction_trace).run()
    except (ValueError, KeyError, TypeError, OSError, SimulationError) as error:
        print(json.dumps({"status": "invalid_input", "error": str(error)}, sort_keys=True), file=sys.stderr)
        return 2
    data = canonical(report) + b"\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_bytes(data)
        summary = {key: report[key] for key in ("status", "error", "seed", "instructions", "virtual_time_ns", "instruction_sha256", "trace_sha256")}
        print(json.dumps(summary, sort_keys=True))
    else:
        sys.stdout.buffer.write(data)
    return 0 if report["status"] == "completed" else 1
