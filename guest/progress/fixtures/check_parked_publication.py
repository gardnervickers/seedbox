"""Strict completed-run framing for the parked-publication discriminator."""
import argparse
import re
from pathlib import Path


def check(text, returncode):
    if returncode != 0:
        raise ValueError(f"process did not exit successfully: {returncode}")
    if re.search(r"FAIL|BAD |Internal Error|usage error|Segmentation fault|timed out", text):
        raise ValueError("failure diagnostic in output")
    begins = re.findall(r"^BEGIN case=restartable iterations=20 target=(0x[0-9a-f]+) "
                        r"poll=(0x[0-9a-f]+) x28=(0x[0-9a-f]+)$", text, re.M)
    if len(begins) != 1 or int(begins[0][1], 16) != int(begins[0][0], 16) + 8:
        raise ValueError("missing or invalid target framing")
    events = []
    latency = []
    for line in text.splitlines():
        if line.startswith("PUBLICATION "):
            m = re.fullmatch(r"PUBLICATION iteration=(\d+) req=1 ack=1 done=(\d+) latency_us=(\d+)", line)
            if not m or m[1] != m[2]:
                raise ValueError("publication without held request/ack/done")
            events.append(("publication", int(m[1])))
            latency.append(int(m[3]))
        elif line.startswith("ITERATION "):
            m = re.fullmatch(r"ITERATION case=restartable iteration=(\d+) faults=1 signals=1 "
                             r"bad=0 body=(\d+) all_gpr=31 all_q=32 state=valid", line)
            if not m or int(m[2]) != 0x600 + int(m[1]):
                raise ValueError("missing fault, signal, full state or current body")
            events.append(("iteration", int(m[1])))
    expected = [(kind, i) for i in range(1, 21) for kind in ("publication", "iteration")]
    if events != expected:
        raise ValueError("incomplete, duplicated or reordered iterations")
    if text.count("END case=restartable completed=20 faults=20 signals=20 bad=0") != 1:
        raise ValueError("missing complete end frame")
    records = re.findall(r"seedbox-ic-images: .* emitted=(\d+) selected=\d+ reclaimed=(\d+)", text)
    if records and (len(records) != 1 or records[0][0] != records[0][1]):
        raise ValueError("unreclaimed image generations")
    return {"completed": 20, "faults": 20, "signals": 20, "bad": 0,
            "publication_latency_us": latency, "image_records": records}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("--returncode", required=True, type=int)
    args = parser.parse_args()
    print(check(args.log.read_text(), args.returncode))
