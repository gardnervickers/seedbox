# Shared-memory fault API acceptance — 2026-10-06

All thirteen executions of the expanded synthetic acceptance sequence produced
the expected outcomes, including every replay. The VMM implementation is from
`fec6cef` and uses the `seedbox` virtio-console shared-memory channel. The prior
[UART RPC record](ACCEPTANCE-RPC-20261006.md) and
[static-schedule record](ACCEPTANCE-STATIC-20261006.md) retain their own artifact
identities and do not qualify this engine.

## Reproduction

Host: `nixos.local`; isolated checkout:
`/home/gvickers/seedbox-scoped-faults-20261006`.

```sh
nix develop -c bash crates/seedbox/tests/accept-runtime.sh \
  "$PWD/artifacts-shmem/bzImage" 14 "$PWD/acceptance-shmem-1" faults
```

The command exited zero. `acceptance-shmem-1/acceptance.json` has
`status: passed`, thirteen retained results and thirteen successful CPU/SMT
restoration checks. Both CPU 14 and sibling 30 returned to their exact original
MSR value, `0x4480000000000`, after every execution. Use a fresh output directory
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
| Healthy | PASS | `a8bdd9c1f66bba6d-CWIMtT` |
| Healthy replay, undo 0 | PASS / MATCH | `a8bdd9c1f66bba6d-NpGzxY` |
| Healthy replay, changed margin/undo | PASS / MATCH | `a8bdd9c1f66bba6d-prnGlR` |
| Memory-only variant | FAIL | `29ca378042f582ef-Eip24F` |
| Memory-only replay | FAIL / MATCH | `29ca378042f582ef-c9SIbI` |
| Memory-only without faults | PASS | `4902c790f03bb204-95i6uy` |
| No-fault replay | PASS / MATCH | `4902c790f03bb204-HsGh38` |
| Missing effect | ERROR | `faa635a3cae8057f-jMmYIC` |
| Rejected effect | ERROR | `6c1d70d335b0f000-tpOBXM` |
| Early request | PASS | `b9bd045ee0ecc8fb-DYajV6` |
| Early request replay | PASS / MATCH | `b9bd045ee0ecc8fb-lNA1It` |
| Pending request cutoff | TIMEOUT | `0ab779285bad4d7d-Q1KTCc` |
| Cutoff replay | TIMEOUT / MATCH | `0ab779285bad4d7d-VUEXbx` |

Healthy recovery retained all 128 independently acknowledged writes. The
memory-only variant lost 96 acknowledged writes after restarts, reproduced that
FAIL on replay, and passed without faults. Both active overlapping partitions
blocked probe traffic; releasing only the first handle kept the cut active.

The early stop was admitted at 483370631 ns, before the server's readiness at
4039284725 ns. Its confirmed effect occurred at 4046513522 ns, after readiness
plus the control delivery interval. The workload then recovered and completed
128 acknowledged writes; replay matched all timing and witness evidence.

The cutoff stop was observed at 304571066 ns and admitted at 311571066 ns.
The world ended at 3000000000 ns, before the unchanged 5311571066 ns effect
deadline. The pending request remained in the transcript, no effect was claimed,
and the result stayed TIMEOUT. Replay matched. Missing and explicitly rejected
effects still produced the expected ERROR results.

All retained runs were rechecked with assertions that every guest has the
`virtio-console-v1` channel, controller requests carry transmitted bytes, and
delivered responses produce shared-memory DMA bytes.

## Artifact identities

These hashes identify the binaries used by the thirteen-execution sequence.
The final CLI report-validation follow-up is recorded separately below.

| Artifact | SHA-256 |
| --- | --- |
| `artifacts-shmem/bzImage` | `e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847` |
| `target/release/seedbox` | `da1738e4fa752f1034d21e080d21a8fa72f5ce57d645cb0bf2479b34228a0f4c` |
| `crates/seedbox-vmm/target/release/seedbox-vmm` | `f40966b025f20555f8863dd1c8faee77e70ca431ef385bc38bc64b1431ef608a` |
| `acceptance-shmem-1/compose-init` | `34017cd9b0c31125cab5b1b23bb02b1ea5c1ef4caeb3b20b55bedcf7e44c3244` |
| `acceptance-shmem-1/compose-init-delayed` | `e4f1093c2ded2996d7a3a650e42d4bbbdd74e79d2e624903361f2cface3cef28` |
| `acceptance-shmem-1/compose-init-missing` | `ce624949714ecdca8c45eadd282ac6f4a9b31581f4742bf1f5f376528d4bfc4b` |
| `acceptance-shmem-1/compose-init-rejecting` | `f0fcd0c712f4f50485581518ab57bb341b01aedb2ca2e94d2c49953afa9defd1` |

## Portable checks and review

Workspace tests and portable suites passed on macOS and Linux, including the
Linux supervisor/process tests and six virtio-console device tests. Seedbox
Clippy with warnings denied and formatting checks passed. The independent
Sol 6.1 reviewer found a rejected-row/cutoff correlation issue; a separate
accepted-request counter and regression preserve the original error, structured
report, and witness. The reviewer rechecked that fix and reported no outstanding
findings. The Claude skill was unavailable in this session.

## Final CLI confirmation

After rebuilding the final CLI, all thirteen retained runs passed the current
transport assertions again. The final CLI then replayed the pending-cutoff case
as `0ab779285bad4d7d-Rsw71t`: TIMEOUT / MATCH, with witness
`5b4d72e2e5b8d2c4ab0cb51e1557b2ade6975f3b2dca08991da518bacb623e21`.
Its scoped CPU/SMT restoration also passed. The extra execution is recorded in
`acceptance-shmem-final-cli/acceptance.json` (`status: passed`).

Final CLI SHA-256:
`8771a7df3393d3782da7e3abaa6f53ea68c1276ba770e9e08e60c052e3efdbb8`.
The kernel, VMM, retained images and supervisors are unchanged from the matrix.
