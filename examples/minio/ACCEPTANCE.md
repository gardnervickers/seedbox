# MinIO acceptance evidence

Source scope on 2026-10-07, Apple Silicon macOS, based on checkout `82f1adae51f96ec4fa44a5e9b338ce79b54f6231`. The carried fixture is issue #48's reviewed index tree `af8aa8ac73ffe4c731a78d6123bf4b14d2768330` on `d6647dd2bd29cf0340ddbc5e01a0911f1817630c`; the Git object and all 16 original fixture files were available locally. Packaging paths were moved from `minio-cas` to `minio`, the qualification template became `compose.yaml`, and input pins were preserved. No files under `crates/seedbox-vmm/`, `kernel/`, `guest/` or admission policy were edited.

## Local results

- Fresh official binary download: exact 110,989,496-byte size and SHA-256 `7c5bd8512c6e966455b1d198209358b2d191c77a83ab377c4073281065fb855f` verified locally. `file` reports static ELF x86-64. The binary was never executed. All seven pinned wheels were acquired and size/hash verified.
- `python3 examples/minio/portable.py --wheel-directory /tmp/seedbox-minio-inputs/wheels`: 18 tests PASS with socket creation forbidden, using the actual pinned SDK. Includes 11 reviewed controls plus bounded readiness, ambiguous PUT/no acknowledgement, retained-evidence and staging/generation controls. Latest receipt: `/tmp/seedbox-minio-controls-final.log`, 5.803 seconds; disposable local receipts are not durable hardware evidence.
- Generation from actual verified artifacts produced three complete projects under `/tmp/seedbox-minio-final-projects`; all three public CLI validations returned VALID. Source qualification/broken/fixed templates also validate. This does not execute OCI preparation.
- `cargo fmt -p seedbox --check` and `cargo clippy -p seedbox --all-targets --locked -- -D warnings`: PASS.
- `cargo test --workspace --locked`: first invocation passed, but repeated checks and `bash harness/acceptance.sh portable` hit existing test `dev_replay_permission_exact_hashes_and_bundle_marker_round_trip` at `crates/seedbox/tests/bundle.rs:860` (`stdout.starts_with("ERROR: ")`). A fresh `/tmp/seedbox-minio-check-target` build reproduced the same assertion. The focused test also failed in a clean archive of unchanged baseline `82f1adae51f96ec4fa44a5e9b338ce79b54f6231`, without this example, confirming a pre-existing Mac failure. No Rust/test/runtime source was changed in this application lane. The acceptance script's 35 harness Python tests and eight SDK Python tests passed before the bundle assertion. Failing receipts: `/tmp/seedbox-minio-workspace-final.log`, `/tmp/seedbox-minio-workspace-clean.log`, `/tmp/seedbox-minio-portable-acceptance.log`, `/tmp/seedbox-minio-baseline-test.log`.
- AMD checklist Bash blocks pass `bash -n`. Command options were checked against the current public CLI help.
- `cargo build-vmm` / `cargo test-vmm`: not run; the standalone VMM is Linux-only and cannot run on this Mac. Portable device-model tests are not KVM evidence.

## Readiness follow-up on PR #104

Checked on 2026-10-07, Apple Silicon macOS, against parent `1a72e8f`. Readiness now uses a 30-second guest monotonic deadline, including request time, instead of eight attempts. The pinned botocore transport maps refused connections to `EndpointConnectionError`, reset/closed connections to `ConnectionClosedError`, and connect/read timeouts to their corresponding timeout exceptions. These four exceptions and HTTP 500/503 are retried only during readiness. Exhaustion emits a distinct infrastructure diagnostic with the last error, exits 2, and starts no workload or acknowledgement. The existing Seedbox exit-based check still retains FAIL for exit 2; the application verifier rejects it as evidence of the planted failure. An in-flight SDK request can finish after the deadline under its finite timeouts, but late success is rejected.

- `python3 examples/minio/portable.py --wheel-directory /tmp/seedbox-minio-inputs/wheels`: all 24 tests PASS with sockets forbidden. New controls cover actual pinned SDK socket-error translation, every retryable startup error through success at 29.75 seconds and exhaustion at 30 seconds, request-time accounting, final sleep truncation, late success rejection, immediate non-startup rejection, no workload/acknowledgement on infrastructure failure, and the verifier's expanded receipt bound. Receipt: `/tmp/seedbox-minio-readiness-controls.log`.
- Required fmt and clippy commands: PASS. Source MinIO and all four acceptance YAML projects: VALID.
- Required workspace tests and `bash harness/acceptance.sh portable`: FAIL at the existing `dev_replay_permission_exact_hashes_and_bundle_marker_round_trip` assertion, `crates/seedbox/tests/bundle.rs:860`. Continuing the workspace exposed `old_host_proof_replay_is_not_comparable_while_inspection_and_transfer_stay_available`, `crates/seedbox/tests/cli.rs:510`. Both failures reproduce in a clean archive of unchanged parent `1a72e8f`. All other workspace tests PASS with those two excluded (250 passed, three already ignored). The acceptance script's 35 harness and eight control-API Python tests PASS. Receipts: `/tmp/seedbox-minio-readiness-workspace.log`, `/tmp/seedbox-minio-readiness-acceptance.log`, `/tmp/seedbox-minio-readiness-baseline.log`, `/tmp/seedbox-minio-readiness-baseline-cli.log`, `/tmp/seedbox-minio-readiness-other-workspace.log`.
- Linux-only `cargo build-vmm` / `cargo test-vmm` were not run on this Mac. No real MinIO guest startup, OCI preparation, KVM execution or replay was performed. Engine, kernel, guest, release policy and frozen paths remain untouched.

## Hardware evidence still required

Execute [AMD.md](AMD.md) without substituting artifacts or increasing the trial memory/timeouts. Record actual input closure, accepted host/profile identity, memory/startup and command/preparation timings, raw console/HTTP receipts, witnesses, replay comparisons and verified cleanup.

| Fixture/job | Required evidence | Current status |
| --- | --- | --- |
| Generated `qualification/compose.yaml`, seed 0, 2048 MiB/VM | PASS, six actual conditional-write/GET cases, real binary compatibility | Unrun |
| Generated `broken/compose.yaml`, same trial settings | FAIL, client exit 1; acknowledged A overwritten by unconditional B after 412 | Unrun |
| Original broken retained replay | MATCH / exit 0, application FAIL, exact inputs/profile/witness | Unrun |
| Generated `fixed/compose.yaml`, same trial settings | PASS, preserved conflict and acknowledged A | Unrun |
| Export original FAIL; fresh offline import and replay | MATCH / exit 0, application FAIL without registry/source preparation | Unrun |

MinIO runtime/CAS/memory admission, native image preparation and cleanup, both deterministic replays, offline closure, integrated AMD/fresh-install qualification and the measured demo remain unverified. Three-region/two-of-three ledger, finite seed exploration and pre-armed ambiguity/selected-flow scenarios remain follow-ups. These source controls do not complete #48, #35 or V0.
