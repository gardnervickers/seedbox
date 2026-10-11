"""Make the spawn-cases project from a generated `fixed` project.

    python3 examples/faults/generate.py /abs/projects
    python3 examples/faults/spawn.py /abs/projects
    seedbox run /abs/projects/spawn-cases
"""
from pathlib import Path
import shutil
import sys

SERVER = '    command: [server, --listen, "0.0.0.0:7000", --state, /data/kv.log]'
SCENARIO = '    command: [scenario, --control-address, "client:7001"]'


def spawn_project(projects):
    destination = Path(projects) / "spawn-cases"
    shutil.copytree(Path(projects) / "fixed", destination)
    compose = destination / "compose.yaml"
    text = compose.read_text().replace("name: synthetic-fault-fixed", "name: synthetic-fault-spawn")
    assert text.count(SERVER) == 1 and text.count(SCENARIO) == 1
    text = text.replace(SERVER, SERVER + "\n    entrypoint: [python3, /workload/control_target.py]")
    text = text.replace(SCENARIO, SCENARIO + "\n    entrypoint: [python3, /workload/spawn_scenario.py]")
    compose.write_text(text)
    return destination


if __name__ == "__main__":
    print(spawn_project(sys.argv[1]) / "compose.yaml")
