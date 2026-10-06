"""Prepare YAML workloads using the Rust API's test-only OCI bridge.

Python still owns scheduling, faults and assertions. No new production command,
YAML parser or content-store implementation is needed here.
"""
import json
import os
from pathlib import Path
import subprocess


def apply_faults(world, schedule):
    """Keep the existing engine fault schedule outside application authoring."""
    if schedule is None:
        return world
    members = {m["name"] for m in world["machines"]}
    if not schedule["members"] or not set(schedule["members"]) <= members:
        raise ValueError("fault schedule targets an unknown YAML service")
    if not 0 <= schedule["start_ns"] < schedule["end_ns"] <= world["duration_ns"]:
        raise ValueError("fault schedule is outside YAML execution limits")
    for field in ("gap_ns", "down_ns"):
        low, high = schedule[field]
        if not 0 <= low <= high or (field == "gap_ns" and low == 0):
            raise ValueError(f"invalid fault {field}")
    return {**world, "kill_restart": schedule}


def prepare_workloads(sim, cases, scenarios, out, args):
    sim, out = Path(sim).resolve(), Path(out).resolve()
    kernel = Path(args.kernel).resolve() if args.kernel else (
        Path(args.artifacts).resolve() if args.artifacts else Path(args.root).resolve() / "artifacts"
    ) / "bzImage"
    busybox = Path(os.environ["SEEDBOX_BUSYBOX"])
    for path in (kernel, busybox, Path(args.vmm)):
        if not path.is_file():
            raise FileNotFoundError(path)
    out.mkdir(parents=True, exist_ok=True)
    setup = out / "setup"
    setup.mkdir()
    print(f"preparing acceptance workloads from YAML; evidence in {out}", flush=True)
    built = subprocess.run([
        "cargo", "test", "--manifest-path", str(sim.parent / "crates/seedbox/Cargo.toml"), "--locked",
        "--test", "acceptance_prepare", "--no-run", "--message-format=json",
    ], text=True, capture_output=True)
    (setup / "build.log").write_text(built.stderr)
    if built.returncode:
        raise RuntimeError(f"preparation helper build failed; see {setup / 'build.log'}")
    binaries = [m["executable"] for line in built.stdout.splitlines()
                if (m := json.loads(line)).get("reason") == "compiler-artifact"
                and m.get("target", {}).get("name") == "acceptance_prepare" and m.get("executable")]
    if len(binaries) != 1:
        raise RuntimeError("Cargo did not return one acceptance preparation test binary")
    guest_init = setup / "compose-init"
    subprocess.run(["bash", str(sim.parent / "guest/build-compose.sh"), str(guest_init)], check=True)
    request = setup / "request.json"
    request.write_text(json.dumps({
        "workloads": {name: str(sim / cases["scenarios"][name]) for name in scenarios},
        "kernel": str(kernel), "vmm": str(Path(args.vmm).resolve()),
        "guest_init": str(guest_init), "busybox": str(busybox), "output": str(out / "prepared"),
    }, indent=2) + "\n")
    with (setup / "prepare.log").open("w") as log:
        completed = subprocess.run([
            "sudo", "-n", "env", f"PATH={os.environ['PATH']}", f"SEEDBOX_ACCEPTANCE_REQUEST={request}",
            binaries[0], "--ignored", "--exact", "prepare_acceptance_workloads", "--nocapture",
        ], stdout=log, stderr=subprocess.STDOUT)
    if completed.returncode:
        raise RuntimeError(f"YAML workload preparation failed; see {setup / 'prepare.log'}")
    worlds = out / "workloads"
    worlds.mkdir()
    sources = {}
    for name in scenarios:
        world = json.loads((out / "prepared" / name / "world.json").read_text())
        world = apply_faults(world, cases.get("faults", {}).get(name))
        path = worlds / f"{name}.json"
        path.write_text(json.dumps(world, indent=2) + "\n")
        sources[name] = str(path)
    return sources
