#!/usr/bin/env python3
"""Read-only independent revalidation/summary of retained matched JIT evidence.

Prints JSON to stdout; does not execute any binary, benchmark, container or VM.
"""
import argparse
import collections
import importlib.util
import json
from pathlib import Path
import re
import statistics

spec = importlib.util.spec_from_file_location("measure_jit", Path(__file__).with_name("measure-jit.py"))
measure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measure)


def matrix(directory):
    plan = json.loads((directory / "plan.json").read_text())
    samples = [json.loads(line) for line in (directory / "samples.jsonl").read_text().splitlines()]
    expected = [(row, mode, repeat, batch) for repeat, order in enumerate(plan["orders"])
                for row in plan["rows"] for mode in order for batch in range(plan["batches"])]
    assert len(samples) == len(expected)
    cells = collections.defaultdict(collections.Counter)
    previous_end = 0
    for sample, identity in zip(samples, expected):
        assert (sample["row"], sample["mode"], sample["repeat"], sample["batch"]) == identity
        assert sample["exit"] == 0 and "failure" not in sample
        assert sample["started_unix"] + 0.001 >= previous_end, "overlapping process samples"
        previous_end = sample["started_unix"] + sample["process_ns"] / 1e9
        mode = "candidate" if sample["mode"] == "baseline" else sample["mode"]
        metrics = measure.validate_short(sample["output"], mode) if sample["row"] == "short" else measure.validate(sample["output"], sample["row"], mode)
        assert metrics == sample["metrics"], "saved metrics disagree with raw output"
        for key, value in metrics.items():
            if isinstance(value, int):
                cells[identity[:3]][key] += value
    elapsed = previous_end - samples[0]["started_unix"]
    assert elapsed < plan["cap_seconds"]
    result = dict(samples=len(samples), elapsed_seconds=elapsed, rows={},
                  linux_load_range=[min(s["load_before"][0] for s in samples), max(s["load_after"][0] for s in samples)])
    for row in plan["rows"]:
        result["rows"][row] = {}
        for mode in plan["orders"][0]:
            keys = [k for k in cells[row, mode, 0] if k.endswith("_ns")]
            result["rows"][row][mode] = {
                k: {"repetitions_ms": [cells[row, mode, repeat][k] / 1e6 for repeat in range(5)],
                    "median_ms": statistics.median(cells[row, mode, repeat][k] / 1e6 for repeat in range(5))}
                for k in keys}
    return result


