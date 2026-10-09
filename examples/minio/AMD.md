# AMD KVM acceptance checklist

Run from this committed checkout in the project development shell, on an AMD Zen KVM host as an authorized operator. Select an available physical core/SMT pair under the normal shared lease; `CPU` below is the chosen logical CPU, not a reservation of any historical lane. Preserve other workloads. Use one CLI, VMM, kernel, supervisor, static BusyBox and project-native Podman for every step. Do not increase memory/deadlines or silently retry failed worlds.

If a Linux source build is needed, the existing aliases are `cargo build --release`, `cargo build-vmm` and `cargo test-vmm`; build the revision under test. The checklist passes the artifacts explicitly. `doctor` checks host compatibility only; replay MATCH is the determinism check.

## Stage and validate

Bind these paths and the selected CPU to the artifacts under test; use fresh writable evidence/input/project directories. Record revision, artifact hashes, host/profile observations, full command logs and elapsed times, and verify CPU/lease cleanup after each guest job. Stop on uncertain cleanup. Preparation happens in each `run`; there is no standalone prepared-input adapter. Retain each run's complete OCI/config/layers/archives/provenance, even if variants rebuild differently.

```bash
set -euo pipefail
SEEDBOX=/absolute/seedbox
VMM=/absolute/seedbox-vmm
KERNEL=/absolute/bzImage
GUEST_INIT=/absolute/guest-init
BUILDER=/absolute/project-native/podman
CPU=8  # Replace with the currently available admitted lane.
EVIDENCE=/absolute/fresh-minio-evidence
INPUTS=/absolute/fresh-minio-inputs
PROJECTS=/absolute/fresh-minio-projects
mkdir "$EVIDENCE"

git rev-parse HEAD > "$EVIDENCE/revision.txt"
sha256sum "$SEEDBOX" "$VMM" "$KERNEL" "$GUEST_INIT" "$BUSYBOX" > "$EVIDENCE/artifacts.sha256"
python3 examples/minio/fetch_inputs.py "$INPUTS" > "$EVIDENCE/acquisition.log"
# Alternatively reuse previously acquired #48 inputs; generation verifies them.
python3 examples/minio/portable.py --wheel-directory "$INPUTS/wheels" > "$EVIDENCE/portable.log" 2>&1
python3 examples/minio/generate.py "$PROJECTS" --minio "$INPUTS/minio" --wheel-directory "$INPUTS/wheels"
"$SEEDBOX" validate "$PROJECTS/qualification"
"$SEEDBOX" validate "$PROJECTS/broken"
"$SEEDBOX" validate "$PROJECTS/fixed"
sudo -E env PATH="$PATH" "$SEEDBOX" doctor --cpu "$CPU" --kernel "$KERNEL" --vmm "$VMM" --json > "$EVIDENCE/doctor.json"
RUNTIME=(--cpu "$CPU" --kernel "$KERNEL" --vmm "$VMM" --guest-init "$GUEST_INIT")
```

Expected: every checksum/control and all three validations succeed; doctor admits the profile and completes cleanup. MinIO packaging itself remains unqualified until the next step. Cap and record preparation/full-command wall time under the host's execution supervision; Compose caps only backend execution. The initial four worlds have at most 270 seconds summed execution caps, and the added offline replay raises that to 330 seconds across five worlds. These are caps, not measured runtimes or a total CLI wall-time promise. Abort further dispatch on exhaustion, ambiguity, nonexpected results or unverified native builder cleanup.

## Qualification, broken, replay, fixed

These are public CLI invocations, with guest-owned assertions. Preserve logs on any refusal. The shell checks exit status and `verify.py` rejects setup failures masquerading as the intended FAIL.

