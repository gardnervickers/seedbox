#!/usr/bin/env python3
"""Revalidate retained optimized-matrix raw outputs, ordering, clocks and gates."""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import statistics

spec = importlib.util.spec_from_file_location("optimized", Path(__file__).with_name("measure-optimized.py"))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)
p = argparse.ArgumentParser(description=__doc__)
p.add_argument("root", type=Path)
a = p.parse_args()
evidence = a.root / "container-evidence"
primary = evidence / "primary"
plan = json.loads((primary / "plan.json").read_text())
assert plan["orders"] == m.ORDERS and plan["batches"] == m.BATCHES
samples = [json.loads(line) for line in (primary / "samples.jsonl").read_text().splitlines()]
expected = [(rep, row, mode, batch) for rep, order in enumerate(m.ORDERS)
            for row, batches in m.BATCHES.items() for mode in order for batch in range(batches)]
assert len(samples) == len(expected)
last_end = 0
for sample, key in zip(samples, expected):
    assert tuple(sample[k] for k in ["repeat", "row", "mode", "batch"]) == key
    assert sample["exit"] == 0 and "failure" not in sample
    assert sample["started_unix"] >= last_end - .002
    last_end = sample["started_unix"] + sample["process_ns"] / 1e9
    mode = "candidate" if sample["mode"] == "baseline" else sample["mode"]
    check = m.jit.validate(sample["output"], sample["row"], mode) if sample["row"] in m.jit.ROWS else m.validate_warm(sample["output"], sample["row"], mode)
    assert check == sample["metrics"]
    command = sample["command"]
    binary = "/candidate" if sample["mode"] == "candidate" else "/baseline-common" if sample["row"] == "call17" else "/baseline"
    assert command[-2] == binary
    assert ("/dr/libseedbox_progress.so" in command) == (sample["mode"] == "dr")
elapsed = last_end - samples[0]["started_unix"]
assert elapsed <= plan["cap_seconds"]
result = {"samples": len(samples), "elapsed_seconds": elapsed, "rows": {}}
for row in m.BATCHES:
    result["rows"][row] = {}
    for mode in m.ORDERS[0]:
        reps = []
        for repeat in range(5):
            metrics = [s["metrics"] for s in samples if s["row"] == row and s["mode"] == mode and s["repeat"] == repeat]
            reps.append({k: sum(v[k] for v in metrics) for k in metrics[0] if isinstance(metrics[0][k], int)})
        clock = "total_ns" if row in m.jit.ROWS else "warm_ns"
        result["rows"][row][mode] = {"repetitions": reps, "clock": clock,
            "median_ns": statistics.median(r[clock] for r in reps),
            "min_ns": min(r[clock] for r in reps), "max_ns": max(r[clock] for r in reps)}
for row in m.jit.ROWS:
    if row.startswith("zero"):
        continue
    control = "zero-high" if row.endswith("high") else "zero-medium" if row.endswith("medium") else "zero"
    delta = {mode: result["rows"][row][mode]["median_ns"] - result["rows"][control][mode]["median_ns"] for mode in m.ORDERS[0]}
    result["rows"][row]["publication_delta_ns"] = delta
    result["rows"][row]["dr_minus_candidate_ns_per_publication"] = (delta["dr"] - delta["candidate"]) / (64 * m.BATCHES[row])
for mode in ["default", "zero", "negative", "overshoot", "scratch", "scratch-write", "fault16", "fault17", "fault-base16", "fault-base17", "indirect16", "indirect17", "return16", "return17"]:
    text = (evidence / f"final-{mode}.log").read_text()
    assert "SEEDBOX_INIT_PASS" in text and "EXECUTOR_FAIL" not in text
    assert "old-pin=reclaimed" in text and "raw-paid-old-map-readable" in text
    for stage in ["Debited", "Returned", "BudgetCommitted", "PaymentCommitted", "Refunded"]:
        assert f"stage={stage}" in text
    assert text.count("EXECUTOR_PV_ENTER\n") == 3
    assert text.count("EXECUTOR_PV_RETURN\n") == 3
for mode in ["gateway-fault", "boundary-x0-fault", "return-unknown"]:
    text = (evidence / f"final-{mode}.log").read_text()
    assert "EXECUTOR_FAIL" in text and "SEEDBOX_INIT_PASS" not in text
    if mode == "return-unknown":
        assert "EXECUTOR_PV_ENTER" not in text
