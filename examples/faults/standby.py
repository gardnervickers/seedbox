"""Make the standby-cases project from a generated `fixed` project.

    python3 examples/faults/generate.py /abs/projects
    python3 examples/faults/standby.py /abs/projects
    seedbox run /abs/projects/standby-cases --timebase pv
"""
from pathlib import Path
import shutil
import sys

SERVER = '    command: [server, --listen, "0.0.0.0:7000", --state, /data/kv.log]'
SCENARIO = '    command: [scenario, --control-address, "client:7001"]'


def standby_project(projects):
    destination = Path(projects) / "standby-cases"
    shutil.copytree(Path(projects) / "fixed", destination)
    compose = destination / "compose.yaml"
    text = compose.read_text().replace("name: synthetic-fault-fixed", "name: synthetic-fault-standby")
    assert text.count(SERVER) == 1 and text.count(SCENARIO) == 1
    text = text.replace(SERVER, SERVER + "\n    entrypoint: [python3, /workload/control_target.py]")
    text = text.replace(SCENARIO, SCENARIO + "\n    entrypoint: [python3, /workload/standby_scenario.py]")
    text = text.replace("  control: {service: scenario}",
                        "  control: {service: scenario}\n  standby:\n    spare: {from: server}")
    compose.write_text(text)
    return destination


if __name__ == "__main__":
    print(standby_project(sys.argv[1]) / "compose.yaml")
