"""Stage self-contained YAML projects; the public CLI owns all OCI preparation."""
from pathlib import Path
import shutil


def prepare_projects(harness, cases, scenarios, out):
    harness, out = Path(harness).resolve(), Path(out).resolve()
    repo = harness.parent
    projects = out / "projects"
    projects.mkdir(parents=True)
    driver = projects / "driver"
    ignore = shutil.ignore_patterns("__pycache__", "*.pyc")
    shutil.copytree(harness / "acceptance/driver", driver, ignore=ignore)
    shutil.copytree(repo / "python/seedbox", driver / "seedbox", ignore=ignore)
    shutil.copytree(repo / "examples/etcd/workload", driver / "etcd", ignore=ignore)
    shutil.copyfile(repo / "guest/race.c", driver / "race.c")
    (driver / ".dockerignore").write_text("__pycache__\n*.pyc\n")
    sources = {}
    for name in scenarios:
        directory = projects / name
        directory.mkdir()
        target = directory / "compose.yaml"
        shutil.copyfile(harness / cases["scenarios"][name], target)
        sources[name] = str(target)
    return sources