def diagnostics(directory, row):
    version_text = (directory / f"final-diagnostic-dr-{row}.out").read_text()
    # Observe the SAME nonlogging fixture flags used for timing. Per-version
    # print diagnostics execute extra DR-instrumented driver instructions and
    # are not the timed-work accounting denominator.
    text = (directory / f"accounting-dr-{row}.out").read_text()
    plans = (directory / f"accounting-dr-{row}.plans").read_text()
    base = int(re.search(r"JIT_CORPUS base=(0x[0-9a-f]+)", text)[1], 16)
    versions = [tuple(map(int, v)) for v in re.findall(r"JIT_VERSION round=(\d+) phase=(\d+) target=(\d+) version=(\d+) calls=(\d+) checksum=(\d+)", version_text)]
    calls = 32 if row.endswith("high") else 512 if row.endswith("medium") else 8192
    expected = []
    for r in range(64):
        for phase in range(4):
            target = 512 if phase >= 2 else 1024 + (r % 8 * 64 if row.startswith("append") else 0)
            version = 1 if phase >= 2 or row.startswith("zero") else r + 2
            n = 1 if phase % 2 == 0 else calls - 1
            expected.append((r, phase, target, version, n, n * (version + 11)))
    assert versions == expected
    for prefix in ["final-diagnostic-linked", "stage-b-diagnostic"]:
        candidate = (directory / f"{prefix}-{row}.log").read_text()
        observed = [tuple(map(int, v)) for v in re.findall(r"JIT_VERSION round=(\d+) phase=(\d+) target=(\d+) version=(\d+) calls=(\d+) checksum=(\d+)", candidate)]
        assert observed == expected, (prefix, row)
    phases = re.findall(r"REF_PHASE (begin|end) payments=(\d+) left=(-?\d+)", plans)
    assert [p[0] for p in phases] == ["begin", "end"]
    _, before, left_before = phases[0]
    _, after, left_after = phases[1]
    payments = int(after) - int(before)
    between = plans.split("REF_PHASE begin", 1)[1].split("REF_PHASE end", 1)[0]
    emitted, reconstructed, deleted = collections.Counter(), collections.Counter(), collections.Counter()
    for address, length, translating in re.findall(r"REF_PLAN (0x[0-9a-f]+) (\d+) translating=(\d)", between):
        offset = int(address, 16) - base
        if 0 <= offset < 4096:
            (reconstructed if translating == "1" else emitted)[f"{offset}:{length}"] += 1
    for address in re.findall(r"REF_DELETE (0x[0-9a-f]+)", between):
        offset = int(address, 16) - base
        if 0 <= offset < 4096:
            deleted[str(offset)] += 1
    publications = 0 if row.startswith("zero") else 64
    assert emitted["512:14"] == publications
    assert deleted["512"] == publications
    assert reconstructed["12:18"] == 256
    return dict(progress_calls_between_markers=payments, left_begin=int(left_before), left_end=int(left_after),
                charged_between_markers=int(left_before) + payments * 50000 - int(left_after),
                emitted=dict(emitted), reconstructed=dict(reconstructed), deleted=dict(deleted), version_checks=len(versions))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("evidence", type=Path)
    args = p.parse_args()
    result = {name: matrix(args.evidence / f"{name}-measurements") for name in ["stage-a", "stage-b", "short"]}
    result["reference_diagnostics"] = {row: diagnostics(args.evidence / "container-evidence", row) for row in measure.ROWS}
    result["major_gates"] = {}
    for mode, (loops, left) in {"default": (585, 50233), "zero": (391, 50157),
                               "negative": (390, 50157), "overshoot": (0, 49996),
                               "indirect16": (585, 50233), "indirect17": (585, 50233),
                               "return16": (585, 50233), "return17": (585, 50233)}.items():
        for name in [f"{mode}-fixed", f"stage-b-{mode}"]:
            text = (args.evidence / "container-evidence" / f"{name}.log").read_text()
            assert f"loops={loops} payments=3 left={left} gpr-flags-fp-tls-stack=exact old-pin=reclaimed" in text
            assert "SEEDBOX_INIT_PASS" in text and "EXECUTOR_FAIL" not in text
            result["major_gates"][name] = "passed"
    for prefix in ["", "stage-b-"]:
        for mode in ["gateway-fault", "boundary-x0-fault"]:
            text = (args.evidence / "container-evidence" / f"{prefix}{mode}.log").read_text()
            assert "EXECUTOR_FAIL" in text and "SEEDBOX_INIT_PASS" not in text
    wrong_hint = (args.evidence / "container-evidence/stage-b-return-hint-miss.log").read_text()
    assert "SEEDBOX_INIT_PASS" in wrong_hint and "EXECUTOR_FAIL" not in wrong_hint
    result["hvf"] = {}
    for stage in ["stage-a-final", "stage-b"]:
        hvf = json.loads((args.evidence / f"{stage}-hvf.json").read_text())
        assert hvf["powered_off"] and "SEEDBOX_INIT_PASS" in hvf["console"] and "EXECUTOR_FAIL" not in hvf["console"]
        assert len(hvf["marked_calls"]) == 3 and len(hvf["unmarked_calls"]) == 1
        assert all(c["elapsed_ns"] == 51000 and c["vmm_horizon_returns"] == 5 for c in hvf["marked_calls"])
        assert hvf["unmarked_calls"][0]["vmm_horizon_returns"] == 0
        result["hvf"][stage] = hvf["marked_calls"]
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
