#!/usr/bin/env python3
"""Check retained ic-dependencies/observer output; never launch a workload."""
import re
import sys
from collections import defaultdict
from pathlib import Path


def check(text: str) -> dict[str, int]:
    if any(error in text for error in ("Internal Error", "Segmentation fault", "timeout:")):
        raise ValueError("execution failure in evidence")
    if text.count("ic-dependencies: checks=18 failures=0") != 1:
        raise ValueError("missing successful application checks")
    phases = [int(value) for value in re.findall(r"^IC_PHASE (\d+)$", text, re.M)]
    if phases != list(range(7)):
        raise ValueError("missing or repeated publication phase")
    emitted: dict[int, list[int]] = defaultdict(list)
    deleted: dict[int, list[int]] = defaultdict(list)
    for phase, offset, reconstruct, trace in re.findall(
        r"^IC_BLOCK phase=(\d+) offset=(\d+) reconstruct=([01]) trace=([01])$", text, re.M
    ):
        if trace != "0":
            raise ValueError("unexpected trace")
        if reconstruct == "0":
            emitted[int(phase)].append(int(offset))
    for phase, offset in re.findall(r"^IC_DELETE phase=(\d+) offset=(\d+)$", text, re.M):
        deleted[int(phase)].append(int(offset))
    warm = sorted(emitted[0])
    if len(warm) != 5 or warm[:4] != [0, 4, 32, 128] or warm[4] < 4092:
        raise ValueError("missing real warm translations")
    cross_page = warm[4]
    expected = {1: ([16], []), 2: ([0, 4], [0, 4]),
                3: ([128], [128]), 4: ([cross_page], [cross_page])}
    for phase, (new, old) in expected.items():
        if sorted(emitted[phase]) != new or sorted(deleted[phase]) != old:
            raise ValueError(f"phase {phase}: incorrect dependency selection or neighbor reuse")
    if sorted(emitted[5]) != [32] or sorted(deleted[5]) != sorted(warm + [16]):
        raise ValueError("permission incarnation did not conservatively invalidate")
    records = re.findall(r"^seedbox-ic-images: (.+)$", text, re.M)
    if len(records) != 1:
        raise ValueError("missing source ownership counters")
    counters = {key: int(value) for key, value in re.findall(r"(\w+)=(\d+)", records[0])}
    for key in ("ic", "admissions", "scanned_bytes", "scan_us", "emitted", "selected", "reclaimed"):
        if key not in counters or counters[key] <= 0:
            raise ValueError(f"missing positive {key}")
    if counters["emitted"] != counters["reclaimed"]:
        raise ValueError("unreclaimed generation metadata")
    return counters


if __name__ == "__main__":
    try:
        result = check(Path(sys.argv[1]).read_text())
        print("ic-dependencies evidence: PASS", result)
    except (IndexError, OSError, ValueError) as error:
        sys.exit(str(error))
