#!/usr/bin/env python3
"""Check the measured probe suite and retain compact, auditable evidence."""
import argparse
import json
from pathlib import Path
import shutil
import statistics


def load(path):
    return json.loads(path.read_text())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("runs", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    batches = ["pilot", "repeats", "no-entropy", "planted-failure", "plain-tcg", "final-smoke", "timeout"]
    rows = {name: load(args.runs / name / "runs.json") for name in batches}
    repeated = rows["repeats"]
    checks = {
        "nine_successful_repeats": len(repeated) == 9 and all(r["status"] == "PASS" for r in repeated),
        "same_seed_console_and_observations": all(
            len({r[key] for r in repeated if r["seed"] == seed}) == 1
            for seed in [42, 43, 44] for key in ["console_sha256", "observations_sha256"]),
        "five_fresh_seed_42": len([r for r in repeated if r["seed"] == 42]) == 5,
        "no_entropy_control": [(r["status"], r["reason"]) for r in rows["no-entropy"]] == [("FAIL", "entropy_not_ready")],
        "planted_failure_control": [(r["status"], r["reason"]) for r in rows["planted-failure"]] == [("FAIL", "planted_failure")],
        "plain_tcg_diverges": len(rows["plain-tcg"]) == 2 and all(r["status"] == "PASS" for r in rows["plain-tcg"])
            and len({r["observations_sha256"] for r in rows["plain-tcg"]}) == 2,
        "timeout_control": [r["status"] for r in rows["timeout"]] == ["TIMEOUT"],
        "rebuilt_probe_matches": rows["final-smoke"][0]["status"] == "PASS"
            and rows["final-smoke"][0]["observations_sha256"] == repeated[0]["observations_sha256"],
    }
    # Check entropy itself varies; differing seed labels in logs are insufficient.
    entropy = {}
    for row in repeated:
        events = load(args.runs / "repeats" / f's{row["seed"]}-r{row["index"]}.observations.json')
        entropy[row["seed"]] = next(e["bytes"] for e in events if e.get("phase") == "payload")
    checks["seed_changes_payload_entropy"] = len(set(entropy.values())) == 3
    summary = {
        "schema": "seedbox.mac-tcg.evidence.v1", "checks": checks,
        "comparison_scope": "complete console and probe observations only; no execution or full-state witness",
        "host": load(args.runs / "host.json"),
        "repeat_wall_seconds": {"median": statistics.median(r["wall_seconds"] for r in repeated),
                                "min": min(r["wall_seconds"] for r in repeated),
                                "max": max(r["wall_seconds"] for r in repeated)},
        "all_qemu_wall_seconds": sum(r["wall_seconds"] for rs in rows.values() for r in rs),
        "runs_including_controls": sum(map(len, rows.values())),
        "payload_entropy_by_seed": entropy,
    }
    if not all(checks.values()):
        raise SystemExit(json.dumps(summary, indent=2))
    args.output.mkdir(parents=True, exist_ok=False)
    shutil.copyfile(args.runs / "host.json", args.output / "host.json")
    for batch in batches:
        dest = args.output / batch
        dest.mkdir()
        # Large kernel/ELF/cpio files remain in ignored run bundles, with hashes
        # in these manifests. Preserve exact source snapshots and raw text.
        for src in sorted((args.runs / batch).iterdir()):
            if src.suffix in {".json", ".console", ".stderr", ".py", ".c", ".nix", ".lock", ".sh"}:
                shutil.copyfile(src, dest / src.name)
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
