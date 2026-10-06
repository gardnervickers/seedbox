#!/usr/bin/env python3
"""Generate self-contained synthetic TCP recovery and broken-server controls."""
import argparse
from pathlib import Path
import shutil

HERE = Path(__file__).resolve().parent

def generate(destination, seed=1009):
    if not 0 <= seed <= 2**64 - 1:
        raise ValueError("seed must be an unsigned64-bit integer")
    destination = Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    base = (HERE / "compose.yaml").read_text().replace("  seed: 1009", f"  seed: {seed}")
    for variant in ("fixed", "broken", "broken-no-faults"):
        project = destination / variant
        project.mkdir()  # Never overwrite a prior project or its run inputs.
        shutil.copytree(HERE / "workload", project / "workload",
                        ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        shutil.copytree(HERE.parents[1] / "python" / "seedbox", project / "workload" / "seedbox",
                        ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        text = base.replace("synthetic-fault-fixed", "synthetic-fault-" + variant)
        if variant != "fixed":
            text = text.replace("--state, /data/kv.log]", "--state, /data/kv.log, --broken]")
        if variant == "broken-no-faults":
            text = text.replace('[scenario, --control-address, "client:7001"]',
                                '[scenario, --control-address, "client:7001", --no-faults]')
        (project / "compose.yaml").write_text(text)
        (project / ".gitignore").write_text(".seedbox/\n")
        (project / "workload" / ".dockerignore").write_text("__pycache__\n*.pyc\n.git\n.seedbox\n")
    return destination

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination")
    parser.add_argument("--seed", type=int, default=1009)
    args = parser.parse_args()
    print(generate(args.destination, args.seed))
