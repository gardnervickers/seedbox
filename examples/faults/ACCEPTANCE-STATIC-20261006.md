# Structured fault acceptance — 2026-10-06

The synthetic TCP workload passed its complete KVM acceptance sequence on
`nixos.local`, in the isolated checkout
`/home/gvickers/seedbox-scoped-faults-20261006`.
The branch includes merged PR #17 (`99488e5`) and main through `bc07f35`.

## Reproduction

From the checkout root:

```sh
CARGO_TARGET_DIR="$PWD/live-target" nix develop ./sim -c \
  bash sim/seedbox/tests/accept-runtime.sh \
  "$PWD/artifacts/bzImage" 14 "$PWD/acceptance-synthetic-1" faults
```

Use a fresh output directory. The command builds the CLI, VMM and three guest
supervisors, generates all projects, and runs eight executions. It returned zero.
Full manifests, prepared inputs, console logs, command logs, fault histories and
MSR readbacks remain under `acceptance-synthetic-1/`; the incremental summary is
`acceptance-synthetic-1/acceptance.json` with `status: passed`.

Inputs: seed 1009, two guests with 256 MiB each, 1 ms network latency,
200 us network jitter, 1 ms timer jitter, 2 ms ordinary stop tolerance,
30 s simulated limit, explicit performance margin 98304. Fault boundaries remain
exact. Replay reconstructs fresh guests from retained inputs with undo disabled.

| Artifact | SHA-256 |
| --- | --- |
| Kernel | `42e25e658245fe265be422814878ea68ab766efac624bc0708cd32556b306626` |
| CLI | `d7c22e59551cd79cbbd451348eda106297973c5e28eeb863050e8be5d1c4888b` |
| VMM | `781db8f50b0efb666249fd3b36ca669dbae4d0a61728eb665f1e549b88ab4a60` |
| Normal supervisor | `7c3c70b1bebe7b8a743b6115a8f552fe8db335e909e4f04d74b8a48a6e42d0cd` |
| Missing-effect supervisor | `93a4b9327fd2ab7baef2bbff4b7f5fc0c9f1823637b665b67470fab2e52e6de5` |
| Rejected-effect supervisor | `3c3f89ac98abfd8046d8282febc23a6a26913d34a2e9204ee5b27149c6e1a248` |

These identify the tested binaries. Only documentation, output-directory
reservation with two regression tests, and a Rust line-wrap changed afterward;
the scheduler, guest supervisor and workload behavior did not change.

## Results

| Case | Original | Fresh retained replay | Application evidence |
| --- | --- | --- | --- |
| Healthy server, eight operations | PASS | PASS / MATCH | 404 acknowledged writes; zero missing; generations 1, 2 and 3; both restarts reload records |
| Memory-only server, same operations/client | FAIL | FAIL / MATCH | 404 acknowledged writes; 322 missing from before restarts |
| Memory-only server, no operations | PASS | PASS / MATCH | 123 acknowledged writes; zero missing; generation 1 |
| Missing supervisor acknowledgment | ERROR | Not part of this control | Effect deadline detected |
| Explicit rejected acknowledgment | ERROR | Not part of this control | Guest rejection detected |

Both faulted workloads recorded all eight operations as applied. The checker
records actual PUT acknowledgments and subsequently GET-checks every recorded
key. The broken control differs only in server persistence, so its failure
requires the real process restarts. It does not inject a checker assertion.

In the healthy run, client-to-server partition drops increased from 9 at the
first heal to 14 at the final heal. Receiver acceptances stayed at 1750 during
that interval, then increased to 4944 by completion. This proves that healing
one overlapping handle leaves the cut active, and traffic resumes after the
last handle heals. Already routed packets remain queued, as separately covered
by deterministic event-queue tests.

Stop/restart effects were recorded at 5.000019280, 6.000156524, 12.000026431 and
13.000226935 simulated seconds. Partition/heal effects occurred exactly at
8, 9, 10 and 11 seconds. Request IDs, effect times and packet counters matched
on replay and contributed to the faulted execution witness.

| Case | Matching original/replay witness |
| --- | --- |
| Healthy | `8ad11045349423aadb699eb360286399e7f559579c59a4ea3f65d9705061bdef` |
| Broken after restart | `85a09f9b4cad81ab440ebb9958d10dac66eadd12f8be8d8a6e2980c1ed87b076` |
| Broken without restart | `a29716fff5b31a24384849c2012617a056f578c7ee54e5bc55417a1dfd843273` |

Original/replay run suffixes are `qSZXzH`/`DlJC7N`, `q9lvBI`/`8ygAoO`, and
`F16b4j`/`WGCXua`; error controls are `bcb8dF` and `NvYbc4` in the retained
`state/runs` directory. CPUs 14 and 30 both read `0x4480000000000` before and
after every invocation. All eight restoration assertions passed. This final
synthetic sequence needed no retries.

## Review and portable checks

An independent Sol 6.1 reviewer checked the implementation and final harness
(Claude review capacity was unavailable). All findings were resolved: the
supervisor permission span now covers the full 32-bit effect writes; NIC
acceptance counters distinguish accepted backlog from IRQ notification and
receiver drops; repeated harness invocations preserve earlier evidence.
Final rereview found no actionable issues.

`sim/acceptance.sh portable` passed on Linux and macOS. The macOS report explicitly
skips Linux-only VMM compilation. Seedbox Clippy passed with warnings denied,
and the static guest supervisors compiled with `-Wall -Wextra -Werror`.
The five synthetic native tests pass, including actual server/client restart
behavior and report-preservation regressions. Seedbox formatting and diff
whitespace checks pass; the VMM-wide formatting check still reports pre-existing
formatting in `machine.rs` and `undo.rs`.

An earlier engine snapshot also passed the existing full suite, retained as
`existing-full-r2.json` in the isolated checkout. That report includes three
successful standard retries after soft-stop overshoots and a skipped cross-build
comparison. It predates the final NIC counter correction and is supplementary;
the synthetic results above qualify the final runtime behavior.

This acceptance covers process lifetimes and network cuts with one pinned
synthetic workload. Storage survives process restart within a guest; whole-machine
reboot and persistent block-device recovery remain separate work.
