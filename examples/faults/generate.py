#!/usr/bin/env python3
"""Generate self-contained synthetic TCP recovery and broken-server controls."""
import argparse
from pathlib import Path
import shutil

HERE = Path(__file__).resolve().parent

def generate(destination, *, batch=32):
    if type(batch) is not int or not 1 <= batch <= 32:
        raise ValueError("batch must be between 1 and 32")
    destination = Path(destination)
    destination.mkdir(parents=True, exist_ok=True)
    base = (HERE / "seedbox.yaml").read_text()
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
        if batch != 32:
            # Both peers use the same explicit workload size; source defaults and
            # the full acceptance matrix remain unchanged.
            text = text.replace('"server:7000"]', f'"server:7000", --batch, "{batch}"]')
            text = text.replace('"client:7001"', f'"client:7001", --batch, "{batch}"')
        (project / "seedbox.yaml").write_text(text)
        (project / ".gitignore").write_text(".seedbox/\n")
        (project / "workload" / ".dockerignore").write_text("__pycache__\n*.pyc\n.git\n.seedbox\n")
    return destination


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
        compose = destination / "seedbox.yaml"
        text = compose.read_text().replace("name: synthetic-fault-fixed", "name: synthetic-fault-" + name)
        server = '    command: [server, --listen, "0.0.0.0:7000", --state, /data/kv.log]\n'
        assert text.count(server) == 1, "missing canonical server command"
        text = text.replace(server, server + '    environment: {SEEDBOX_TEST_DELAY_READY: "1"}\n')
        if name == "readiness-cutoff":
            text = text.replace("simulated_time: 30s", "simulated_time: 3s")
        compose.write_text(text)
        (destination / "workload" / "scenario.py").write_text(program)


def control_project(projects, name="control-cases", scenario="control_scenario.py"):
    destination = Path(projects) / name
    shutil.copytree(Path(projects) / "fixed", destination)
    compose = destination / "seedbox.yaml"
    text = compose.read_text().replace("name: synthetic-fault-fixed", "name: synthetic-fault-" + name)
    server = '    command: [server, --listen, "0.0.0.0:7000", --state, /data/kv.log]'
    assert text.count(server) == 1
    text = text.replace(server, server + '\n    entrypoint: [python3, /workload/control_target.py]')
    compose.write_text(text)
    shutil.copyfile(destination / "workload" / scenario, destination / "workload/scenario.py")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination")
    args = parser.parse_args()
    print(generate(args.destination))
