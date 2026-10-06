"""Guest-owned etcd scenario: workload, process/network faults, then verification."""
import argparse
import json
import re
import subprocess
import tempfile
import time
import urllib.request

from seedbox import World


def ready(endpoints):
    deadline = time.monotonic() + 20
    for endpoint in endpoints:
        while True:
            try:
                with urllib.request.urlopen(f"http://{endpoint}/health", timeout=1) as response:
                    if json.load(response).get("health") not in (True, "true"):
                        raise OSError("etcd is not healthy")
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"etcd readiness timed out: {endpoint}")
                time.sleep(0.1)


def inject(world, members, rounds, workload):
    """Exercise confirmed process effects and a peer partition during live load."""
    for index in range(rounds):
        world.sleep(1)
        if workload.poll() is not None:
            raise RuntimeError("workload exited before the fault phase completed")
        target = members[index % len(members)]
        world.stop(target)
        world.sleep(0.3)
        world.restart(target)
        cut = world.partition([target], [member for member in members if member != target])
        if world.inspect()["active_handles"] != [cut.handle]:
            raise RuntimeError("partition did not become active")
        world.sleep(0.5)
        world.heal(cut)
        if world.inspect()["active_handles"]:
            raise RuntimeError("partition remained active after healing")


def verify(text, code, planted):
    lines = re.findall(r"^RESULT (pass|fail) acked=(\d+) .+$", text, re.M)
    if len(lines) != 1 or code != (0 if lines[0][0] == "pass" else 1):
        raise RuntimeError("missing or contradictory workload verification result")
    if int(lines[0][1]) <= int(planted):
        raise RuntimeError("workload acknowledged no real writes")
    return code


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--endpoints", default="n1:2379,n2:2379,n3:2379")
    parser.add_argument("--duration", default="10s")
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--plant", choices=["missing"])
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("at least one fault round is required")
    endpoints = args.endpoints.split(",")
    ready(endpoints)
    command = ["/client", "--endpoints", args.endpoints, "--duration", args.duration,
               "--concurrency", "8"]
    if args.plant:
        command += ["--plant", args.plant]
    with tempfile.TemporaryFile(mode="w+") as output:
        with subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT) as workload:
            try:
                with World.connect() as world:
                    inject(world, [ep.split(":")[0] for ep in endpoints], args.rounds, workload)
                code = workload.wait(timeout=90)
            finally:
                if workload.poll() is None:
                    workload.terminate()
                    workload.wait()
            output.seek(0)
            text = output.read()
        print(text, end="", flush=True)
        result = verify(text, code, bool(args.plant))
        print(f"SCENARIO verified rounds={args.rounds}", flush=True)
        return result


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"SCENARIO error {error}", flush=True)
        raise SystemExit(2)
