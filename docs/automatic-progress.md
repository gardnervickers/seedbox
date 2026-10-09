# Automatic PV progress: draft migration and ARM blocker

This draft prepares the move from PMU branch counting to PV with automatic binary
instrumentation. **Do not merge yet.** ARM exclusive-store retries are not
reproducible, ordinary instrumented Node fails full-state replay, and internal
counter removal and final Linux integration validation are unfinished.

## Contract and implementation

Existing images must run with their normal arguments and environment. No
recompilation, `GOMAXPROCS=1`, V8 predictability flags, disabled JIT, or
application-specific recipes may be required. The supervisor's old implicit
`GOMAXPROCS` override is removed in this draft. Explicit image/Compose settings
are preserved.

`guest/progress/` contains the production candidate client. DynamoRIO inserts a
per-thread raw-TLS decrement at each translated block entry, including libraries
and JIT code. Every 10,000 entries a clean call issues a raw getpid syscall to
charge PV time. This is block progress, not instruction-exact counting; translator
and kernel instructions are outside client coverage. x86 preserves flags through
drreg; ARM uses non-flag-setting arithmetic and CBNZ with a reserved scratch GPR.

`nix/progress.nix` packages DynamoRIO 11.3.0-1, drmgr/drreg, the client and a private
launcher libc. The archive is retained with prepared inputs and mounted read-only
under `/seedbox/progress`. The supervisor resolves ordinary ELF/script entrypoints
and passes their original arguments to drrun. ARM syscall entry charges 1,000 PV
clock reads through the existing bounded rendezvous. Linux's existing PV syscall
hook and the new x86 cpu_relax hook provide kernel progress.

New CLI runs select PV; the public timebase selector is removed. Legacy counter
code, admission paths, tests and documentation still need a separate removal pass.
This is intentionally an incomplete migration, not a claim that PMU removal has
landed. The watchdog CLI fix and compact ARM per-page hashes are included.

## Evidence as of 2026-10-09

Linux prototype results are summarized in
[tools/dbi-spike/RESULTS.md](../tools/dbi-spike/RESULTS.md): unchanged etcd with
GOMAXPROCS=4, ordinary Node, and a pure computation spinlock all matched full state.
Measured sustained etcd overhead was 1.84x native (single trial), with larger
startup overhead. These are prototype results, not final cross-platform acceptance.

Mac/ARM public-CLI controls used one guest CPU, 1 GiB RAM, seed 1009, a fixed epoch,
this draft's syscall-progress kernel, and unmodified Node 22 Alpine unless noted:

| Control | Result |
| --- | --- |
| Static computation with client | Full-state MATCH |
| Spinlock with client | Full-state MATCH; owner wakes while waiter computes |
| Single-block LDXR/STXR loop with client | MATCH; 100,000,000 increments, zero failures |
| Ordinary Node with client | Application PASS, replay MISMATCH; sometimes clock and RAM differ, sometimes RAM only |
| Ordinary Node, new kernel, native supervisor | Full-state MATCH |
| Ordinary Node, empty DynamoRIO translation | Full-state MATCH |
| Node stopped at 1 ms and 10 ms | Full-state MATCH in these trials |
| Node stopped at 30 ms | CPU and RAM MISMATCH |
| Default DynamoRIO ARM exclusive-to-CAS conversion | Did not finish before the 90-second wall limit |
| Node with diagnostic `--predictable` | Still RAM MISMATCH; not a proposed workaround |

The default conversion also hung for a static musl fixture, even with no client
inside the Linux ARM builder. `-no_ldstex2cas` avoids that hang but is not a
correctness solution. `-unsafe_build_ldstex` also did not fix Node replay.

## Minimal exclusive-store reproducer

[arm-exclusive/retry.c](../tools/dbi-spike/arm-exclusive/retry.c) removes Node,
JIT, shared-library concurrency and application threads from the problem. It
increments one local integer with LDXR/STXR and reports failed store attempts.
Adding one conditional branch between the load and store exposes the failure.
Both runs reach 100,000,000; the visible retry counts differ:

| Execution | Run failures | Replay failures | Replay |
| --- | ---: | ---: | --- |
| Instrumented, ordinary block splitting | 15,017 | 15,012 | MISMATCH |
| Instrumented, `-unsafe_build_ldstex` | 12 | 3 | MISMATCH |
| Native, no DynamoRIO, same kernel | 18 | 13 | MISMATCH |

The ordinary instrumented pair printed at the same virtual time (37,377,816 ns).
The discrepancy is observable guest behavior, not merely dead RAM or a hash issue.
The native result disproves an explanation limited to our counter or translator.
It does **not** establish that every Node difference has the same cause.

ARM exclusive stores may fail spuriously. Host interruptions and the inserted
memory accesses are candidate causes, not individually proven by these trials.
DynamoRIO documents the difficulty of instrumenting exclusive regions and warns
that its compare-and-swap conversion changes ABA semantics:
[exclusive monitor instrumentation](https://dynamorio.org/API_BT.html#sec_ldrex).
Simply hiding retry counts, weakening full-state comparison, or requiring app
flags would not satisfy Seedbox's contract.

## Review questions and remaining work

1. How should ARM exclusive-monitor state and failures be made deterministic,
   including sequences spanning blocks, intervening writes, exceptions, context
   switches, and ABA? Is software emulation necessary at the translator or VM
   boundary? Neither native hardware retries nor the tested DR options suffice.
2. Diagnose the static musl hang with DR's default conversion separately. A fix
   must preserve application semantics, not only make this fixture exit.
3. Re-run ordinary Node after resolving exclusive-store behavior; other causes
   of its mismatch remain possible. Preserve normal application flags.
4. Complete PMU/counter removal, update remaining legacy docs/harness suites,
   validate the integrated Linux build and full run/replay matrix, and qualify
   ARM static, dynamic, JIT and multi-thread workloads before merging.

The cleaned draft passes the portable suite, native `mac-hvf` suite, strict
workspace Clippy, ARM supervisor compilation, and the Linux supervisor
process-control test. The integrated Linux VMM and complete Mac application
suite are not claimed as validated.
Temporary full-RAM dumping was removed; only compact per-page hashes remain.
Raw dumps, SDKs, generated contexts and reports stay outside Git. The earlier
34k-line experiment-output tree was archived outside the checkout.

## Retained local evidence

These are local debugging artifacts, not dependencies needed to build the PR.
All pairs below are under `/tmp/seedbox-arm-progress/state/runs/` on the Mac.

| Trial | Run | Replay |
| --- | --- | --- |
| Ordinary Node, RAM capture | bd71dbd9ab35ddb5-g7N00n | bd71dbd9ab35ddb5-nYMTG3 |
| Node, native new kernel | 44990b95a5910446-OAXLcg | 44990b95a5910446-n4NVpA |
| Node, empty translation | 9c0f8f5178496007-j48mFf | 9c0f8f5178496007-EzrTRV |
| Single-block retry probe | 3c19075358750f55-worwsL | 3c19075358750f55-BobsEI |
| Split-block retry probe | 7b8c9e2d7e30fa83-CKvKhE | 7b8c9e2d7e30fa83-SU7Gsx |
| Packed retry probe | ffc35af84f6e6096-R6qsYK | ffc35af84f6e6096-Q9wYdc |
| Native retry probe | 7d25cd32d63b5d02-mWebYg | 7d25cd32d63b5d02-EzaENt |

The fixture build and public run/replay commands are in
[arm-exclusive/README.md](../tools/dbi-spike/arm-exclusive/README.md). The native
control used the supervisor from `42381a5b`; the experimental launchers and raw
console logs are also retained under `/tmp/seedbox-arm-progress/`.
