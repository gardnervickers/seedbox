#!/usr/bin/env python3
"""Accept only one fresh, successfully completed VMM attempt's client result."""
import json
import math
import pathlib
import sys


def completed_result(path, seed):
    stem = path.with_suffix("")
    if pathlib.Path(str(stem) + ".status").read_text().strip() != "0":
        raise ValueError("attempt did not exit successfully")
    report = json.loads(path.read_text())
    if type(report.get("seed")) is not int or report["seed"] != seed:
        raise ValueError("result belongs to a different seed")
    if report.get("outcome") != "end_machine_exited":
        raise ValueError("missing terminal outcome")
    clients = [machine for machine in report.get("machines", []) if machine.get("name") == "client"]
    if len(clients) != 1 or clients[0].get("state") not in {"Exited(0)", "Exited(1)"}:
        raise ValueError("client did not exit with checker status")
    host = report.get("host_seconds")
    if type(host) not in (int, float) or not math.isfinite(host) or host < 0:
        raise ValueError("missing valid completion timing")
    lines = (stem / "client.console").read_text().splitlines()
    results = [line for line in lines if line.split()[:2] in (["RESULT", "pass"], ["RESULT", "fail"])]
    if len(results) != 1:
        raise ValueError("expected one client RESULT")
    expected = "Exited(0)" if results[0].split()[1] == "pass" else "Exited(1)"
    if clients[0]["state"] != expected:
        raise ValueError("client RESULT contradicts checker exit status")
    return f"{round(host)} {results[0]}"


if __name__ == "__main__":
    try:
        print(completed_result(pathlib.Path(sys.argv[1]), int(sys.argv[2])))
    except (OSError, ValueError, TypeError, AttributeError) as error:
        sys.exit(f"incomplete attempt: {error}")
