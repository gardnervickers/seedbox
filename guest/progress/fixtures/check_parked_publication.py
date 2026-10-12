"""Strict completed-run framing for the parked-publication discriminator."""
import argparse
import re
from pathlib import Path


def check(text, returncode):
    if returncode != 0:
        raise ValueError(f"process did not exit successfully: {returncode}")
    if re.search(r"FAIL|BAD |Internal Error|usage error|Segmentation fault|timed out", text):
        raise ValueError("failure diagnostic in output")
    case = "progress" if "BEGIN case=progress " in text else "restartable"
    begins = re.findall(r"^BEGIN case=" + case + r" iterations=20 target=(0x[0-9a-f]+) "
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
            m = re.fullmatch(r"ITERATION case=" + case + r" iteration=(\d+) faults=1 signals=1 "
                             r"bad=0 body=(\d+) all_gpr=31 all_q=32 state=valid", line)
            if not m or int(m[2]) != 0x600 + int(m[1]):
                raise ValueError("missing fault, signal, full state or current body")
            events.append(("iteration", int(m[1])))
    expected = [(kind, i) for i in range(1, 21) for kind in ("publication", "iteration")]
    if events != expected:
        raise ValueError("incomplete, duplicated or reordered iterations")
    if text.count(f"END case={case} completed=20 faults=20 signals=20 bad=0") != 1:
        raise ValueError("missing complete end frame")
    if case == "progress":
        ledger = re.findall(r"^LEDGER iteration=(\d+) old_charge=(\d+) fresh_charge=(\d+) "
                            r"initial=(-?\d+) after_add=(-?\d+) payments=(\d+) "
                            r"before_signal=(-?\d+) released=(\d+) deferred=(\d+) "
                            r"fresh_before=(-?\d+) fresh_after=(-?\d+)$", text, re.M)
        expected_ledger = []
        for i in range(1, 21):
            initial = 2 if i % 2 else -99998
            payments = 1 if i % 2 else 3
            expected_ledger.append(tuple(map(str, (i, 5, 7, initial, initial - 5 + 50000,
                                                   payments, 50002, 1, 1))))
        if [row[:9] for row in ledger] != expected_ledger or any(
                int(row[10]) != int(row[9]) - 7 for row in ledger):
            raise ValueError("incorrect old-debt/payment/refund/current-charge ledger")
    records = re.findall(r"seedbox-ic-images: .* emitted=(\d+) selected=\d+ reclaimed=(\d+)", text)
    if records and (len(records) != 1 or records[0][0] != records[0][1]):
        raise ValueError("unreclaimed image generations")
    return {"case": case, "completed": 20, "faults": 20, "signals": 20, "bad": 0,
            "publication_latency_us": latency, "image_records": records}


def check_application_return(text, returncode):
    if returncode != 0 or re.search(r"FAIL|BAD |Internal Error|usage error", text):
        raise ValueError("application-return process failure")
    begin = re.findall(r"^BEGIN case=application-return iterations=20 outer=(0x[0-9a-f]+) "
                       r"poll=(0x[0-9a-f]+) read=(0x[0-9a-f]+) edit=(0x[0-9a-f]+) "
                       r"x28=(0x[0-9a-f]+)$", text, re.M)
    if len(begin) != 1:
        raise ValueError("missing return frame")
    outer, poll, read, edit, x28 = map(lambda v: int(v, 16), begin[0])
    if not x28 or (poll, read, edit) != (outer + 8, outer + 128, outer + 64):
        raise ValueError("incorrect application PC framing")
    events, latency, pcs = [], [], []
    for line in text.splitlines():
        if line.startswith("KERNEL_READ "):
            m = re.fullmatch(r"KERNEL_READ iteration=(\d+) syscall=63 pc=(0x[0-9a-f]+) "
                             r"sp=(0x[0-9a-f]+) fd=(\d+)", line)
            if not m: raise ValueError("not an observed kernel read")
            events.append(("read", int(m[1])))
            pcs.append(int(m[2], 16))
        elif line.startswith("RETURN_PUBLICATION "):
            m = re.fullmatch(r"RETURN_PUBLICATION iteration=(\d+) outer_pc=(0x[0-9a-f]+) "
                             r"edited_pc=(0x[0-9a-f]+) read_pc=(0x[0-9a-f]+) "
                             r"read_effects=0 done=(\d+) latency_us=(\d+)", line)
            if not m or m[1] != m[5] or tuple(int(m[n], 16) for n in (2, 3, 4)) != (poll, edit, read + 4):
                raise ValueError("publication did not precede read release")
            events.append(("publication", int(m[1])))
            latency.append(int(m[6]))
        elif line.startswith("ITERATION "):
            m = re.fullmatch(r"ITERATION case=application-return iteration=(\d+) faults=1 "
                             r"nested=1 reads=1 effects=1 edited=(\d+) outer_body=(\d+) "
                             r"read_body=(\d+) all_gpr=31 all_q=32 bad=0", line)
            if not m: raise ValueError("missing return context/effect evidence")
            i = int(m[1])
            if (int(m[2]), int(m[3]), int(m[4])) != (int(i % 2 == 0), (0x600 if i % 2 else 0x800) + i, 0x700 + i):
                raise ValueError("stale body or incorrect PC edit")
            events.append(("iteration", i))
    expected = [(kind, i) for i in range(1, 21) for kind in ("read", "read", "publication", "iteration")]
    if events != expected or len(set(pcs)) != 1:
        raise ValueError("missing/reordered read, nested restart, publication or return")
    if text.count("END case=application-return completed=20 faults=20 nested=20 reads=20 edits=10 bad=0") != 1:
        raise ValueError("missing complete return frame")
    records = re.findall(r"seedbox-ic-images: .* emitted=(\d+) selected=\d+ reclaimed=(\d+)", text)
    if records and (len(records) != 1 or records[0][0] != records[0][1]):
        raise ValueError("unreclaimed return generations")
    return {"case": "application-return", "completed": 20, "faults": 20, "nested": 20,
            "reads": 20, "edits": 10, "bad": 0, "kernel_read_pc": pcs[0],
            "publication_latency_us": latency, "image_records": records}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("--returncode", required=True, type=int)
    args = parser.parse_args()
    text = args.log.read_text()
    validator = check_application_return if "BEGIN case=application-return " in text else check
    print(validator(text, args.returncode))