hvf = json.loads((a.root / "candidate-hvf.json").read_text())
assert hvf["powered_off"] and "SEEDBOX_INIT_PASS" in hvf["console"]
assert len(hvf["marked_calls"]) == 3 and len(hvf["unmarked_calls"]) == 1
assert all(c["elapsed_ns"] >= 50000 and c["vmm_horizon_returns"] >= 4 for c in hvf["marked_calls"])
assert hvf["unmarked_calls"][0]["elapsed_ns"] == 2000
result["hvf"] = {k: hvf[k] for k in ["marked_calls", "unmarked_calls"]}
# Distinguish actual DR block emission from reconstruction, using the separate
# corrected diagnostic client; no diagnostic build is a timing denominator.
text = (evidence / "dr-call17.out").read_text()
base = int(re.search(r"base=(0x[0-9a-f]+)", text)[1], 16)
plans = []
for line in (evidence / "dr-call17.plans").read_text().splitlines():
    found = re.fullmatch(r"REF_PLAN (0x[0-9a-f]+) (\d+) translating=(\d) trace=0", line)
    if found and base <= int(found[1], 16) < base + 4096:
        plans.append((int(found[1], 16) - base, int(found[2]), int(found[3])))
assert {(offset, count) for offset, count, translating in plans if not translating} == {(0, 1), (4, 2), (64, 14), (12, 18)}
result["dr_call17_plans"] = plans
# Every published version and independent neighbor call count is checked in
# separate verbose candidate diagnostics. DR accounting uses NONlogging fixture
# arguments and a different client, never those verbose version-print runs.
result["diagnostics"] = {}
for row in m.jit.ROWS:
    text = (evidence / f"versions-{row}.out").read_text()
    versions = [dict(item.split("=", 1) for item in line.split()[1:]) for line in text.splitlines() if line.startswith("JIT_VERSION ")]
    assert len(versions) == 256 and "EXECUTOR_FAIL" not in text
    n = 32 if row.endswith("high") else 512 if row.endswith("medium") else 8192
    for i, version in enumerate(versions):
        round_, phase = divmod(i, 4)
        count = 1 if phase % 2 == 0 else n-1
        value = 1 if phase >= 2 or row.startswith("zero") else round_+2
        assert {k:int(version[k]) for k in ["round","phase","version","calls","checksum"]} == dict(round=round_,phase=phase,version=value,calls=count,checksum=count*(value+11))
    base = int(re.search(r"base=(0x[0-9a-f]+)", (evidence / f"accounting-dr-{row}.out").read_text())[1], 16)
    phases = []
    emissions = 0
    neighbor_emissions = 0
    neighbor_deletions = 0
    reconstructs = 0
    active = False
    for line in (evidence / f"accounting-dr-{row}.plans").read_text().splitlines():
        phase = re.fullmatch(r"REF_PHASE (begin|end) payments=(\d+) left=(-?\d+)", line)
        if phase:
            phases.append((phase[1],int(phase[2]),int(phase[3])))
            active = phase[1] == "begin"
        if not active:
            continue
        plan = re.fullmatch(r"REF_PLAN (0x[0-9a-f]+) (\d+) translating=(\d) trace=0", line)
        if plan and base <= int(plan[1],16) < base+4096:
            if int(plan[3]): reconstructs += 1
            else:
                emissions += 1
                neighbor_emissions += int(int(plan[1],16) == base+512)
        deleted = re.fullmatch(r"REF_DELETE (0x[0-9a-f]+)",line)
        if deleted and int(deleted[1],16) == base+512: neighbor_deletions += 1
    assert len(phases) == 2 and phases[0][0] == "begin" and phases[1][0] == "end"
    assert emissions == (0 if row.startswith("zero") else 320)
    assert neighbor_emissions == neighbor_deletions == (0 if row.startswith("zero") else 64)
    assert reconstructs == 256
    payments = phases[1][1] - phases[0][1]
    result["diagnostics"][row] = dict(emissions=emissions, neighbor_emissions=neighbor_emissions,
        neighbor_deletions=neighbor_deletions,reconstructions=reconstructs,payments=payments,
        left_begin=phases[0][2],left_end=phases[1][2],logical=payments*50000+phases[0][2]-phases[1][2])
result["discriminators"] = {}
for name in ["region-short", "raw-v1-short"]:
    rows = [json.loads(line) for line in (evidence/name/"samples.jsonl").read_text().splitlines()]
    assert len(rows) == 320
    for row in rows:
        assert row["exit"] == 0 and "failure" not in row
        checked = "candidate" if row["mode"] == "baseline" else row["mode"]
        assert m.jit.validate_short(row["output"], checked) == row["metrics"]
    result["discriminators"][name] = len(rows)
(a.root / "analysis.json").write_text(json.dumps(result, indent=2) + "\n")
print(f"PASS: {len(samples)} raw samples, 5 rotations, 13 rows, {elapsed:.3f}s; major gates and actual HVF; DR call17 plans1+14+2/fault18")
