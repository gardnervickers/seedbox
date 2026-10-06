# Seedbox evidence log

Updated 2026-10-05. Design and objectives: [CONCLUSIONS.md](CONCLUSIONS.md).
Current work and runtime observations: [STATUS.md](STATUS.md).

**Current acceptance:** reproducible bug/race simtests with modest real workloads
and injected faults. The human superseded the earlier native-calibration, 10x
speedup and 100 µs stop requirements on 2026-10-05. Measurements below remain
observations; they do not impose those retired gates. A declared virtual CPU
model is sufficient. The completed seven-chat round is archived; integration
continues in the coordinator chat, with optional bounded subagents.

## Reading the evidence

Results below lead with what they establish. **Model** means scripted native
services; **bare KVM** means instruction fixtures; **functional Linux** means an
application works; **native** means uncontrolled host Linux. Only fresh guest
state comparisons qualify deterministic execution. Scope and remaining claims
are consolidated at the end.

Useful sources, fixtures and raw results are retained under research/.
[ARTIFACTS.json](ARTIFACTS.json) contains file provenance, checksums,
compression and relocation edits. Compressed files preserve their original bytes.
No evidence is accepted merely because its wrapper exits zero or a temporary
archive was sealed. Superseded prototypes, caches and bulk upstream copies are
not retained. Old unsupported evidence hashes/temporary links are omitted here.

## W5a: precise execution control — deciding gate

**Original-profile bare and bounded Linux correctness pass; useful cost is open.**
The original boundary-count probe ran 560 fresh bare cases on Ryzen 9 5950X.
All 520 conditional-branch cases match assembly predictions under idle/outside
load; 40 additional cases inspect SpecLockMap. Host administration and
completion-only reentry do not add guest counts. CPL filters and one modular
wrap behave as expected. **50,000 unrolled additions count zero**, so branch
count is not universal instruction/time progress.

The exact-stop probe uses host perf on the vCPU thread with rr's Zen event
`0x5100d1`, excludes host execution and counts guest CPL0/CPL3. An early overflow
signals KVM_RUN; guest-debug correction reaches count plus RIP/REP identity.
The first smoke lands at count 100,000 after 26,046 correction steps, about
147 ms including observer work. rr declares Zen skid unbounded; overshoot is a
failed target, not a stop at the requested point.

Original-profile V4 data contains **62 attempts** across idle/loaded cases.
The worker's explicit corrected-owner interpretation reports eight positive
plain/CPL3/PIO/REP groups with five fresh processes each, matching landing state
and uninterrupted terminal references. Both correction-off groups show five
different landings. Insufficient-margin and unconditional-loop controls fail
closed. This interpretation remains distinct from the original strict report,
which rejected `caller_threads=2`.

A V5 smoke identifies one userspace owner and a sleeping `kvm-nx-lpage-re` kernel
auxiliary task. The source keeps world/vCPU access on the owner. The revised
interpretation distinguishes that task rather than silently ignoring unexpected
threads; it does not prove absence of all auxiliary interference.

The intervening-OUT diagnostic returns #DB at the next instruction, before
completion-only EINTR. V4 checks exact completion RIP/count/unchanged RAM, a
following-instruction canary, analytic REP copied-prefix/remainder and continuation
with debugging disabled. Natural PIO remains a committed semantic boundary;
correction administration stays outside canonical numbering.

**Workaround-loaded profile is rejected.** All 31 enabled-loaded attempts record
CPU18's bit54 cleared before teardown, despite successful initial readback.
CPU2 remains set; saved originals are restored exactly with no unrelated-bit
change. Child success does not override the guard failure. The helper changed
from initial readback-only receipts to end-of-interval checking; enabled-idle
results from the earlier helper cannot establish persistence throughout a run.
Root perf permission and the explicitly authorized scoped workaround remain
separate from persistent host configuration.

