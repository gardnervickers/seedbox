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

## ARM software reservations

The ARM guest and translator implement a software exclusive reservation. A host
interrupt can clear the physical exclusive monitor without any observable guest
exception, so passing the hardware `STXR` failure directly to the application
would make weak CAS, retry counters and progress accounting nondeterministic.

`nix/dynamorio.nix` builds the ARM core and register-management extensions with
`guest/progress/dynamorio-exclusives.patch`; x86 continues to use the upstream
SDK core. The supervisor enables `-ldstex2cas` on ARM. Packing and the client's
block-local retry exclusions are no longer used: every application block is
charged, including split exclusive sequences and polling loops.

The protocol relies on the existing single guest CPU:

1. The kernel advances an epoch at every return to EL0, after signal/scheduling
   work and before restoring user registers. It clears the physical monitor and
   publishes the epoch in `TPIDRRO_EL0`. The epoch lives at byte 40 of the shared
   PV page; the stopped VMM also advances it at semantic exits and before device
   delivery, including delivery that can partially modify memory before failing.
   Host interruptions, ordinary publication and backend debug-unlock maintenance
   leave it unchanged. VMM updates of `TPIDRRO_EL0` are restricted to stopped EL0;
   kernel entry code can continue to use that register as scratch at EL1.
2. A translated exclusive load records the epoch before loading, together with
   its address, value and size. The same-block optimization is disabled because
   a guest exception can interrupt even a single translated block. Overlapping
   address/destination registers are saved before the load overwrites them.
3. A translated exclusive store checks address and size and consumes the saved
   reservation. Its commit loop takes a physical exclusive reservation, compares
   the value, reads the live epoch, and attempts the physical exclusive store.
   Epoch validation is inside every physical attempt, closing the race between
   the check and the store. There are no intervening memory accesses inside the
   physical pair. A changed epoch or value fails the application store. A lost
   physical reservation with unchanged guest state retries internally, below
   application control flow and progress accounting. This retry loop is
   deliberately unbounded: exposing failure after a fixed number of physical
   retries would make application behavior depend on host interruptions again.
4. `CLREX` invalidates the software reservation. Every attempted store consumes
   it, including mismatched stores. `WZR` status uses a private scratch register
   so physical retries still complete deterministically. Fault reconstruction
   preserves the original full status register, other borrowed registers and
   the application PC. Successful stores retain their actual zero/one status.

Fault restoration also bounds drreg's TLS spill-slot lookup to the public slot
range. Upstream's out-of-range arithmetic can mistake private translator slots
for public client slots and overwrite correctly restored application registers.
The paired-store fault regression exercises this interaction with the counting
client enabled.

The translator virtualizes application reads of `TPIDRRO_EL0` to Linux's native
64-bit value of zero; applications do not see the epoch or supply any settings.
The protocol covers translated shared libraries and JIT code as well as static
binaries. A progress syscall between application load/store instructions is a
real guest boundary and may cause a deterministic failed store. The aim is
repeatable execution, not a promise that applications observe zero retries.

The kernel's two futex LL/SC helpers retry physical failures locally rather than
exhausting a 128-attempt budget and reaching `-EAGAIN`/`cond_resched`. Fault fixups,
value-mismatch exits and memory barriers remain intact. The audited native
DynamoRIO atomics and enabled ARM kernel atomic helpers likewise retry physical
failures internally; they are outside application block counting.

`harness/acceptance/workloads/exclusives.yaml` exercises a weak-CAS counter on
both architectures. On ARM it also checks all exclusive widths, pairs, address
overlap, consumed reservations, `CLREX`, syscall/ABA invalidation, discarded
loads/status, stack and reserved-register operands, and signal contexts for
faulting single/pair stores. Normal acceptance
runs it twice and compares full state, then replays it. The assembly fixtures
live under `guest/progress/fixtures/`. The older `run.sh` remains a native
upstream packing diagnostic, not the acceptance gate for this protocol.

The investigation is tracked in [#241](https://github.com/gardnervickers/seedbox/issues/241).
Kernel polling that never reaches a PV operation can still stall virtual time;
this reservation protocol does not add kernel progress instrumentation.

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

The guest kernel now canonicalizes YMM after successful task-state saves.
It zeroes ignored payload for an absent component and clears the presence bit
for a present all-zero component. Nonzero state is preserved. Full-state
comparison remains unchanged.

With that repair, four atomic fresh runs matched across alternating watchdog
settings, as did replay. A vector-preservation control passed 4,000 nonzero
checks, 4,000 zero checks and 4,000 signal returns while another process was
runnable; its fresh runs and replay matched. The clock-drift workload also
passed fresh comparison and replay. These controls cover the reproduced
kernel task-save issue, not direct application XSAVE, signal-frame storage
or other state components. Broader coverage remains tracked in
[#242](https://github.com/gardnervickers/seedbox/issues/242).
