#!/usr/bin/env python3
"""Prepare image inputs, instrumentation controls and a guest-owned pause scenario."""
import argparse
import json
from pathlib import Path
import shutil
import copy

p = argparse.ArgumentParser()
p.add_argument("--repo", type=Path, required=True)
p.add_argument("--runtime-context", type=Path, required=True)
p.add_argument("--writes", type=int, default=20, help="Acknowledged writes per each of eight client workers")
p.add_argument("--duration", default="60s", help="Guest workload deadline")
p.add_argument("destination", type=Path)
a = p.parse_args()
if a.writes < 1:
    p.error("--writes must be positive")
d = a.destination.resolve()
d.mkdir(parents=True, exist_ok=True)
if any(d.iterdir()):
    p.error("destination must be empty")
shutil.copy(Path(__file__).with_name("Dockerfile"), d / "Dockerfile")
shutil.copytree(a.runtime_context / "sdk", d / "sdk")
shutil.copy(a.runtime_context / "libbudget.so", d / "libbudget.so")
shutil.copytree(a.repo / "examples/etcd/workload", d / "client")
shutil.copy(Path(__file__).with_name("client.Dockerfile"), d / "client/Dockerfile")
shutil.copy(Path(__file__).with_name("Dockerfile.pause"), d / "Dockerfile.pause")
shutil.copy(Path(__file__).with_name("pause.py"), d / "pause.py")
shutil.copytree(a.repo / "python/seedbox", d / "seedbox", ignore=shutil.ignore_patterns("__pycache__"))
for name, procs, mode in [("native1", 1, "native"), ("native4", 4, "native"),
                           ("empty4", 4, "empty"), ("checkpoint4", 4, "checkpoint"),
                           ("progress4", 4, "progress"), ("optimized4", 4, "optimized")]:
    services = {}
    for n in ("n1", "n2", "n3"):
        entry = ["/usr/local/bin/etcd"]
        if mode != "native":
            entry = ["/opt/dynamorio/bin64/drrun"]
            if mode == "optimized":
                entry += ["-opt_speed", "-disable_traces"]
            if mode != "empty":
                entry += ["-c", "/experiment/libbudget.so"]
                if mode in ("progress", "optimized"):
                    entry += ["--progress"]
                if mode == "optimized":
                    entry += ["--fast"]
            entry += ["--", "/usr/local/bin/etcd"]
        services[n] = dict(build=".", entrypoint=entry,
            environment={"GOMAXPROCS": str(procs)}, command=[
                f"--name={n}", f"--data-dir=/data/{n}",
                "--listen-client-urls=http://0.0.0.0:2379",
                f"--advertise-client-urls=http://{n}:2379",
                "--listen-peer-urls=http://0.0.0.0:2380",
                f"--initial-advertise-peer-urls=http://{n}:2380",
                "--initial-cluster=n1=http://n1:2380,n2=http://n2:2380,n3=http://n3:2380",
                "--initial-cluster-state=new", "--logger=zap", "--log-level=error"])
    services["client"] = dict(build="./client", environment={"GOMAXPROCS": "1"},
        command=["--endpoints", "n1:2379,n2:2379,n3:2379", "--duration", a.duration,
                 "--concurrency", "8", "--writes", str(a.writes)])
    compose = {"name": "dbi-etcd-" + name, "services": services,
        "x-seedbox": {"version": 1, "seed": 1009, "wall_clock_epoch": 1791504000,
            "machine": {"memory_mb": 1024},
            "limits": {"simulated_time": "120s", "wall_time": "120s"},
            "network": {"latency": "1ms", "jitter": "200us"},
            "schedule": {"timer_jitter": "1ms", "stop_tolerance": "2ms"},
            "check": {"service": "client", "exit_code": 0}}}
    (d / (name + ".yaml")).write_text(json.dumps(compose, indent=2) + "\n")
    if name == "progress4":
        pause = copy.deepcopy(compose)
        pause["name"] = "dbi-etcd-pause4"
        pause["services"]["client"] = {
            "build": {"context": ".", "dockerfile": "Dockerfile.pause"},
            "environment": {"GOMAXPROCS": "1"}}
        pause["x-seedbox"]["control"] = {"service": "client"}
        (d / "pause4.yaml").write_text(json.dumps(pause, indent=2) + "\n")
