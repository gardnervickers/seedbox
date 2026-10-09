"""Guest-owned pause/resume during live etcd writes, followed by ledger validation."""
from dataclasses import asdict
import json
import re
import subprocess
import time
import urllib.request
from seedbox import World

endpoints = ["n1:2379", "n2:2379", "n3:2379"]
deadline = time.monotonic() + 20
for endpoint in endpoints:
    while True:
        try:
            with urllib.request.urlopen("http://" + endpoint + "/health", timeout=1) as r:
                if json.load(r).get("health") not in (True, "true"):
                    raise OSError("unhealthy member")
            break
        except OSError:
            if time.monotonic() >= deadline:
                raise RuntimeError("readiness deadline: " + endpoint)
            time.sleep(0.1)

with World.connect() as world:
    workload = subprocess.Popen(["/client", "--endpoints", ",".join(endpoints),
                                 "--duration", "3s", "--concurrency", "8"],
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True)
    try:
        world.sleep(0.05)
        if workload.poll() is not None:
            raise RuntimeError("workload ended before pause")
        paused = world.pause("n1")
        print("PAUSED", json.dumps(asdict(paused), sort_keys=True), flush=True)
        world.sleep(0.25)
        resumed = world.resume("n1")
        print("RESUMED", json.dumps(asdict(resumed), sort_keys=True), flush=True)
        if resumed.effect_at_ns - paused.effect_at_ns < 250000000:
            raise RuntimeError("pause interval was too short")
        output, _ = workload.communicate(timeout=30)
    finally:
        if workload.poll() is None:
            workload.kill()
            workload.wait()
print(output, end="", flush=True)
results = re.findall(r"^RESULT pass acked=(\d+) .+$", output, re.M)
if workload.returncode != 0 or len(results) != 1 or int(results[0]) == 0:
    raise RuntimeError("workload failed to verify acknowledged writes")
print("PAUSE_CHECK pass", flush=True)
