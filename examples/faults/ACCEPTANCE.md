# Shared-memory fault API review acceptance — 2026-10-06

All fifteen executions produced their expected outcomes, and every replay
matched. The runtime implementation is from `f3d00e2`, including the review fixes
for lifecycle errors, sessions, cutoff classification and nominal-time inspection.
Earlier static-schedule, UART and initial shared-memory records remain in
[Git history](https://github.com/gardnervickers/seedbox/tree/67bd5e1bb9149fc0493cef50b351da9330c8f9e6/examples/faults).
They retain their own identities and do not qualify this engine.

## Reproduction

Host: `nixos.local`; isolated checkout:
`/home/gvickers/seedbox-scoped-faults-20261006`.

```sh
nix develop -c bash crates/seedbox/tests/accept-runtime.sh \
  "$PWD/artifacts-shmem/bzImage" 14 "$PWD/acceptance-review-2" faults
```

The command exited zero. `acceptance-review-2/acceptance.json` has
`status: passed`, fifteen retained results and fifteen successful CPU/SMT
restoration checks. CPU 14 and sibling 30 returned to their exact original MSR
value, `0x4480000000000`, after every execution. Use a fresh output directory
for another run.

Seed 1009; three Python guests with 512 MiB each; 1 ms network latency, 200 us
network jitter, 1 ms timer jitter, 2 ms stop tolerance; 7 ms control latency.
The normal limit is 30 simulated seconds. Baseline margin is 98304; ordinary
replays use undo 0, and the additional healthy replay uses margin 131072/undo 16.
The readiness build delays the server supervisor's readiness by four simulated
seconds. The cutoff case has a three-second world limit.

## Results

| Case | Expected and observed | Retained attempt |
| --- | --- | --- |
| Healthy | PASS | `3592c60bdee7247a-3gkTDU` |
| Healthy replay, undo 0 | PASS / MATCH | `3592c60bdee7247a-iiPh11` |
| Healthy replay, changed margin/undo | PASS / MATCH | `3592c60bdee7247a-rSWbNQ` |
| Memory-only variant | FAIL | `4003c775ff654970-7t6jMM` |
| Memory-only replay | FAIL / MATCH | `4003c775ff654970-2xM5yN` |
| Memory-only without faults | PASS | `4aae6a744e57c352-3S8JiP` |
| No-fault replay | PASS / MATCH | `4aae6a744e57c352-uUuo5F` |
| Missing effect | ERROR | `98dd03c1cb2c4e08-ICkQQs` |
| Rejected effect | ERROR | `97b4f09d983a90e1-Re4wLG` |
| Early request | PASS | `0c74baa1af83764a-itrHAc` |
| Early request replay | PASS / MATCH | `0c74baa1af83764a-yu6LCu` |
| Pending request cutoff | TIMEOUT | `c0685bdc2c2dc237-1vPNwG` |
| Cutoff replay | TIMEOUT / MATCH | `c0685bdc2c2dc237-9D8xSd` |
| Lifecycle and reconnect | FAIL | `f531dcb0fb5c9b30-wmn8tv` |
| Lifecycle and reconnect replay | FAIL / MATCH | `f531dcb0fb5c9b30-ltpQqy` |

Healthy recovery retained all 128 independently acknowledged writes. The
memory-only variant lost 96 acknowledged writes after restarts, reproduced that
FAIL on replay, and passed without faults. Overlapping partitions blocked probe
traffic until both handles were released. The changed-margin/undo replay matched
the control history, sampled counters and execution witness.

The early stop was requested at 481786881 ns, before management
readiness at 4038828025 ns, and applied at 4046056538 ns.
The workload recovered all 128 acknowledged writes, and replay matched.

The cutoff stop was requested at 305191722 ns and admitted at
312191722 ns. World termination at 3000000000 ns
preceded its unchanged 5312191722 ns effect deadline. The request
remained pending, no effect was claimed, and TIMEOUT replayed identically.
Missing and explicitly rejected effects still produced ERROR.

The lifecycle scenario received `invalid_state` for restart on a running target
and for a second stop. It closed its first connection, reconnected, and restarted
the stopped target. Both target launches confirmed that no fault socket existed
there. After the target exited with code 17, inspection reported `Exited(17)` and
both process commands returned `target_unavailable`. A subsequent time query
succeeded. The scenario then chose FAIL, and replay matched witness
`5244b67697daa5167e43ad88b8c8566ae7f1299debf3295093ced96420645de0`. Both sessions and all four
recoverable errors are present in the retained transcript.

Every guest used `virtio-console-v1`; requests carried transmitted bytes and
responses produced shared-memory DMA bytes. Every live result was checked by
the harness against its expected structured outcome and transcript.

## Artifact identities

| Artifact | SHA-256 |
| --- | --- |
| `artifacts-shmem/bzImage` | `e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847` |
| `target/release/seedbox` | `eaba56d9c5abfc4e83c3f7af52a78f9b679a47bcbf34e037d4f29c1ac32926bf` |
| `crates/seedbox-vmm/target/release/seedbox-vmm` | `6de8124b762929be4038818a338c58ca8d2da7386971b08bb649d7b7b98fc443` |
| `acceptance-review-2/compose-init` | `05d74f370ce5a09698be9b23e6f2ff2ffa97d224a71825262e2f6f3ee0f2dadf` |
| `acceptance-review-2/compose-init-delayed` | `ce5f4f8d96d3cabf248d5ec058eb25db096a6fa13ec59950acd5e73bdfa746b9` |
| `acceptance-review-2/compose-init-missing` | `cd2af61a95ff4069c735817d766c29ff32d079d29a05b5f30d11446ef0a1a04f` |
| `acceptance-review-2/compose-init-rejecting` | `baa003806d24dbacc18c350a17a42aafe1cfdc999b707d5b8bf39c9d35692cdd` |

## Portable checks and review

Workspace tests and portable suites passed on macOS and Linux. Linux VMM tests,
seven Python SDK tests, native supervisor/parser tests, and strict C builds for
normal/missing/rejected effect supervisors passed. Formatting and workspace
Clippy with warnings denied passed; the separate VMM retains its existing
non-fatal dead-code warning.

The independent Sol 6.1 reviewer found three report-integrity gaps: a claimed
successful controller-target action, an empty partition handle, and an effect
left pending after a successful response. All three now have regression checks.
The reviewer rechecked the fixes and reported no outstanding findings. The
Claude skill was unavailable in this session.