**Cost warning:** original plain target 100,000 / margin 10,000 V4 elapsed medians
are **175.446 ms idle** (174.494–180.099) and **436.435 ms loaded**
(434.510–437.642), including the final 512 KiB RAM hash. Observed overflow skid
is 1,431–1,730 branches idle and 1,527–1,554 loaded. These debug-build whole-probe
numbers cannot serve as pure correction or normal-mode stop cost. Required next
measurements separate guest work, overflow/correction and witnesses, then sweep
margin and try target-RIP breakpoints. The then-requested 100 us median budget
is superseded; exact Linux correctness and practical simtest runtime now decide
integration.

A **pinned Linux starting input** is retained: 256 MiB RAM, same-host CPU ABI,
CPUID/MSRs, kernel/initramfs and a two-phase user loop. Each phase retires one
million conditional branches; a semantic PIO sits between them. No new timer or
device IRQ is injected during the bounded interval. This isolates stop mechanics
from known cold-boot differences; it is not full snapshot/boot qualification.
**V6 original-profile Linux qualifies its full 44-attempt matrix.** Targets
100,000 and 1,000,100 each match count/RIP, full GPRs, RAM, debug/event state and
uninterrupted terminal references in five fresh processes under idle and load.
All four correction-off groups have five different landings; ten corrected
second-phase cases complete the midpoint PIO during correction. All 44 TIDs are
fresh, initial witnesses match and before/after host profiles match the round pin.
Every original/restored MSR equals its saved value; this profile performs no writes.

W3 independently reconciles all raw rows, receipts, commands and ledgers, recomputes
register hashes and replays the validator/controls. Missing receipt, missing result,
host drift despite a true status flag, a truncated plan and an omitted loaded batch
each fail. Its earlier missing-attempt/matrix/profile concerns are closed for this
batch. Stock KVM suffices for this bounded stopping mechanism from the pinned start.
Complete snapshots, broader kernel/IRQ/debug transparency and performance remain
open. V6 elapsed includes hashing and cannot decide stop overhead.

**Separated cost smoke: correct, still expensive.** One optimized original-profile
bare breakpoint run reaches count 100,000 and the analytic 200,000-branch terminal
state with unchanged RAM. Stop commitment after overflow return takes **63.146 ms**,
of which correction takes **63.138 ms across 8,595 debug exits**. Landing RAM hashing
is separately **0.230 ms**. The fixed-RIP breakpoint fires once per remaining loop
iteration; removing hashing does not remove this correction cost. Host brackets
match the round pin and the read-only guard records no MSR changes or errors.
This is an exploratory known-future-RIP fixture, not a five-process Linux cost
qualification. The measured stop interval excludes overflow delivery/return and
therefore understates complete preemption overhead. Raw results:
[cost smoke](evidence/w5a/preemption-cost/cost-smoke-breakpoint-v1/),
[guard receipt](evidence/w5a/preemption-cost/cost-smoke-breakpoint-v1-plain-100000-10000.json).

**Smaller-margin exploratory stops pass at millisecond cost.** With margin 2,000,
one bare known-RIP breakpoint run uses **693 debug exits / 5.302 ms**, and one
count-only run uses **839 exits / 5.188 ms**. Both land and continue correctly;
their different overflow counts prevent an equal-tail method comparison.
Margin 1,500 overshoots by **215 branches** and is rejected; its fast return is
not a successful stop. All three receipts and host brackets match the original
read-only profile. Costs exclude overflow-return latency and hashing and are
single-process observations. These do not yet qualify generic Linux stopping.
The next Linux count-only smoke is prepared but unexecuted.
[Margin results](evidence/w5a/preemption-cost/margin-analysis.json),
[raw batch](evidence/w5a/preemption-cost/cost-exploratory-margin-v1/),
[Linux proposal](evidence/w5a/preemption-cost/next-linux-experiment-proposal.json).

