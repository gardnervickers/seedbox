"""Five sequential rotated fixed-work samples; no build or simultaneous load."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time


def deadline(*_):
    raise TimeoutError("fixed-work sample exceeded 30 seconds")


def sample(command, path):
    started = time.monotonic()
    timed_out = False
    with path.open("w") as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        old = signal.signal(signal.SIGALRM, deadline)
        signal.alarm(30)
        try:
            _, status, usage = os.wait4(process.pid, 0)
        except TimeoutError:
            timed_out = True
            os.killpg(process.pid, signal.SIGKILL)
            _, status, usage = os.wait4(process.pid, 0)
        finally:
            signal.alarm(0)
            signal.signal(signal.SIGALRM, old)
        process.returncode = os.waitstatus_to_exitcode(status)
    elapsed = time.monotonic() - started
    text = path.read_text()
    rows = re.findall(r"ic-throughput: mode=(append|patch) publications=(\d+) calls=(\d+) checksum=(\d+) expected=(\d+)", text)
    valid = (not timed_out and process.returncode == 0 and len(rows) == 1 and
             not any(error in text for error in ("Internal Error", "Segmentation fault", "fatal")))
    if valid:
        _, count, calls, checksum, expected = rows[0]
        valid = (int(count) == 2000 and int(calls) == 512000 and
                 int(checksum) == int(expected) == 275840000)
    return {"command": command, "returncode": process.returncode, "timeout": timed_out,
            "elapsed_seconds": elapsed, "max_rss_kib": usage.ru_maxrss,
            "valid": valid, "log": str(path)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--drrun", required=True)
    parser.add_argument("--client", required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    cases = [(work, mode) for work in ("append", "patch") for mode in ("off", "on")]
    results = []
    for rotation in range(5):
        for work, mode in cases[rotation % 4:] + cases[:rotation % 4]:
            command = [args.drrun, "-opt_speed", "-disable_traces", "-no_ldstex2cas",
                       "-no_hw_cache_consistency", "-fake_ctr_dic"]
            if mode == "on":
                command.append("-seedbox_icache_images")
            command += ["-c", args.client, "--instructions-per-progress-call", "50000",
                        "--progress-points-per-block", "1", "--", args.fixture, work, "2000"]
            result = sample(command, args.output / f"{rotation}-{work}-{mode}.log")
            result.update(rotation=rotation, work=work, mode=mode)
            results.append(result)
            print(rotation, work, mode, result["valid"], result["elapsed_seconds"], flush=True)
    (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    return 0 if all(item["valid"] for item in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
