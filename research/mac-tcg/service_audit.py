"""Verify receipts against raw retained artifacts before making pass claims."""
import json
import math
from pathlib import Path

from run import digest


def load(path):
    return json.loads(path.read_text())


def require(condition, message):
    if not condition:
        raise ValueError(message)


def valid_witness(witness, returncode):
    if not isinstance(witness, dict):
        return False
    integers = [witness.get(k) for k in ("events", "instruction_events", "discontinuities", "terminal_code")]
    return (all(type(n) is int and n >= 0 for n in integers)
            and witness.get("schema") == "seedbox.tcg.boundaries.v2"
            and witness.get("terminal") is True and integers[0] > 0
            and integers[0] == integers[1] + integers[2]
            and integers[3] <= 255 and returncode == ((integers[3] << 1) | 1)
            and isinstance(witness.get("sha256"), str) and len(witness["sha256"]) == 64
            and all(c in "0123456789abcdef" for c in witness["sha256"]))


def verify_batch(batch, *, mode=None, witness_required=None):
    from service import classify
    rows = load(batch / "runs.json")
    prefixes = {f"s{row['seed']}-r{i}" for i, row in enumerate(rows)}
    for suffix in ("console", "mgmt", "witness.json", "manifest.json", "attempt.json"):
        found = {p.name.removesuffix("." + suffix) for p in batch.glob("*." + suffix)}
        require(found <= prefixes, f"orphan {suffix} in {batch}")
    for i, row in enumerate(rows):
        prefix = batch / f"s{row['seed']}-r{i}"
        console = Path(str(prefix) + ".console").read_bytes()
        management_path = Path(str(prefix) + ".mgmt")
        management = management_path.read_bytes() if management_path.exists() else b""
        require(digest(console) == row["console_sha256"], f"console hash mismatch: {prefix}")
        require(digest(management) == row["management_sha256"], f"management hash mismatch: {prefix}")
        witness = row.get("witness")
        witness_path = Path(str(prefix) + ".witness.json")
        if witness is not None:
            raw = load(witness_path)
            if row.get("witness_diagnostics") is not None:
                require({"post_terminal_events": raw.pop("post_terminal_events")} == row["witness_diagnostics"], f"witness diagnostics mismatch: {prefix}")
            require(raw == witness, f"witness mismatch: {prefix}")
        else:
            rejected = row.get("witness_rejected_sha256")
            if witness_path.exists():
                require(row["status"] in {"ERROR", "TIMEOUT", "INTERRUPTED", "INCOMPLETE"}
                        and digest(witness_path.read_bytes()) == rejected, f"unaccounted rejected witness: {prefix}")
            else:
                require(rejected is None, f"missing rejected witness: {prefix}")
        if mode is not None:
            manifest = load(Path(str(prefix) + ".manifest.json"))
            require(manifest["seed"] == row["seed"] and manifest["mode"] == mode, f"manifest identity mismatch: {prefix}")
            actual = classify(console.decode(errors="replace"), management.decode(errors="replace"), row["returncode"], row["status"] == "TIMEOUT", mode)
            require(actual == (row["status"], row["reason"]), f"classification mismatch: {prefix}")
            if witness_required:
                require(valid_witness(witness, row["returncode"]), f"invalid required witness: {prefix}")
                require(manifest.get("witness_plugin_sha256") is not None and "-plugin" in manifest["argv"], f"missing witness invocation: {prefix}")
            elif witness_required is False:
                require(witness is None and manifest.get("witness_plugin_sha256") is None and "-plugin" not in manifest["argv"], f"instrumented no-witness control: {prefix}")
    return rows


def verify_profiles(batches, reference):
    """Compare witnesses only within the same declared runtime/plugin inputs.
    This establishes input identity, not correctness of a historical plugin.
    """
    keys = ("profile", "shift", "qemu_sha256", "kernel_sha256", "base_sha256", "witness_plugin_sha256")
    require(all(reference.get(k) is not None for k in keys), "incomplete reference profile")
    for batch in batches:
        for i, row in enumerate(load(batch / "runs.json")):
            prefix = batch / f"s{row['seed']}-r{i}"
            manifest = load(Path(str(prefix) + ".manifest.json"))
            require(all(manifest.get(k) == reference[k] for k in keys), f"witness profile mismatch: {prefix}")


def verify_inventory(runs, budget):
    require(not budget.get("pending"), "unreconciled pending attempt")
    attempts = {}
    for batch in sorted(runs.glob("service-*")):
        if not batch.is_dir():
            continue
        if not (batch / "runs.json").exists():
            require(not any(batch.glob("*.console")) and not any(batch.glob("*.attempt.json")), f"unreported attempt: {batch}")
            continue
        for i, row in enumerate(verify_batch(batch)):
            attempts[f"{batch.name}/s{row['seed']}-r{i}"] = row["wall_seconds"]
    ledger = {}
    for row in budget["runs"]:
        path = Path(row["path"])
        key = f"{path.parent.name}/{path.name}"
        require(key not in ledger, f"duplicate ledger attempt: {key}")
        require(type(row["wall_seconds"]) in (int, float) and math.isfinite(row["wall_seconds"]) and row["wall_seconds"] >= 0, f"invalid duration: {key}")
        ledger[key] = row["wall_seconds"]
    require(attempts == ledger, "budget does not match retained attempts")
    require(abs(sum(ledger.values()) - budget["spent_seconds"]) < 0.001, "budget total mismatch")
    require(0 < budget["limit_seconds"] <= 600 and budget["spent_seconds"] <= budget["limit_seconds"], "aggregate service budget exceeded")
