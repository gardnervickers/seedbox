#!/usr/bin/env python3
"""Bounded low-level kernel-writer discriminator, not application acceptance."""
import argparse
import json
import os
from pathlib import Path
import re
import selectors
import signal
import subprocess
import time

READY = re.compile(r"ic-kernel-write-ready: pid=(\d+) code=(0x[0-9a-f]+) done=(0x[0-9a-f]+)\n")
RESULT = re.compile(r"ic-kernel-write: before=(\d+) after=(\d+) bytes=(0x[0-9a-f]+) failures=(\d+)\n")


def classify(stdout, stderr, returncode, timed_out):
    rows = RESULT.findall(stdout)
    if timed_out:
        return "timeout"
    if any(word in stderr.lower() for word in ("internal error", "segmentation fault", "sigsegv", "fatal")):
        return "fatal"
    if len(rows) != 1:
        return "invalid-output"
    before, after, data, failures = (int(value, 0) for value in rows[0])
    if (before, after, data, failures, returncode) == (1, 2, 0x52800040, 0, 0):
        return "pass"
    if (before, after, data, failures, returncode) == (1, 1, 0x52800040, 1, 1):
        return "stale-translation"
    return "unexpected-result"


def run(command, external, directory):
    started = time.monotonic()
    deadline = started + 20
    stdout = bytearray()
    timed_out = False
    observation = None
    error = None
    with (directory / "stderr.txt").open("wb") as stderr_file:
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=stderr_file,
                                   start_new_session=True)
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        try:
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    timed_out = True
                    break
                for key, _ in selector.select(min(remaining, 0.1)):
                    data = os.read(key.fd, 65536)
                    if not data:
                        selector.unregister(key.fileobj)
                        continue
                    stdout.extend(data)
                    match = READY.search(stdout.decode(errors="replace"))
                    if external and observation is None and match:
                        pid, code, done = (int(value, 0) for value in match.groups())
                        if pid != process.pid:
                            raise ValueError("reported pid is not the owned child")
                        with open(f"/proc/{pid}/mem", "r+b", buffering=0) as memory:
                            old = os.pread(memory.fileno(), 4, code)
                            if old != (0x52800020).to_bytes(4, "little"):
                                raise ValueError("unexpected original instruction")
                            written = os.pwrite(memory.fileno(), (0x52800040).to_bytes(4, "little"), code)
                            current = os.pread(memory.fileno(), 4, code)
                            if written != 4 or current != (0x52800040).to_bytes(4, "little"):
                                raise ValueError("external code write did not complete")
                            if os.pwrite(memory.fileno(), (1).to_bytes(4, "little"), done) != 4:
                                raise ValueError("external acknowledgement write did not complete")
                            observation = {"pid": pid, "code": code, "done": done,
                                           "old": old.hex(), "current": current.hex(),
                                           "written": written}
            if not timed_out:
                process.wait(timeout=max(0.01, deadline - time.monotonic()))
        except subprocess.TimeoutExpired:
            timed_out = True
        except (OSError, ValueError) as exc:
            error = str(exc)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=2)
            selector.close()
            process.stdout.close()
    text = stdout.decode(errors="replace")
    (directory / "stdout.txt").write_text(text)
    stderr = (directory / "stderr.txt").read_text(errors="replace")
    status = classify(text, stderr, process.returncode, timed_out)
    if error or (external and observation is None):
        status = "observer-error"
    return {"command": command, "returncode": process.returncode, "timed_out": timed_out,
            "elapsed_seconds": time.monotonic() - started, "status": status,
            "external_write": observation, "observer_error": error}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--drrun", required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--progress-client", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    results = {}
    for mode in ("native", "off", "on"):
        for external in (False, True):
            name = f"{mode}-{'external' if external else 'self'}"
            directory = args.output / name
            directory.mkdir()
            command = [args.fixture]
            if external:
                command.append("--external")
            if mode != "native":
                prefix = [args.drrun, "-debug", "-opt_speed", "-no_ldstex2cas",
                          "-no_hw_cache_consistency"]
                if mode == "on":
                    prefix.append("-seedbox_icache_images")
                command = prefix + ["-c", args.progress_client,
                                    "--instructions-per-progress-call", "50000",
                                    "--progress-points-per-block", "1", "--"] + command
            results[name] = run(command, external, directory)
            print(name, results[name]["status"], flush=True)
    (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    return 0 if all(result["status"] == "pass" for result in results.values()) else 1


if __name__ == "__main__":
    raise SystemExit(main())