```bash
sudo -E env PATH="$PATH" "$SEEDBOX" run "$PROJECTS/qualification" --seed 0 "${RUNTIME[@]}" \
  --state "$EVIDENCE/state" > "$EVIDENCE/qualification.log" 2>&1
QUALIFICATION=$(sed -n 's/^[A-Z]*: //p' "$EVIDENCE/qualification.log")
"$SEEDBOX" inspect "$QUALIFICATION" --json > "$EVIDENCE/qualification-record.json"
python3 examples/minio/verify.py "$QUALIFICATION" --mode qualification
# Required: PASS / exit 0, all six real CAS cases. Stop otherwise.

set +e
sudo -E env PATH="$PATH" "$SEEDBOX" run "$PROJECTS/broken" --seed 0 "${RUNTIME[@]}" \
  --state "$EVIDENCE/state" > "$EVIDENCE/broken.log" 2>&1
BROKEN_CODE=$?
set -e
test "$BROKEN_CODE" = 1
BROKEN=$(sed -n 's/^[A-Z]*: //p' "$EVIDENCE/broken.log")
"$SEEDBOX" inspect "$BROKEN" --json > "$EVIDENCE/broken-record.json"
python3 examples/minio/verify.py "$BROKEN" --mode broken
# Required: FAIL / exit 1, 200(A), 412(B), unconditional 200(B), GET B != ack A.

sudo -E env PATH="$PATH" "$SEEDBOX" replay "$BROKEN" --cpu "$CPU" --state "$EVIDENCE/state" \
  > "$EVIDENCE/replay.log" 2>&1
REPLAY=$(sed -n 's/^[A-Z]*: //p' "$EVIDENCE/replay.log")
"$SEEDBOX" inspect "$REPLAY" --json > "$EVIDENCE/replay-record.json"
python3 examples/minio/verify.py "$REPLAY" --mode broken --replay
# Required: replay MATCH / exit 0; retained application outcome remains FAIL.
# Memory comes from x-seedbox.machine.memory_mb (2048) and is inherited from the original profile on replay.

sudo -E env PATH="$PATH" "$SEEDBOX" run "$PROJECTS/fixed" --seed 0 "${RUNTIME[@]}" \
  --state "$EVIDENCE/state" > "$EVIDENCE/fixed.log" 2>&1
FIXED=$(sed -n 's/^[A-Z]*: //p' "$EVIDENCE/fixed.log")
"$SEEDBOX" inspect "$FIXED" --json > "$EVIDENCE/fixed-record.json"
python3 examples/minio/verify.py "$FIXED" --mode fixed
# Required: PASS / exit 0; conflict retained, GET A == ack A; no unconditional retry.
```

Any ERROR, INCOMPLETE, TIMEOUT, MISMATCH, setup exit 2 or protocol discrepancy blocks continuation. Record actual cold preparation time, warm demo time and startup/memory observations; 2048 MiB is not a qualified minimum until measured.

## Offline bundle replay

Export the original FAIL, import into a fresh object store with host networking disabled, and replay in that network namespace. Guest networking remains the VMM's internal fabric. This tests the retained closure without registry/PyPI access; neither input acquisition nor image building belongs in replay. Preserve the original evidence/state.

```bash
"$SEEDBOX" export "$BROKEN" --state "$EVIDENCE/state" -o "$EVIDENCE/broken.seedbox.tar"
sudo -E unshare --net env PATH="$PATH" "$SEEDBOX" import "$EVIDENCE/broken.seedbox.tar" \
  --state "$EVIDENCE/offline-state" > "$EVIDENCE/import.log" 2>&1
IMPORTED=$(sed -n 's/^IMPORTED: //p' "$EVIDENCE/import.log")
"$SEEDBOX" inspect "$IMPORTED" --json > "$EVIDENCE/imported-record.json"
python3 examples/minio/verify.py "$IMPORTED" --mode broken
sudo -E unshare --net env PATH="$PATH" "$SEEDBOX" replay "$IMPORTED" --cpu "$CPU" \
  --state "$EVIDENCE/offline-state" > "$EVIDENCE/offline-replay.log" 2>&1
OFFLINE_REPLAY=$(sed -n 's/^[A-Z]*: //p' "$EVIDENCE/offline-replay.log")
"$SEEDBOX" inspect "$OFFLINE_REPLAY" --json > "$EVIDENCE/offline-replay-record.json"
python3 examples/minio/verify.py "$OFFLINE_REPLAY" --mode broken --replay
# Required: export/import exit 0; offline replay MATCH / exit 0, application FAIL.
```

Real CAS results, guest compatibility/memory, retained witnesses and both replay comparisons are still pending. Attach actual receipts and cleanup evidence to the acceptance record only after running these commands; portable fake-transport controls do not establish any of them.
