# Execution-performance investigation: agent handoff

## Read this first

This branch is a **preserved investigation snapshot, not an accepted runtime
optimization**. The user requested this handoff, preservation of the code, a
branch push and cleanup. Do not treat the snapshot merge as tested integration.
No new builds, guest runs or tests were authorized for consolidation.

The immediate unresolved question is **which operation dominates real Java
HOST wall time**. We have a trustworthy indexed-mode regression, but not an
operation-level profile. Do not start another broad implementation from a
source-level hunch or a toy benchmark.

- Original Kafka performance goal is **paused**. Do not restart it without the
  user explicitly resuming that goal.
- Further implementation/testing is on hold. Architecture/research was allowed
  to continue; a proposal is not authorization to change the runtime or model.
- Agent configuration: **Codex Astra, high**, for architecture/ideas/review;
  **GPT-6.1-Sol, medium**, for implementation/profiling. One measurement/build
  owner; parallel writers need independent worktrees and exclusive ownership.
- Independent review was deferred to the end. This branch has not received
  final independent acceptance. No PR or merge is authorized by this handoff.

## Product premises and target

Seedbox is a utility for users testing their own programs, **not a hostile-code
sandbox**. Deliberate attempts to bypass code publication through proc-memory
injection/ptrace need not drive anti-circumvention architecture. This does not
permit classifying ordinary application bugs as attacks: preserve normal JIT
configurations, mappings/COW, threads, signals, syscall effects and ARM cache
publication. No application allowlists, JVM native escapes or tuning flags as
engine fixes.

Each machine has **one vCPU**, not one Linux thread. JVM compilation/GC threads
and parked syscall/signal contexts still exist. Multiple machines can execute
on host workers concurrently. A future architecture should make one-vCPU
ownership load-bearing, not simply retain a general N-CPU rendezvous.

Original target: unchanged three-broker Kafka, normal tiered JIT, at least two
quiet-host oracle PASS runs at >=0.8 simulated seconds/execution-wall second;
stretch >1.0. Baseline was ~0.149x. Required gates include correct flush-while-
parked PC/registers, no JVM fatal errors, replay MATCH and 1-vs-4-worker invariant
results. Fixed useful-work wall throughput must be reported separately from
simulated-time changes. No budget inflation or fabricated charges to improve S/W.

HVF on the tested M4 Max has **no usable guest PMU instruction counter**: tested
EL2-off traps / EL2-on zero counters. Do not assume a hardware-counter solution
or repeat that campaign without new evidence. Syscall-free loops must still
produce deterministic bounded PV returns; host timers/watchdogs are not a
semantic instruction clock. Software-exclusive guest epochs must remain
independent of host interruptions.

## What this branch contains

