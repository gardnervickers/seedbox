# Automatic application progress

The [determinism catalogue](determinism-catalogue.md) explains virtual clocks,
automatic application progress, kernel polling and ARM software reservations.
This guide records their runtime integration and validation evidence.

Seedbox has one engine mode. Applications run under the translator without
recompilation or application-specific flags, environment overrides or
`GOMAXPROCS` settings.

## Mechanism

`nix/progress.nix` packages DynamoRIO, drmgr, drreg, the client and a private
launcher libc. The archive is retained with the prepared inputs and mounted
read-only under `/seedbox/progress`. The supervisor (`guest/compose-init.c`)
resolves ordinary ELF and script entrypoints and runs them under `drrun` with
`-opt_speed -disable_traces` and their original arguments.

The kernel charges each syscall; arm64 adds 1,000 bounded clock reads at syscall
entry. See [application progress](determinism-catalogue.md#progress-from-translated-blocks),
[kernel polling coverage](determinism-catalogue.md#progress-inside-kernel-polling),
and [watchdog behavior](determinism-catalogue.md#observation-without-guest-progress)
for the time model and uncovered loops.

## ARM software reservations

The [reservation protocol](determinism-catalogue.md#software-exclusive-reservations)
is documented in the catalogue, including epoch publication, commit retries,
fault restoration and the single-CPU assumption.

`nix/dynamorio.nix` builds the ARM core and register-management extensions with
`guest/progress/dynamorio-exclusives.patch`; x86 uses the upstream SDK core.
The supervisor enables `-ldstex2cas` on ARM. Packing and the client's block-local
retry exclusions are no longer used: every application block is charged,
including split exclusive sequences and polling loops.

[The exclusive acceptance workload](../harness/acceptance/workloads/exclusives.yaml)
exercises a weak-CAS counter on both architectures and ARM-specific reservation
and fault cases. Acceptance runs it twice, compares full state, then replays it.
The [assembly fixtures](../guest/progress/fixtures/) include those cases; their
older `run.sh` remains a native upstream packing diagnostic, not the acceptance
gate for the protocol. The investigation is tracked in
[#241](https://github.com/gardnervickers/seedbox/issues/241).

## Original reproducer and baseline evidence

This fixture removes Node, JIT and threads from the question. It increments one
integer with `LDXR`/`STXR` and counts failed stores; the conditional branch
between the load and the store is what exposed block splitting.

```c
#include <stdint.h>
#include <stdio.h>
int main(void) {
    uint64_t value = 0, failures = 0;
    for (uint64_t i = 0; i < 100000000; ++i) {
        uint64_t loaded;
        unsigned failed;
        do {
            __asm__ volatile("ldxr %0, [%2]\n\tcbnz %0, 1f\n\tnop\n1:\tadd %0, %0, #1\n\tstxr %w1, %0, [%2]"
                : "=&r"(loaded), "=&r"(failed) : "r"(&value) : "memory");
            failures += failed;
        } while (failed);
    }
    printf("value=%llu failures=%llu\n", (unsigned long long)value,
           (unsigned long long)failures);
}
```

Measured on an Apple Silicon host, one guest CPU, 1 GiB, seed 1009, before the
earlier counting mitigation:

| Execution | Run failures | Replay failures | Replay |
| --- | ---: | ---: | --- |
| Instrumented, ordinary block splitting | 15,017 | 15,012 | MISMATCH |
| Instrumented, `-unsafe_build_ldstex` | 12 | 3 | MISMATCH |
| Native, no DynamoRIO, same kernel | 18 | 13 | MISMATCH |

Both runs reached 100,000,000. The split-block pair differed at the same
virtual time (37,377,816 ns), so the difference is observable guest behaviour,
and the native pair establishes that observable failures also occur without
the translator. Packing greatly reduced failures in this example. These
measurements do not establish deterministic accounting for arbitrary sequences.

Other Mac controls from the same series, with unmodified Node 22 Alpine where
Node is named:

| Control | Result |
| --- | --- |
| Static computation with client | Full-state MATCH |
| Spinlock with client | Full-state MATCH; owner wakes while waiter computes |
| Single-block `LDXR`/`STXR` loop with client | MATCH; 100,000,000 increments, zero failures |
| Ordinary Node, native supervisor | Full-state MATCH |
| Ordinary Node, empty DynamoRIO translation | Full-state MATCH |
| Ordinary Node with the counting client, before the rule | Application PASS, replay MISMATCH (RAM, sometimes clock) |
| Default exclusive-to-CAS conversion | Did not finish before the 90-second wall limit |

These are historical controls from the prototype, not validation of every
subsequent client revision. Current PR checks are reported separately.

## x86 evidence

Measured on one Linux x86-64 KVM host, seed 1009. Three etcd 3.5.2 members
with `GOMAXPROCS=4` and eight guest client writers; same kernel, 1 GiB per VM,
1,000 ms watchdog. Engine time excludes preparation.

| Acknowledged writes | Native seconds | Initial client | Optimized client |
| ---: | ---: | ---: | ---: |
| 160 | 1.43 | 8.83 | 4.89 |
| 1,600 | 2.26 | 11.47 | 6.93 |
| 16,000 | 14.29 | 34.05 | 26.25 |

The short row is three-run medians; larger rows are single trials. The
optimized multiplier is 3.43x for short work and 1.84x sustained. Empty
translation took 9.81 s for the short workload, so the counter is not the sole
cost. Optimizations: direct raw-TLS access, no module lookup, and
`-opt_speed -disable_traces` (traces enabled cost 29.02 s for 16,000 writes).

Correctness controls on the same host:

- The etcd executable bytes were unchanged by the wrapper image, and native
  `GOMAXPROCS=4` passed: the historical Go stall was not reproduced.
- Short and sustained etcd runs passed full-state replay, including a watchdog
  change from 1,000 to 200 ms; all console bytes matched.
- An application spinlock with no `PAUSE`, clock read or syscall ran 51,919,559
  spins with zero debug steps and matched; with injected progress disabled it
  stalled until the 20-second wall limit.
- Ordinary Node JIT printed its result with zero steps and matched.
- A guest-controlled etcd pause froze one machine for 278 ms; 4,274 writes were
  verified and replay matched.
- The kernel `cpu_relax` fixture completed with a lock held and zero debug
  steps; a raw `PAUSE` loop still stalled for 20 s, which is why the client,
  not the kernel hook, is the primary progress source.

Retained runs are on the hosts named in the project notes, outside this tree.

### Saved x86 vector-state representation

A single-threaded x86 weak-CAS control completed one million `lock cmpxchg`
operations with zero failures at both 1 ms and 200 ms watchdog settings. Its
execution, clock, registers and output matched, but full RAM differed by one
bit in a kernel task's saved XSAVE header: zero YMM state was marked present
in one run and absent in the other. The 256-byte YMM payload was zero in both.
This is distinct from the observable ARM store-exclusive failures.

The repair and its scope are explained in
[canonical saved vector state](determinism-catalogue.md#canonical-saved-vector-state).

With that repair, four atomic fresh runs matched across alternating watchdog
settings, as did replay. A vector-preservation control passed 4,000 nonzero
checks, 4,000 zero checks and 4,000 signal returns while another process was
runnable; its fresh runs and replay matched. The clock-drift workload also
passed fresh comparison and replay. These controls cover the reproduced
kernel task-save issue, not direct application XSAVE, signal-frame storage
or other state components. Broader coverage remains tracked in
[#242](https://github.com/gardnervickers/seedbox/issues/242).
