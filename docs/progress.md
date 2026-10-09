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
syscall entry. The PV watchdog and cycle proof (`docs/progress-proof.md`)
remain as the backstop for loops none of these cover; `--watchdog-ms` changes
only how often the host looks, never a result.

## ARM exclusive sequences

ARM atomics in musl, glibc, Go and V8 are `LDXR`/`STXR` sequences (LSE is
exposed to the guest but libraries still ship the LL/SC paths). The local
exclusive monitor is cleared by every exception return into the guest, so a
`STXR` fails whenever the host entered the VM between the load and the store.
Under HVF that entry is invisible to the VMM: `hv_vcpu_run` returns only for
guest exceptions, cancellation and the virtual timer, so a host interrupt or a
preemption lands at a point no replay can reproduce. The failed store is
retried, and natively the retry is invisible to the program.

Block-entry counting makes it visible. The retried block runs again, shifts the
TLS budget, and eventually moves the `getpid` charge to a different
instruction: RAM differs when only the budget moved, and the clock differs when
a 10,000 boundary moved. Splitting a sequence across blocks is worse, because
each block boundary is a translator exit inside the sequence.

The client therefore follows one rule: **a block entry that a failed exclusive
store can reach is never counted.** On aarch64 the supervisor passes
`-no_ldstex2cas` (DynamoRIO's compare-and-swap conversion changes ABA
semantics and hung static musl binaries) and `-unsafe_build_ldstex`, which
packs each exclusive sequence into one block. The client's analysis pass then
skips the count on any block that contains an exclusive load, `ldstex` macro
instruction, exclusive store or `clrex` unless the exclusive load is the
block's first instruction and so the block can only be entered by starting the
sequence. The progress charge lands on the block the sequence exits to, which
runs exactly once per completed increment regardless of how many retries
preceded it.

The rule does not make the retry count itself reproducible: a program that
prints its own `STXR` failure count still differs between run and replay, as
it does natively. It makes the charge independent of that count.

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
counting rule above:

| Execution | Run failures | Replay failures | Replay |
| --- | ---: | ---: | --- |
| Instrumented, ordinary block splitting | 15,017 | 15,012 | MISMATCH |
| Instrumented, `-unsafe_build_ldstex` | 12 | 3 | MISMATCH |
| Native, no DynamoRIO, same kernel | 18 | 13 | MISMATCH |

Both runs reached 100,000,000. The split-block pair differed at the same
virtual time (37,377,816 ns), so the difference is observable guest behaviour,
and the native pair shows the host interruptions, not the translator, as the
source of the failures. Packing removed the deterministic failures that block
boundaries added; the counting rule removes the dependence on the remaining,
host-caused ones.

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