The DR path was based on `23e651fd` (merged #307–#310). DR checkpoint
`f8670781` is the measured source reference. Custom-executor checkpoint
`25bd4569` was merged as an **experimental code preservation** step after the
user stopped testing; the consolidated tree itself has not been built/tested.

| Owner | Location / status |
|---|---|
| DR client and packaging | `guest/progress/progress.c`, `nix/{dynamorio,progress}.nix` |
| IC dependencies/index | `guest/progress/dynamorio-icache-{images,index}.patch` |
| Private context ownership | `dynamorio-{predicate-wait,predicate-wait-end,owned-progress}.patch` |
| Ordinary ARM fixes | IC ranges, inactive SME header, stored-translation Debug checker, TLS permission query patches in `guest/progress` |
| Executable DR regressions | `guest/progress/fixtures/`; retained raw unannotated negative plus annotated wait/progress/application-return tests |
| Actual application fixtures | `guest/{java-jit,alias-jit}/` |
| HVF integration tests | `crates/seedbox-arm64/tests/{parked_guest,progress_guest,executor_proof}.rs` |
| Purpose-built executor | `crates/seedbox-executor/`: ARM linked/region/raw-gateway path; x86 remains diagnostic |
| Previously uncommitted code/tools | `tools/perf/handoff/`: Java scripts, unfinished CDS sources, and compressed ledger/translator diagnostic patches; historical reproducer/probes/harness sources also preserved |
| Unaccepted combined experiment | `tools/perf/handoff/patches/blocked-combined-stack.patch.gz`; archival only, do not apply wholesale |

**Index remains opt-in**, removed from the ordinary launch recipe at
`f8670781`. The stored experimental executable still embeds an older ON recipe;
do not mistake it for a build of the current default. Custom executor examples
are not the production guest runtime. CDS files are preserved outside the live
Kafka example so unfinished training is not silently enabled.

## Considered, tried, and disposition

| Approach | Evidence / decision |
|---|---|
| Fix DR parked clean-call return | Original JVM safepoint could receive pc=0 after invalidation. #308 SIMD layout and #309 dispatcher return remain essential; never resume a deleted cache fragment. |
| Sparse metering / PMU | PMU unusable here. Sparse checks without precise exceptional-prefix handling are unsound. Region/path accounting is still a proposal, not blanket permission to skip charge points. |
| DIC masking | Guest CTR DIC=0, IDC=1; DR release exposed `-fake_ctr_dic`. Masking DIC restores application IC publication; 1,000 rewrite probe and replay passed. Not a speedup by itself. |
| Eager page IC invalidation | Removed historical executed-area multiplier, 75–88x fewer flush admissions. Large Hello improvement, but Kafka S/W did not improve; simulated duration changed more than wall. |
| CDS | Native startup ~3s to ~2s; DR runs inconclusive/timeouts. Training source preserved, unfinished, not an engine fix or representative performance result. |
| Combined #314 correctness stack | Extracted range/register/probe ideas, excluded #311 accounting changes. Combined branch remains blocked on signal payload, vfork obligation, blocking cleanup and sighand identity. Patch preserved, not integrated. |
| Custom signal-per-block executor | Proved instruction relocation, metering/PV and pinned invalidation, but signal dispatch was a diagnostic scaffold, not a fast runtime. |
| Custom linked path, first revision | Compute +3.6%, short calls +52.6%, sparse JIT +2.9% vs corrected DR. Seven publications and unused eager append translation were not a meaningful JIT comparison. |
| Return-hint tweak | ~6% faster on a longer-call control but 19% slower on the six-instruction short loop. Leave opt-in; fewer emitted instructions did not predict throughput. |
| Optimized custom region/raw path | Shared scratch and mapped raw payments materially improved supported warm paths; no complete Linux runtime qualification. See numbers below. |
| Precise DR dependencies | Same-line append, aliases, overlapping/elided/cross-page dependencies and reclamation passed. All-image scan was deliberately costly lifetime scaffolding, not final architecture. |
| DR owned continuation repair | Raw unannotated helper wait deadlocked publication; annotated restartable wait, owned finite progress and real read/nested signal return passed. This is not a general arbitrary blocking-helper guarantee. |
| Indexed DR | Small publication benchmark wins, but matched real Java is 2.77x slower. Global mapping incarnation and whole-birth revocation are source-backed suspects, not measured causal attribution. Do not promote. |
| Other libraries | No proven better fit. Frida/Stalker is interesting but full-process/context coverage unproven; QEMU/Valgrind change execution models; QBDI/MAMBO have documented fidelity gaps. |
| Kernel-assisted thin executor | Conditional first-principles architecture preference, not selected/implemented. See design section below. |

#311's clock exemptions, arbitrary translator discount/fractional accounting and
raw-exit relaxation were **not accepted**. Application-specific JIT restrictions
and native `libjvm` escape are ruled out. Deliberate no-IC self/external
`/proc/*/mem` writes demonstrably retain stale code in DR OFF and ON; retain that
unsupported-mechanism evidence, but do not reopen a security-sandbox project.

## Performance numbers and comparability

### Historical Kafka / IC work

| Run / configuration | Simulated s | Execution wall s | S/W |
|---|---:|---:|---:|
| #307–#310 fixed integration `adcb1c4e`, seed1009 |176.7706|1187.5371|0.148855|
| 2ms-link fixed run |170.1866|1021.1232|~0.167|
| DIC-guarded seed1009, host-load contaminated |174.3946|1360.7058|0.128165|
| Eager IC candidate Kafka |67.3596|880.3485|0.076515|

These are historical, not quiet-host qualification. Replay MATCH evidence exists
for the noted fixed/guard/IC runs, but not every comparison used full-state
mode. The earlier ledger found ~98.3% wall inside guest HVF residence; that is
**not an operation attribution**. No equivalent native three-broker baseline
fits the shared 2GiB Podman VM; W/N and S/N remain unavailable.

Eager IC Hello medians: DR-only ~9.99s to ~0.62s; progress client ~22.45s to
~2.59s. Do not benchmark future work against that obsolete pathological baseline.

### Custom executor: fixed useful work, supported finite ARM slice

Frozen optimized checkpoint `25bd4569`; corrected IC DR plus conservative
scratch-restoring client was the reference. Median milliseconds per repetition:

| Shape | Native unmetered | Corrected DR | Old custom A | Optimized custom |
|---|---:|---:|---:|---:|
| Compute, 8 batches |470.178|629.432|655.782|621.631|
| Six-instruction short calls, 32 batches |228.940|877.522|1410.716|743.726|
| 17-instruction calls, 16 batches |450.396|1302.132|1157.253|601.621|
| Old seven-write JIT, 8 batches |473.397|649.877|669.856|639.493|

Optimized short takes 15.2% less wall than DR and 47.3% less than A; long-call
wall is 53.8% less than DR. Compute ~1% difference is noise-scale. Five rotated
repetitions, shared host. These are not native-overhead or whole-JVM claims.
DR short charges 4+2 versus custom 2+2+2; driver/completion/kernel charges differ.
No cross-engine charge-prefix, schedule or replay equivalence is established.

Matched *executed* append/patch microfixtures preserved hot neighbors. After
subtracting corresponding no-publication controls, optimized custom saved
~26–33 microseconds per append/recompile and ~41 microseconds per four-byte
patch vs DR. Driver/fault/target-distribution differences remain: these are not
isolated compiler latencies or a measured JVM crossover.

### DR indexed release microbenchmark

Five rotations, 512,000 calls and 2,000 publications/sample, same progress client:
append OFF150.28ms / ON74.79ms (~2.0x); patch OFF150.40ms / ON85.79ms (~1.75x).
Whole process timing includes startup/driver/completion and predates the final
HINT/predicate capability fixes. Not the final-source application denominator.

### Definitive frozen Java/JFR comparison

The later profiling lane resolved historical artifact/oracle/epoch confounds.
Same signed engine, kernel, DR core, progress client, Java and oracle payloads;
ON changed only the init entry to add `-seedbox_icache_images` in BOTH guests.
Seed704, epoch1791839201, 1GHz, zero jitter, quantum50,000, normal software
exclusives, Java1536MiB/oracle384MiB, 1ms network, workers1. Java argv remains
`java -cp /workload Workload` with normal tiered JIT. Five rotated pairs:

| Pair | OFF execution wall s | ON execution wall s |
|---|---:|---:|
|0|31.214491|100.701898|
|1|32.805102|88.001851|
|2|33.215818|92.117622|
|3|33.438808|88.113364|
|4|34.657897|98.541824|
|**Median**|**33.215818**|**92.117622**|

**All ten PASS; ON/OFF=2.773.** Representative replays `nx7wq5z8` and
`g27vwh5s` MATCH outcome/witness/fullState/faultHistory. Actual core hash
`83c041…`, progress `bff14f…`, Java image `16071b…`, oracle `278f71…`.
Full identities and observations are in archived evidence, not runtime pins.
Compact results: `tools/perf/handoff/java-cost/results.json`.

End simulated time repeats13.607700391s OFF /5.258700391s ON. Do not interpret
less simulated duration as better performance: same useful results, worse wall.
The earlier33.34s/86.87s observations were **unmatched** (core/oracle/epoch
revisions differed); this new matrix independently confirms the regression.

HOST log receipt medians, not calibrated exclusive CPU timers:

| Phase proxy | OFF s | ON s |
|---|---:|---:|
|Oracle waiting to Java/JFR RUNNING|24.478|68.820|
|RUNNING to final checksum|1.531|5.196|
|Final checksum to JFR STOPPED (GC/dump/control)|2.503|7.624|

Scheduler `machine_s`31.553/90.415 includes both machines, guest/kernel/backend
and waits; coordinator ~0.031/~0.033. Nested scheduler timers are not additive.
Most elapsed time is readiness, **not an identified expensive operation**.
All machine-run wall remains unknown at operation level. No Java-wide IC,
retranslation, due-payment, revocation or exclusive HOST-time samples exist.

## Correctness evidence: keep artifact generations distinct

- Before the last capability fix: public Java/JFR `2rgqx3gs` → replay
  `6vw3zqdd`, alias `xdvfepvm` → `6k23ts92`, exclusive suite `aag9bzgk` →
  `bexjwrmv`: PASS/MATCH, all four fields including full state.
- Same earlier artifacts: private complete-CLI 1/4 workers matched witnesses,
  CPU/RAM digests, effects and committed times against public source. Java
  10,564 assignments/5,321 rounds/1,729 migrations; alias1,292/705/93. Actual
  two-machine overlap; public server does not expose this worker setting.
- Latest baseline profiling freezes the corrected final core and supplies final
  within-mode Java PASS/MATCH. It is not a blanket final-core rerun of every
  earlier alias/exclusive/worker gate.
- Bounded Linux ownership discriminator: three positive cases × OFF/ON ×
  normal/diagnostic ×20 =240 completed iterations. Exact polls/full registers,
  signal payload/masks, read effects, old charge5 versus new7, alternating1/3
  payments and one old refund. Raw unannotated wait still hangs as expected.
- Real-PV integrated gate found a stale predicate valid-context capability after
  a signal abandoned the helper stack; it projected a cache PC. The targeted
  capability-end fix passed dedicated real-kernel/software-exclusive guests.
  Twenty polls per case, real horizons10/12/10, twice each, equal full state.
  Their progress binary differs from the latest archive; retain exact scope.
- Syscall-free loop: seven uneven real `MachineStop::Horizon` boundaries, twice,
  zero overshoot and equal evidence. That archive predates the last capability
  cleanup. Callback counts are not Horizon counts.
- Prior portable151.1s and mac-hvf passed; core/fixture tests and zero-fuzz patch
  checks passed on recorded checkpoints. No x86 KVM/Nixos qualification.
- **No tests/builds were rerun after snapshot consolidation.**

Ordinary repaired issues: TLS exit probe fault, legal inactive SME-only host
signal header, singleton stored-translation Debug lookahead, zero-length
HotSpot madvise probe, legal PC-independent HINT raw encoding, predicate context
capability lifetime. The SME fix does not qualify active SME/SVE state.

## What to do next, after the user permits testing

1. **Do not promote the index.** Recovering92s to33s is not the requested sizeable
   improvement over baseline. Global mutation incarnation, whole-birth
   revocation and global post-sync selection loops are hypotheses to measure.
2. Obtain the smallest trustworthy HOST observation that can distinguish the
   baseline's expensive operation. Existing host stacks see opaque
   `hv_vcpu_run`; guest `scan_us`/`query_time_micros` are PV time, not HOST wall.
3. Candidate: low-rate host-only stopped-HVF PC/PSTATE/address-space sampler.
   Apple `hv_vcpus_exit` can force observation; ARM currently has no existing
   watchdog sampler. CANCELED/Interrupted behavior must be audited so an
   observation does not publish a round, inject IRQs, adopt guest time or
   advance exclusive epoch. Map acquisition is a feasibility gate, not a
   hidden new kernel/DR instrumentation project.
4. Start with validated kernel/native DR/generated-cache ranges, task/mm
   identities and UNKNOWN. Cache samples cannot initially split application
   instructions from meter/IBL. Reused/ambiguous ranges stay UNKNOWN. Measure
   request/stop latency, sampling coverage/overhead, rate effects, replay and
   Horizon invariance; no claim of precise operation timing from raw counts.
5. If HVF map acquisition is too invasive, a native ARM Linux DR profile may
   identify source hot functions, but its percentages do not transfer to HVF.
6. Identify the most expensive **materially removable operation by total wall
   cost**, reproduce JUST that mechanism with its safety/one-vCPU/alias/context
   conditions, test a solution there, then verify real-workload improvement.
   Do not strip the costly correctness contract out of the reproducer.

### Architecture work remains open, independently of the profiler

Astra's conditional first-principles preference: a thin same-ISA executor with
**kernel-owned logical application contexts** on BOTH entry and return. Linux
parks original PCs/registers and exposes original signal frames; one vCPU can
then exclude EL0 once at publication instead of suspending every task. Own
private committed effects explicitly; resolve current code before all reentry.

Potential cheaper metering: register-carried budget, bounded regions/path
potentials, reconstruction of exceptional prefixes, near-threshold slow path.
Preserving existing logical precharge points can remove physical bookkeeping;
exact completed-instruction accounting would change faults/interleavings/jitter
and requires an explicit model decision. No larger quantum or invented S/W.

Consider publication journals only with consumer-before-writer-ISB coverage,
self-modifying fallthrough handling and precise interrupted commit states.
Mapping/backing identities need lifetimes; pins retain maps, not execution
permission. Maintenance traps must not silently increment guest-exclusive
return epochs. Canonicalize before syscall restart/signal/rseq consumers, not
just on EL0 return; nested contexts and edited sigreturn PCs are mandatory.

Alternatives: kernel-assisted DR core fork (mature coverage but possibly retains
both costs), native/in-place metering islands (interior targets/aliases/self-
reading/JIT publication tend to require shadow code anyway). No architecture
was selected, no kernel ABI/model change approved, no numerical sizeable Java
speedup or near-real-time forecast supported by the profile. Astra should keep
thinking; the next empirical implementation follows the bottleneck evidence.

## Restart, artifacts and cleanup

Build commands in AGENTS.md remain authoritative: project `nix develop`,
`cargo xtask build-assets`, complete `cargo build --release`; retained kernels
can avoid an unnecessary kernel rebuild. Libraries never build at runtime.
Before experimenting, read the specific scripts: archived profiling tools have
absolute paths and require companion inputs, signed executable and private0700
state/cache directories. They are historical scripts, not drop-in public APIs.
Normal public run/replay uses an explicitly started owned server. Save exports
before deleting runs; never remove a checkpoint source still needed by children.

**External local evidence archive** (not pushed to Git):
`/Users/gardnervickers/.pi/agent/evidence/seedbox-performance-2026-10-12/`.
The `MANIFEST.md` there records original paths and retained bundles/reports.
It preserves raw measurements, exact signed engines/helpers/kernel, public
exports, PV initrds, historical investigations and complete pre-cleanup Git
bundles. Those bundles also preserve unaccepted branch histories without
applying them to this branch. Another host must request that archive;
Git source/doc alone does not reproduce historical binary inputs. Sim IDs above
are historical labels, not live catalogue resources. No old catalogue reload.

Historical reports: `DR-GUEST-E2E.md`, `BASELINE-COST.md`, `MECHANISM-MAP.md`,
`OPTIMIZED-EXECUTOR.md`, `PARKED-DISCRIMINATOR.md`,
`OPTIMAL-EXECUTION-DESIGN.md`, alternative-library and Kafka/IC reports. Handoff
numbers distinguish their artifact generations; the raw archive is supporting
material, not independent final acceptance.

Cleanup scope: owned servers/simulations/build/probe containers and finished
investigation worktrees only, after source and evidence preservation and push.
Keep this pushed branch checked out for the next agent. Do not change shared
Podman VM/store/images, unrelated worktrees or human Git credentials. Local
`.pi` agent settings/session records are not code and are excluded from Git.

Host constraints at last checkpoint: Mac M4 Max, shared Linux builder2GiB/7CPUs,
~6.1GiB free of100GiB in its store. One build job after prior OOM; no overlapping
builds/timings. No resize/restart/shared GC without approval. **Nixos is off
limits/offline**; do not act on older SSH permission. Use `robo-git`/`robo-gh`
for repository operations; report missing provisioning, never human fallback.