Retained input: [Linux starting package](evidence/w5a/linux-start-package-v2.tar.gz)
and [members](evidence/w5a/linux-start-package-v2-members.json).
Original counters: [report](evidence/w5a/report.json),
[frozen source](evidence/w5a/completed-round-1-source.tar.gz).
[Bare deciding evidence](evidence/w5a/preemption-v4/original-owner-interpretation.json),
[validator controls](evidence/w5a/preemption-v4/preempt-owner-validator-negative-controls.json),
[V6 source review](evidence/w3/v6-source-review/review.json),
[Linux qualification](evidence/w5a/preemption-v6/qualification.json),
[matrix controls](evidence/w5a/preemption-v6/validator-negative-controls.json),
[independent acceptance](evidence/w3/v6-source-review/matrix-acceptance.json).
The original strict rejection and all invalid workaround attempts remain retained.

## Future CPU and host portability

**Intel is the closest extension; ARM and macOS remain unqualified.** rr's
[counter implementation](https://github.com/rr-debugger/rr/blob/master/src/PerfCounters.cc)
uses model-specific retired conditional-branch events on Intel and retired taken
branches on supported Neoverse CPUs. Its Apple M1/M2 entries are evidence about
hardware counters under Linux, not qualification of macOS guest-only counting.
[KVM's guest-debug API](https://docs.kernel.org/virt/kvm/api.html#kvm-set-guest-debug)
supports arm64 single-step and architecture-specific hardware debugging. These
are relevant primitives; rr userspace support does not establish whole-guest
counter filtering, overflow handling or deterministic ARM atomics.

For Graviton, [AWS documents full PMU exposure on metal instances](https://aws.github.io/graviton/perfrunbook/debug_hw_perf.html)
subject to OS drivers. Bare metal is the starting candidate for our own ARM/KVM
backend; ordinary instance PMU access alone does not establish nested KVM access.
No cloud machine has been provisioned or tested.

[Apple's Hypervisor framework](https://developer.apple.com/documentation/Hypervisor)
documents one VM per process and vCPUs owned by their creating threads;
[debug traps](https://developer.apple.com/documentation/Hypervisor/hv_vcpu_set_trap_debug_exceptions%28_%3A_%3A%29)
are exposed. Guest-only deterministic PMU overflow control is still unestablished,
and VM ownership differs from the current multi-machine, one-thread world.
Apple hardware running Linux would retain the KVM approach where supported.
Native ARM virtualization needs ARM guest binaries. A deterministic macOS guest
would additionally require a new OS input/admission profile.

**Local Mac check: park native HVF for now.** Read-only inspection of this M4 Max
and installed SDK finds no public arm64 retired-counter/count-triggered-exit API.
Single-step/debug correction exists, but the SDK ties CNTVCT to host elapsed time
minus an offset and restricts vCPU ownership to one per thread. Together with the
one-VM-per-process rule, these leave both execution control and world topology
unqualified. [QEMU's HVF implementation](https://raw.githubusercontent.com/qemu/qemu/master/target/arm/hvf/hvf.c)
documents cycles-only OS PMU emulation. ARM exclusive-monitor behavior also needs
admission review; bare-Linux rr counter support does not qualify this macOS path.
No VM ran, RNDR exposure was not measured, and unsupported/private alternatives
were not ruled out. Exact host/header locations and primary-source references:
[findings](evidence/mac-feasibility/findings.json).

[QEMU TCG instruction counting](https://www.qemu.org/docs/master/devel/tcg-icount.html)
provides exact execution budgets without a hardware PMU. It is a plausible later
local Linux candidate under the modest-simtest objective, but input/clock closure,
support-thread effects, world-owned devices and workload runtime remain untested.
The bounded follow-up in NEXT_STEPS is optional after the first AMD simtest.

## W2: real Linux workload and input closure

**Redis works in one owned Linux VM; fresh Linux equality fails.** Guest Linux
6.12.112 and static Redis 7.2.7 complete two fresh 1,000 SET/GET workloads over
loopback. Application throughput is roughly **1,016–1,025 operations/host second**,
including boot, observation and final RAM hashing. The prototype records about
116–117k logical exits/host second. Its **0.0302–0.0305 simulated s/host s** uses
an artificial one-microsecond clock-read charge and is not a calibrated O5 result.

The charged-poll control advances an event after 1,000 reads. Removing charges
leaves a 100k-read guest poll unfinished, even though Redis still works. Thus
cooperative virtual time needed an explicit policy. That policy is now retired
in favor of calibrated counted progress.

Fresh boots first differ at boundary 206 / sequence 207 in `native_sched_clock`:
same RIP, differing stack/context. CPUID/APIC-ID warning appears in one boot;
normalization was compiled but not measured. This is an investigation lead,
not established attribution of every RAM difference. A sleeping KVM NX recovery
task was also observed despite disabled host NX mitigation.

The runner currently stubs PIC/PIT/RTC/serial, uses PIO-based synthetic TSC and
fixed credited boot entropy, and has a bounded CPU oracle. The next runner work
is cheap clock access, controlled CRNG/vDSO/direct instructions, NO_HZ_IDLE,
counted timer/IRQ delivery and actual owned virtio devices. Input closure and
JVM compatibility are the deciding integration uncertainties.

[Report/stub list](evidence/w2/report.json),
[first comparison](evidence/w2/clean-comparison.json),
[runner sources](kvm-runner/Cargo.toml).
The measured-source variant is retained separately from the compiled normalized
variant; it was reconstructed by reversing mechanical handoff edits. Useful
kernel/Redis/busy inputs and exact boundary/serial traces are preserved compressed.

**Off-host clock/IRQ preparation passes its smoke.** Rational count→ns conversion
with fractional carry, duplicate-count handling, incarnation guards, pending-I/O/
debug IRQ deferral and the **128-byte** C clock-page ABI pass locally. Seed changes
alter synthetic queued IRQ targets. The proposed NO_HZ_IDLE/HZ=100 configuration
checker rejects the retained HZ=250 profile. A passive page stays fixed between
stops, so progress depends on qualified preemption; ordinary pvclock still reads
TSC. The Linux injection adapter is prepared but uncompiled/unexecuted. Native
calibration, guest clocksource/IRQ and snapshot execution remain integration work.
[Clock/IRQ contract](kvm-runner/next-profile/contract.json),
[smoke provenance](evidence/w2/next-profile/manifest.json).

## W1: world, protocol and fault semantics

**The native plain-data protocol and its restore controls pass.** Twelve tests,
formatting and strict Clippy pass. Five fresh full traces agree under outside CPU
load and timezone changes. Seed 43 changes actual delivered frame identities;
both seeds deliver 15/32 in the partial-loss fixture. Seven deliberately broken
variants remain sensitive.

The scenarios exercise directed AZ2→AZ1 loss, partial AZ3→AZ1 loss and overlapping
fault handles; crash during queued disk/DMA work; and rolling recovery of three
scripted replicas. Flushed value 17 survives, acknowledged volatile value 18 does
not. Obsolete callbacks cannot mutate new incarnations; already transmitted
packets and declared physical writes follow separate survival contracts.
Dispatching one equal-time event and resuming its ready controller before the
next preserves the intended fault intervention.

The wire is little-endian u32 length plus UTF-8 JSON, capped at 16 MiB. Ordered
controller/incarnation/command identities support idempotent duplicate commands;
observations remain until ordered acknowledgment. The reliable control channel
is separate from application frames.

World state covers ingress/pending payloads, response/ack queues, deadlines and
registration order, frames, faults, RNG streams, incarnations, DMA and durable/
volatile storage. The native controller saves program/PC/locals/pending command;
the scripted service saves pending flush replies. Snapshot/decode/resume while
sleeping, with a fault queued and awaiting health all reproduce the full future
trace and final bundle. Lost/reordered commands fail restore validation. Observer
history is excluded. This replaces the earlier opaque-Future continuation gap.

The [off-host causal argument](evidence/w1/causal-horizons-2026-10-05/argument.json)
requires **per-machine frontiers**, proven
producer lookahead and exact equal-time closure. Empty queues, a ready controller,
shared disks/DMA and a latency-reducing heal are counterexamples to advancing
solely to known event minima. The retained model currently sums stopped-guest
deltas and dispatches overdue events at `max(now,due)`; those operations must
change for the exact multi-machine profile. Source-time stamped scenario actions
need the same barrier admission as network effects. This is a conditional design
argument, not a hardware result.

[World source](world/Cargo.toml),
[wire contract](world/protocol-contract.json),
[final summary](evidence/w1/protocol-v2/summary.json),
[manifest](evidence/w1/protocol-v2/manifest.json).

## W3: landing and state witnesses

**Witness 0.2.0 detects synthetic divergences even when application output matches.**
Eight unit tests and 20 fresh-process suite checks pass. Count, RIP, REP RCX/RSI/
RDI, boundary kind, inactive-vCPU state and control payload mutations localize
at boundary 5. Thirty-six administrative diagnostics leave twelve canonical
boundaries and world state unchanged.

The contract declares CPU inventory, counter source/validity/epoch/wrap, landing
registers, RAM and structured world/device/event/RNG/control state. Unknown or
inconsistent counts are rejected. Dirty omissions are detected at the periodic
full checkpoint; a fresh full-memory rerun localizes the actual earlier change.
Detailed reruns regenerate from original inputs, not recorded execution logs.

W3's first W5a review found exit-zero cases that had missed/overshot the target,
insufficient profile grouping and missing analytic/continuation acceptance.
The strengthened validator checks actual child/guard status, analytic state,
PIO completion and uninterrupted terminal references. The completed V6 review
found missing-attempt/matrix/profile qualifications, now closed in the deciding
Linux matrix. Normal mode retains required execution checks and coarse
checkpoints; check mode adds declared CPU/world/device/control state and dirty
guest pages union host-written/DMA pages, with full audits. Both use identical
canonical seed/time/stop choices. Exclusive cost buckets count each host interval
once; KVM_RUN includes KVM work, and check-minus-normal includes indirect
dirty-logging costs.

[Witness source](determinism/Cargo.toml),
[v2 contract](evidence/w3/integration-contract-v2.json),
[v2 results](evidence/w3/precise-landing-v2/results.json).
Normal versus determinism-check witness cost remains useful simtest diagnostics.

## W4: native workload observations and next benchmark

**The native survey gives useful workload oracles; it does not decide simulation speed.**
Redis 8.10.2 exercises AOF rewrite, BGSAVE, replica reconnect and graceful restart
with 1,001 keys. etcd 3.6.14 exercises compact/defrag/snapshot/restart with 40 keys and
Go signals/shared mappings. PostgreSQL 18.6 exercises WAL replica reconnect,
checkpoint/restart and process/SysV sharing with 1,002 rows. Tokio 1.48 current-thread
and two-worker services each answer 64 requests through edge-triggered epoll/eventfd.

A 10,000-read libc-clock fixture reports success with zero traced clock syscalls;
forcing the syscall produces 10,000. Syscall traces miss vDSO/direct instruction
inputs. io_uring was not observed on these paths. The corpus does not cover packet
loss, power loss, multi-member Raft, PostgreSQL promotion or JVM/JIT.

[Native matrix](evidence/w4/native.20261005T203307/matrix.json),
[recommendation](evidence/w4/recommendation.json),
[retained harness](workloads/survey.py).
**The Redis/Java fixture is retained off-host preparation.** Java/Jedis 5.2.0 compiles against
six pinned release jars on Darwin Zulu 21.0.11+10. The prepared profile is
100 operations/s, concurrency 1, 80/20 reads/writes, 1,024 keys and 256-byte values
for 60 s; replication is partitioned/healed at 25/35 s and AOF rewritten at 40 s.
Inputs include 1 ms one-way latency, HZ=250 plus NO_HZ_IDLE, explicit disk costs
and a warmed client snapshot point. Recovery checks remain in the host denominator.

The local RESP-mock smoke passes **200 offers (160 reads / 40 writes)** and
32 replica keys; a corrupted expected read fails. Cost arithmetic controls reject
double-counted time, an arbitrary calibration factor, changed profile and an
unreached fault. Synthetic results cannot qualify a guest simtest. This prepares
the fixture and contracts; real Redis/VM/fabric/AOF/snapshot behavior is unexecuted.
The retained measurement code implements the historical performance profile,
whose numeric gates and mandatory native calibration are inactive for current
Seedbox acceptance. A smaller trusted client can serve the first race/fault demo.
The shared Nix benchmark shell smoke reports Darwin JDK
21.0.11+10 and Redis 8.10.2; the final Linux JDK pin is 21.0.12.1+1.
[Profile](benchmarks/profile.json),
[off-host smoke](benchmarks/offhost-validation.json),
[measurement contract](benchmarks/measurement-contract.json).

## W5b: bounded instruction admission

**Fixed-site AMD RNG mediation and CPL3 clock controls pass; CPUID hiding alone fails.**
Seven Rust tests/build/format checks pass. Two fresh seed 42 runs match; seed43
changes consumed guest values. Three manifested RDRAND/RDSEED encodings use
seeded responses with tested width/flag behavior. Hidden/exposed raw instructions
execute unmediated and fail the zero-trap oracle; unrelated UD2/#GP is rejected.

A candidate finder sees three instruction sites plus two handler constants;
MOV-immediate demonstrates why blind byte rewriting is invalid. The original
handler test uses synthetic INT6, not an Intel hardware interception. Intel is
source-only because that host is unavailable.

Linux CRNG extraction also depends on the prior pool, CPU/cycle inputs and later
reseed/cache/vDSO state. Late /dev/urandom seeding cannot undo earlier influence.
Trusted pinned kernel/programs plus controlled CRNG are the initial admission
strategy. A JVM/JIT audit must bind to the benchmark's selected JDK profile.

The pinned Linux OpenJDK **21.0.12.1+1** source performs RDTSC during counter
initialization before checking `UseFastUnorderedTimeStamps`; disabling that flag
alone does not close the early input. Warmed Java generators also retain state
independent of a newly selected world seed, and NativePRNG buffering uses realtime.
The audit therefore requires an early trusted-JDK guard or correct TSC mediation,
both clock domains and a qualified application/provider reseed policy. These are
source findings; a local Java probe is not x86/JVM guest admission.
[Pinned JVM audit](intel-probe/java/admission-audit.json).

[Source probes](intel-probe/validate.sh),
[entropy/admission profile](evidence/w5b/entropy-profile.json),
[native results](evidence/w5b/native/run.5Ap08O/stdout.txt).

## Earlier useful component results

**Two bare KVM contexts share one userspace owner without sharing RAM.** Five
fresh bounded observations match; intentional RAM aliasing corrupts a guest and
omitting an incarnation guard permits an obsolete completion. The legal-IRQ
fixture holds a vector while IF=0, waits through STI's shadow and injects at KVM's
ready window. The handler observes the expected phase and resumes with IRETQ;
forced early injection is sensitive.
[Owned-KVM fixture archive](evidence/earlier/cooperative-kvm.tar.gz),
[IRQ archive](evidence/earlier/irq-kvm.tar.gz).

**Early Linux clock/entropy intervention works only within its scoped interval.**
Thirteen functional boots reach PID1; three controlled fixed-seed cases match
selected checkpoints, and seed43 changes actual CRNG samples. Changing the
emulation clock scale changes the untouched prior pool and downstream samples
before the intervention, despite matching supplied words. That exposes incomplete
input coverage rather than same-input divergence. Later userspace time still
tracks the outside RTC.
[Retained source/checkpoint archive](evidence/earlier/linux-boot-controls.tar.gz).

Syscall/scheduler/idle yield lists miss SQPOLL and other autonomous pollers.
An empty event queue can coexist with runnable producers. If A advances to 8
while B can send at 5 and deliver at 6, known event minima cannot repair A's past
observation. A ready fault controller and equal-time receive/timeout ties also
require semantic ordering; shared devices need deterministic arbitration. These
counterexamples motivate single-world ownership and conservative causal horizons.

## W6: reused mechanisms and verified bug target

**Use focused tests, not a replacement modeled-Linux runtime.** rr's inspected
Zen event supplies the counter starting point and declares unbounded skid.
Shadow's shared clock/cost accounting offers explicit deadline/result-ownership
ideas; its default modeled costs are not calibrated native CPU time. gVisor/LTP
readiness, futex, mmap and signal fixtures are future Linux conformance inputs.

The executed Hermit epoll fixture passes polling and blocking on native Linux;
removing producer writes times out as intended. Native scheduling can preempt
it, so a guest test still needs a witnessed poller-before-producer interval.
[Vendored source and license](prior-art/hermit/tests/c/epoll_pwait_zero_timeout_progress.c),
[raw native result](evidence/w6/run.yL7eck/stdout.txt),
[source review](evidence/w6/findings.json).

**The etcd target is verified upstream.** The maintainer postmortem identifies
non-atomic database/apply and consistent-index persistence introduced in 3.5.0,
reproduced by SIGKILL under heavy requests and fixed in 3.5.3. HashKV comparisons
must use the same available revision; a missing/compacted revision is not a
healthy result. The proposed campaign preserves durable disk across process
kill, tracks acknowledged writes and compares 3.5.2 with 3.5.3.
[Postmortem](https://github.com/etcd-io/etcd/blob/main/Documentation/postmortems/v3.5-data-inconsistency.md).

The version-pinned source follow-up verifies the fix and proposes **4 host-hours**
across 16 paired seeds. Its first campaign uses process SIGKILL with kernel/block
caches and the same data directory preserved, rather than VM power loss. An immune
ack ledger plus local serializable reads at a final common revision prevents
unanimous loss or a HashKV error from passing. No bug run has occurred.

That follow-up leaves CPU18's workaround-bit reset **unattributed**. Linux's cached
LS_CFG SSB path is restricted to older families; it cannot explain the family-19h
5950X observation by itself. The failed model-specific AMD download supplied HTML,
not a register specification. No host mitigation change/module is justified.
[Source findings and proposed campaign](evidence/w6/followup-20261005/results.json).

## Preservation, host inputs and limits

Selected source/evidence compresses roughly 1.4 GB to about 49 MB before the new
8 MB Linux starting package and deciding preemption results. Build targets, vmlinux/download caches and discarded
modeled-Linux/Python runtime code are excluded. The native world rebuild passes
12 tests; witnesses pass 8 tests / 20 suite checks; admission preparation passes 7 tests.
A relocated seed 42 world run reproduces the retained full trace exactly.
Relocation-only path/output changes are recorded separately from measured source.
All retained indexed bytes and 90 lossless compressed entries verify; all 20 Linux
payload members and its embedded index verify. Compact relocation checks are
[retained](evidence/relocation/summary.json).

[Host profile](host-profile.json) pins the AMD 5950X, kernel 6.18.54,
CPU microcode 0xa20102e, booted kernel identity and PMU/NX/perf settings. Check
before/after deciding batches, abort on drift, and keep kernel/microcode unchanged
for the round. Generic Linux stopping precedes modest simtest integration on the
same lock. A second suitable Linux machine would allow independent seed runs and
cross-host comparison.

Still unshown: generic count-only Linux stops; complete kernel/runtime clocks and
entropy including JVM/JIT; real virtio/DMA/network/disk fault integration; causal
multi-machine/seeded IRQ behavior; coherent full VM/world restore; reproducible
modest race/fault simtests and fixed controls; cross-host equality; and the bounded
etcd bug campaign. JVM/JIT and native performance calibration are later coverage.
Model/native/bare successes do not close these claims. The integrated gates in
NEXT_STEPS decide when probes become the versioned runtime.
