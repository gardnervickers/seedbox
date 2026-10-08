#!/usr/bin/env python3
"""Generate three self-contained MinIO projects with verified local inputs."""
import argparse
import json
from pathlib import Path
import shutil
from stage_inputs import stage

HERE = Path(__file__).resolve().parent
MODES = ("qualification", "broken", "fixed")

def generate(destination, minio, wheels):
    destination = Path(destination)
    destination.mkdir(parents=True)  # Refuse to overwrite any prior inputs.
    for mode in MODES:
        project = destination / mode
        project.mkdir()
        for context in ("minio", "client"):
            shutil.copytree(HERE / context, project / context,
                            ignore=shutil.ignore_patterns("__pycache__", "*.pyc", "wheels", "minio"))
        stage(project, minio, wheels)
        template = "compose.yaml" if mode == "qualification" else f"compose.{mode}.yaml"
        compose = json.loads((HERE / template).read_text())
        (project / "compose.yaml").write_text(json.dumps(compose, indent=2) + "\n")
        shutil.copyfile(HERE / "inputs.lock.json", project / "inputs.lock.json")
        (project / ".gitignore").write_text(".seedbox/\n")
        for context in ("minio", "client"):
            (project / context / ".dockerignore").write_text("__pycache__\n*.pyc\n.git\n.seedbox\n")
    return destination

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--minio", required=True, type=Path)
    parser.add_argument("--wheel-directory", required=True, type=Path)
    args = parser.parse_args()
    print(generate(args.destination, args.minio, args.wheel_directory))
