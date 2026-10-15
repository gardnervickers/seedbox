# Measurements: `bash -c true` under DR, AArch64 VM (Fedora 44, 8 vCPU, idle)

Release DR built from master 25842b959 (`~/w/btm25r`, source `~/w/wtm25` verified by md5
against the host tree).  Every run: `taskset -c 0 drrun -opt_speed -disable_traces
-ldstex2cas -no_hw_cache_consistency -fake_ctr_dic [-c CLIENT --instructions-per-progress-call
50000 --progress-points-per-block 1] -- bash -c true`, 25 runs per cell, two passes.
Timing by `date +%s%N` around each run (wall clock, includes drrun's own exec and the
process exit).  Four cells (marked *) were hit by a VM clock step (negative minimum);
their means are discarded and the other pass is used.  5214 blocks are built.

Variant clients: `$SP/lean-client/variants/lean_variant.c` (built outside both repos,
against the VM's release DR; `VARIANT` selects the behaviour).

| variant | what it does | pass 1 mean/min (ms) | pass 2 mean/min (ms) | peak fcache (KB) |
|---|---|---|---|---|
| native | no DR | 1.16 / 1.04 | 1.12 / 1.00 | |
| bare DR | no client | 19.8 / 18.7 | * | 628 |
| v1 | client loaded, no events | 20.5 / 19.1 | 20.0 / 17.2 | |
| v2 | empty `dr_register_bb_event` | * | 21.2 / 20.1 | 628 |
| v3 | drmgr, empty app2app/analysis/insertion | 22.9 / 21.6 | 23.1 / 21.1 | |
| v4 | drmgr + drreg init, empty events | 49.4 / 43.7 | 45.1 / 43.6 | 628 |
| v5 | current client, charge only, no clean call | 56.9 / 53.9 | 55.5 / 52.0 | |
| v6 | current client (= a64_progress_like) | 125.7 (outlier) / 81.8 | 81.3 / 79.5 | 1532 |
| v7 | v6 + DR_CLEANCALL_ALWAYS_OUT_OF_LINE | 101.3 (outlier) / 81.6 | 82.2 / 80.1 | |
| v8 | **A**: no drreg, raw-TLS spill of x1, same clean call | * | 55.5 / 53.8 | 1524 |
| v9 | **B**: no drreg, raw-TLS spill, shared stub (mov-imm + blr) | 37.3 / 35.8 | * | 932 |
| v10 | B without drmgr (plain bb event) | 36.2 / 33.6 | 34.9 / 33.3 | 932 |
| v11 | fast path only (spill, ldr, sub, str, tbz, restore; no slow path) | 30.1 / 28.1 | 29.1 / 27.9 | 752 |
| v12 | B with a direct `bl stub` (reachable in this layout, not guaranteed) | 34.6 / 33.3 | 34.0 / 31.5 | |
| v6 `-opt_cleancall 0` | | 82.9 / 80.3 | 85.7 / 80.2 | |
| v6 budget 1000 | | 84.5 / 80.8 | 93.1 / 89.9 | |
| v9 budget 1000 | | 36.3 / 35.0 | 41.2 / 39.3 | |

Refill counts are identical across v6..v12 for `bash -c 'echo hello'` (22 refills at
budget 50000): the charges are the same.

Derived per-block attribution (5214 blocks, over bare DR's 19.8 ms):

| cost | ms | source |
|---|---|---|
| client library loaded | ~0.5 | |
| a bb hook exists: full decode + translation recording | ~1.3 | core/arch/interp.c:5286-5321 |
| drmgr's phases and per-instruction dispatch | ~2 | ext/drmgr/drmgr.c:990-1135, 1248-1300 |
| drreg's per-block liveness analysis, even with nothing reserved | ~22-26 | ext/drreg/drreg.c:367-462 (31 GPRs x every instr, reverse scan, drvector per reg) |
| charge insertion with drreg reserve/get_app_value/unreserve (6 instrs) | ~8-10 | drreg.c reserve/restore paths, ff2e7b30d/9961d0a marks |
| the clean call's inline skeleton (38 instrs, out-of-line context switch) | ~25-26 | core/lib/instrument.c:5261-5420, core/arch/mangle_shared.c:193-445 |
| **total** | **~61** | |

Rule of thumb from v11 -> v9 -> v6: each meta instruction emitted per block costs
~0.7-0.8 ms over 5214 blocks (~140-150 ns per instruction per block: instr creation,
encoding, translation bookkeeping, cache space).

Exact sequences (A64P_DUMP=1; full dumps in `$SP/lean-client/dumps.log`): the current
client emits 44 meta instructions (176 bytes) per charge point; A emits 44; B emits 15
(60 bytes); the fast path executed in every variant is the same 6 instructions.

## Seedbox's real client, old (5ebddb73) vs new (4bbb627e, branch claude/lean-progress)

Copies of `guest/progress/progress.c` + `budget.h` built outside both repos
(`$SP/lean-client/seedbox-build/`, `DynamoRIO_USE_LIBC OFF`, drreg only for the old one)
against the same release DR; same command line and method as above (N=25, two passes;
the script skips a clock-glitched sample without renormalizing, so trust the minima and
pass 1).

| client | pass 1 mean/min (ms) | pass 2 mean/min (ms) | peak fcache (KB) |
|---|---|---|---|
| bare DR | 20.7 / 19.7 | 20.3 / 19.6 | |
| sb_old (drreg + per-block clean call) | 84.3 / 81.6 | 80.5 / 81.5 | 1532 |
| sb_new (fixed x1, TLS resume/charge, shared routine via `br`) | 37.6 / 36.4 | 39.3 / 36.6 | 952 |

Speed-up for `bash -c true`: 2.2x (overhead over bare DR 61 -> 17 ms).  Output check:
`bash -c 'echo hello; seq 1 3 | wc -l'` at budget 1000, 3 points per block, is correct.
The real client's progress syscall (getpid with the mark) runs on the VM's stock kernel.
