# Prior-engine runtime RPC acceptance — 2026-10-06

The Python controller and public guest RPC passed the complete nine-execution
Linux KVM sequence. These results qualify the runtime API and three-service
Python example for the exact engine and supervisor hashes recorded below. They
precede the root-layout merge and supervisor-readiness changes and do not qualify
the current engine. The earlier static-schedule results are preserved
in [their historical record](ACCEPTANCE-STATIC-20261006.md).

## Reproduction and inputs

Host: `nixos.local`, isolated checkout
`/home/gvickers/seedbox-scoped-faults-20261006`. From that checkout:

```sh
CARGO_TARGET_DIR="$PWD/live-target" nix develop ./sim -c \
  bash sim/seedbox/tests/accept-runtime.sh \
  "$PWD/artifacts/bzImage" 14 "$PWD/acceptance-rpc-1" faults
```

The command returned zero. The summary `acceptance-rpc-1/acceptance.json` has
`status: passed`; full retained images, manifests, transcripts, console logs and
command output remain under that directory. Use a fresh output for another run.

Inputs: seed 1009; three Python guests with 512 MiB each; 1 ms network latency,
200 us network jitter, 1 ms timer jitter, 2 ms ordinary stop tolerance; 30 s simulated
limit; control latency 7 ms. The `scenario` service is both controller and check.
Baseline margin is 98304. Ordinary replay disables undo; the additional healthy
replay uses margin 131072 and undo 16. Replays execute the retained Python again.

| Tested artifact | SHA-256 |
| --- | --- |
| Kernel | `42e25e658245fe265be422814878ea68ab766efac624bc0708cd32556b306626` |
| CLI | `7533722dd43e64262facfabc6b1c0630bb52d323528c3c3ad237308fdfdf5a6d` |
| VMM | `38224afc197db5a284c3f4b9b5ae1e5229bb98d54d0f318ab4265623d8f2237b` |
| Normal supervisor | `ae6a793490602352d2f3234466c0e62205b04e64846d89b474b4b71735c265cb` |
| Missing-effect supervisor | `26175bef153a5179184f3bc6c37005a6c54fc8818e67044e59f5441ba0cc6309` |
| Rejected-effect supervisor | `289f8db5ef74abe5a03a558cb7541f23916ef03058265942636b7405b9fc6d12` |

## Results

| Case | Expected and observed result | Retained run suffix |
| --- | --- | --- |
| Healthy | PASS; 128 ACKs, 0 missing | `9ddaa1c31d69eefa-aPxWtR` |
| Healthy replay, undo 0 | PASS / MATCH; 128 ACKs, 0 missing | `9ddaa1c31d69eefa-wEkL6g` |
| Healthy replay, changed margin/undo | PASS / MATCH; 128 ACKs, 0 missing | `9ddaa1c31d69eefa-veTAQj` |
| Memory-only server | FAIL; 128 ACKs, 96 missing | `7e13ed9a60988c9e-RRFmQs` |
| Memory-only replay | FAIL / MATCH; 128 ACKs, 96 missing | `7e13ed9a60988c9e-YuJ5Be` |
| Memory-only without restarts | PASS; 128 ACKs, 0 missing | `3c85b13bc4b5563d-ESuULO` |
| No-restart replay | PASS / MATCH; 128 ACKs, 0 missing | `3c85b13bc4b5563d-hCNzYf` |
| Missing effect | ERROR; correct cause detected | `41aacc402baab7e1-rzeTUD` |
| Rejected effect | ERROR; correct cause detected | `2a1aa216ae4b17f7-fWoHj0` |

The healthy server reloads 32 records on its second start and 96 on its third.
The independent client verifies all 128 acknowledged values. The memory-only
server loses the 96 values acknowledged before the final restart, and the same
server/client pair passes all 128 checks without restarts. Scenario decisions
follow actual client progress; the checker has no planted failure.

The healthy controller issued 16 RPCs: eight fault operations, three sleeps,
three inspections and two time queries. Every response has correlated request,
admission, effect and strict delivery times. Each admission is exactly 7 ms after
its request, and each response is delivered exactly 7 ms after its effect. All
eight fault operations completed. Request/effect/reply histories contribute to
the witness and match under both replay performance settings.

Client-to-server drops increased from 4 after the first heal to 8 at the
last heal; NIC acceptances remained at 322 during that interval and reached
1291 by completion. The client's probes recorded four failed writes while both
handles were active and four after partial healing; a fresh batch of 32 real
acknowledgments followed final healing. Controller traffic remained available.

| Case | Matching witness |
| --- | --- |
| Healthy | `94cec929c1073616e0131229a07f33067dc836d3ffc85c5d3e9c4c8af1a99a22` |
| Memory-only server | `af92276fe50a9644955d944649ef377e67d940ada81b05321abd85e257458639` |
| Memory-only without restarts | `cd3717f9dc6d60e08834264b37aed023e3f277a58b9ee0eca1caaf6b8a608b69` |

All nine invocations restored the original MSR values on CPUs 14 and 30:
`0x4480000000000` before and after. No case needed a retry.

## Review and other checks

Independent Sol 6.1 review completed with all findings resolved, including
sequential request IDs, UTF-8 and timing validation, exact reply ordering,
controller completion, UART divisor-byte filtering and transcript correlation.
The final narrow review found no remaining actionable issues.

Portable acceptance passed on Linux and macOS, including the Python binding
and real native Python client/server tests; macOS explicitly skips Linux-only
VMM compilation. Seedbox Clippy passed with warnings denied, and all guest
supervisor variants compiled with `-Wall -Wextra -Werror`. The final UART framing
change passed its focused Linux tests before the live sequence. Native tests
also cover the actual supervisor socket relay, causal timing bounds, operation
errors and evidence preservation. Seedbox formatting and diff whitespace pass.

This qualifies one controller connection and sequential RPCs per run, using the
public version 1 protocol. It covers process restarts with guest-local storage
and overlapping network partitions. Whole-machine reboot and persistent block
devices remain separate work. The VMM-wide formatting check reports only existing
formatting differences in the unchanged `undo.rs`.
