"""Portable tier: host-independent tests that never start a VM."""
from pathlib import Path
import subprocess
import sys

from .checks import FAIL, PASS, SKIPPED, Verdict

LINUX = sys.platform.startswith("linux")


def steps(repo):
    sim = repo / "sim"
    scripts = sorted(str(p) for p in sim.rglob("*.sh") if "target" not in p.parts)
    # batch.py's lifecycle tests rely on Linux process-group/waitid semantics.
    tests = ["test_campaign", "test_msr54", "test_acceptance"] + (["test_batch"] if LINUX else [])
    yield "bash -n", [["bash", "-n", s] for s in scripts], repo, None
    yield "runner unit tests", [[sys.executable, "-m", "unittest", "-q", *tests]], sim / "tests", None
    yield ("VMM cargo test", [["cargo", "test", "--release", "--manifest-path", "vmm/Cargo.toml"]],
           sim, None if LINUX else "kvm-ioctls builds only on Linux")
    crate = sim / "seedbox" / "Cargo.toml"
    yield ("seedbox crate tests", [["cargo", "test", "--locked", "--manifest-path", str(crate)]],
           sim, None if crate.exists() else "sim/seedbox not present on this branch")


def portable(repo, log=print):
    outcomes = []
    for name, commands, cwd, skip in steps(Path(repo)):
        if skip:
            outcomes.append((name, Verdict(SKIPPED, skip)))
            continue
        log(f"portable: {name}")
        failed = []
        for argv in commands:
            proc = subprocess.run(argv, cwd=cwd, capture_output=True, text=True)
            if proc.returncode:
                failed.append(f"{' '.join(argv[-2:])}: {(proc.stderr or proc.stdout).strip()[-800:]}")
        if failed:
            outcomes.append((name, Verdict(FAIL, "\n".join(failed))))
        else:
            outcomes.append((name, Verdict(PASS, f"{len(commands)} command(s) passed")))
    return outcomes
