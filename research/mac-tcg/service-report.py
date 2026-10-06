#!/usr/bin/env python3
"""Audit the local integration experiment and retain small evidence files."""
import argparse
import json
from pathlib import Path
import shutil
import statistics

from service_audit import load, verify_batch, verify_inventory, verify_profiles


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--runs", type=Path, required=True)
    ap.add_argument("--budget", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    args = ap.parse_args()
    budget = load(args.budget)
    verify_inventory(args.runs, budget)
    def rows(name, mode, witness=True):
        return verify_batch(args.runs / name, mode=mode, witness_required=witness)
    fault = rows("service-qualified-fault", "fault")
    controls = rows("service-qualified-controls", "controls")
    healthy = rows("service-qualified-healthy", "healthy")
    negative = rows("service-qualified-negative", "negative")
    sensitivity = rows("service-qualified-seed", "fault")
    off = rows("service-qualified-no-witness", "fault", False)
    fixed = rows("service-qualified-fixed", "fault")
    fixed_manifests = [load(path) for path in [args.runs / "service-qualified-fixed" / f"s{r['seed']}-r{i}.manifest.json" for i, r in enumerate(fixed)]]
    compared = [args.runs / ("service-qualified-" + name) for name in ("fault", "controls", "healthy", "negative", "seed", "fixed")]
    reference = load(compared[0] / "s1009-r0.manifest.json")
    verify_profiles(compared, reference)
    def equal(batch):
        return len({(r["console_sha256"], r["management_sha256"], json.dumps(r["witness"], sort_keys=True)) for r in batch}) == 1
    checks = {
        "five_fresh_fault_runs": len(fault) == 5 and {r["seed"] for r in fault} == {1009} and all(r["status"] == "PASS" for r in fault) and equal(fault),
        "packet_controls_repeat": len(controls) == 2 and all(r["status"] == "PASS" for r in controls) and equal(controls),
        "healthy_128_writes_repeat": len(healthy) == 2 and all(r["status"] == "PASS" and "acked=128 failed=0" in r["reason"] for r in healthy) and equal(healthy),
        "planted_missing_write_detected": len(negative) == 1 and negative[0]["status"] == "FAIL" and "k/99/000000" in negative[0]["reason"],
        "two_additional_seeds_pass_and_change_witness": len(sensitivity) == 2 and {r["seed"] for r in sensitivity} == {1010, 1011} and all(r["status"] == "PASS" and r["witness"]["sha256"] != fault[0]["witness"]["sha256"] for r in sensitivity),
        "witness_does_not_change_guest_observations": len(off) == 1 and off[0]["status"] == "PASS" and all(off[0][key] == fault[0][key] for key in ["console_sha256", "management_sha256", "reason"]),
        "fixed_version_same_seed_set": len(fixed) == 3 and {r["seed"] for r in fixed} == {1009, 1010, 1011} and all(r["status"] == "PASS" for r in fixed),
        "fixed_version_identity": len(fixed_manifests) == 3 and all(m["etcd_version"] == "3.5.3" and m["overlay_sha256"]["bin/etcd"] == "79e672f61c4ae4197cee976fc4e6ab94cf42b40124a42e1c3774dc34c8d01c5e" for m in fixed_manifests),
    }
    old = verify_batch(args.runs / "service-boundary-final")
    checks["uncut_witness_control_detected_drift"] = all(r["status"] == "PASS" and r.get("witness") for r in old) and {r["seed"] for r in old} == {1009} and len({(r["console_sha256"], r["management_sha256"]) for r in old}) == 1 and len({r["witness"]["sha256"] for r in old}) > 1
    checks["post_terminal_work_varies_but_canonical_witness_matches"] = equal(fault) and len({r["witness_diagnostics"]["post_terminal_events"] for r in fault}) > 1
    checks["bounded_total_includes_failed_attempts"] = True  # verify_inventory above
    checks["budget_contains_every_retained_attempt"] = True
    report = {"witness_qualification": {
                  "historical_batches": "provisional: pre-fix terminal decoder; raw integrity and matching runtime inputs verified, not proof of complete execution coverage",
                  "corrected_decoder": "two seed-1009 fault runs qualified separately in review-fixes-2026-10-06; remaining historical witness profiles not requalified"},
              "schema": "seedbox.mac-tcg.service-evidence.v1", "checks": checks,
              "fault_witness": fault[0]["witness"], "fault_result": fault[0]["reason"],
              "fault_wall_seconds": {"median": statistics.median(r["wall_seconds"] for r in fault), "min": min(r["wall_seconds"] for r in fault), "max": max(r["wall_seconds"] for r in fault)},
              "aggregate_qemu_wall_seconds": budget["spent_seconds"], "attempts": len(budget["runs"]),
              "comparison_scope": "selected instruction/exception/interrupt witness through terminal guest I/O, complete console and management log; no full RAM/device snapshot"}
    if not all(checks.values()):
        raise SystemExit(json.dumps(report, indent=2))
    args.output.mkdir(parents=True, exist_ok=False)
    for directory in sorted(args.runs.glob("service-*")):
        if not directory.is_dir():
            continue
        target = args.output / directory.name
        target.mkdir()
        for src in sorted(directory.iterdir()):
            if src.suffix in {".json", ".console", ".mgmt", ".stderr", ".py", ".c", ".h", ".nix", ".lock", ".init", ".sh"}:
                shutil.copyfile(src, target / src.name)
    shutil.copyfile(args.budget, args.output / "budget.json")
    (args.output / "summary.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
