# Automatic application progress

Seedbox has one engine mode. Guest time is a PV clock: compute between exits
costs no virtual time, and time advances at guest exits, syscalls and clock
reads. Application code that computes without exiting would never advance the
clock, so every application process runs under a binary translator that
charges progress for it. Nothing is recompiled, and no application flags,
environment overrides or `GOMAXPROCS` settings are required or applied.

## Mechanism

`guest/progress/progress.c` is a DynamoRIO 11.3.0 client. It keeps a per-thread
counter in raw TLS and decrements it at the entry of each translated basic
block, including shared libraries and JIT code. Every 10,000 block entries it
issues one raw `getpid` syscall, which the kernel charges as PV progress like
any other syscall. This counts block entries, not retired instructions; the
translator's own code and the guest kernel are outside the client's coverage.
x86 preserves flags through drreg; ARM uses non-flag-setting arithmetic and
`CBNZ` with a reserved scratch register.

`nix/progress.nix` packages DynamoRIO, drmgr, drreg, the client and a private
launcher libc. The archive is retained with the prepared inputs and mounted
read-only under `/seedbox/progress`. The supervisor (`guest/compose-init.c`)
resolves ordinary ELF and script entrypoints and runs them under `drrun` with
`-opt_speed -disable_traces` and their original arguments.

The guest kernel covers what the client cannot: the existing syscall hook
charges each syscall, the x86 `cpu_relax` hook charges bounded clock reads in
kernel polling loops, and the arm64 kernel charges 1,000 bounded clock reads at
syscall entry. A loop none of these cover never exits and stalls virtual time
until the world's wall-time limit; the host watchdog (`--watchdog-ms`)
interrupts such a run to report on stderr where the guest is stuck, then
resumes it, so it changes only how soon the report appears, never a result.

## ARM limitations and partial mitigation

**General ARM determinism is not established.** A single guest CPU does not
prevent host interruptions from causing spurious `STXR` failures. An application
can observe those failures directly through an LL/SC retry counter, weak CAS or
backoff. Even an otherwise invisible retry can change the client's TLS budget
and the placement of progress charges. The translator's own code and kernel
LL/SC sequences also remain outside the client's coverage.

The supervisor uses `-no_ldstex2cas -unsafe_build_ldstex`. Packing keeps supported
exclusive sequences together and reduces instrumentation-induced failures.
DynamoRIO cannot pack every sequence: PC-relative instructions, long regions and
register pressure can prevent it. Its alternative exclusive-to-CAS conversion
remains disabled because of correctness problems described below.

The client applies a limited, block-local mitigation:

- It skips the entry charge when a block starts with an exclusive load or a
  packed `ldstex` instruction, or contains a store-exclusive or `CLREX` before
  any exclusive load. Ordinary successor blocks retain their entry charge.
- If an unpacked load-starting block branches backward before any exclusive
  store, it charges the taken branch. That allows simple abandoned-reservation
  polls to advance time.
- A packed poll may loop internally without a charge. The client reports
  recognized cases on stderr, but cannot make the owner wake on virtual time.

This is not a control-flow proof. An intermediate block in a split sequence,
retry work outside the recognized blocks, or a different retry target can still
change accounting or insert instrumentation inside a reservation. Looking back
sixteen adjacent instructions cannot establish the incoming control flow, so
that heuristic has been removed. Neither a successful workload nor equal syscall
counts proves these cases safe. A loop outside progress coverage can stall until
the wall-time limit; the diagnostic watchdog does not advance guest time.

The fixtures under `guest/progress/fixtures/` are native ARM Linux diagnostics
for packed/unpacked polls, split sequences and observable weak-CAS failures.
They do not assert replay determinism. In particular, their native owner thread
can wake on host time even when a Seedbox guest owner would remain asleep.

## Independent ARM follow-up

Tracked in [#241](https://github.com/gardnervickers/seedbox/issues/241).

Two concrete DynamoRIO 11.3.0-1 conversion problems have been reproduced on
Apple Silicon using minimal static ARM programs:

1. For an overlapping load such as `LDAXR W3, [X3]`, the converter saves the
   reservation address after the load overwrites X3. Static musl's exit lock
   uses this shape and retries forever. Saving the address before the load
   fixes the minimal reproducer and lets the static-musl program finish. A
   same-compiler rebuild without the patch still hangs.
2. After a successful store-exclusive, a second store-exclusive without a new
   load must fail. The converter can reuse its saved reservation and succeed.
   The minimal test passes natively and fails under stock conversion.

The first repair is small and confirmed; it is not a general determinism fix.
Value comparison alone also cannot represent reservation loss across a guest
interrupt or an ABA write. A software reservation must be consumed, honor
`CLREX`, and be invalidated at the relevant guest exception and scheduling
boundaries. Checking an epoch before a CAS has a race if the guest switches
between the check and the store. The eventual commit operation must close that
race, preserve fault behavior and memory ordering, and hide host retries from
both the application and the progress budget.

Controlling the guest kernel gives us places to implement that contract without
translating the entire kernel. A kernel exception/return hook plus a protected
commit sequence is a candidate, not yet a validated design. Linux 6.12.112 also
contains explicit LL/SC in `arch/arm64/include/asm/futex.h`, including bounded
retries that can return `-EAGAIN`; enabling LSE alone does not replace that inline
assembly. Kernel coverage and translator-internal atomics need separate audits.
No application-specific flags or environment changes are an acceptable fix.

## Reproducer

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
