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
    if report.get("outcome") not in {"duration", "quiescent", "end_machine_exited"}:
        raise ValueError("missing terminal outcome")
    host = report.get("host_seconds")
    if type(host) not in (int, float) or not math.isfinite(host) or host < 0:
        raise ValueError("missing valid completion timing")
    lines = (stem / "client.console").read_text().splitlines()
    results = [line for line in lines if line.startswith(("RESULT pass", "RESULT fail"))]
    if len(results) != 1:
        raise ValueError("expected one client RESULT")
    return f"{round(host)} {results[0]}"


if __name__ == "__main__":
    try:
        print(completed_result(pathlib.Path(sys.argv[1]), int(sys.argv[2])))
    except (OSError, ValueError, TypeError, AttributeError) as error:
        sys.exit(f"incomplete attempt: {error}")
