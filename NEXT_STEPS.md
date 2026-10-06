# Seedbox next steps

Updated 2026-10-05. [STATUS.md](STATUS.md) tracks open/closed items;
[CONCLUSIONS.md](CONCLUSIONS.md#primary-objectives) defines O1–O5 and the design;
[EVIDENCE.md](EVIDENCE.md) records results. Only remaining work belongs here.

The next milestone is a **modest deterministic bug/race simtest with fault
injection**, a reproducible failing seed and a fixed control. Runtime costs are
observations. The former 10x speedup, native slowdown, native calibration and
100 µs stop gates do not block this milestone.

The coordinator integrates directly. Use bounded subagents where useful; do not
require a new Codex chat for every slice. The seven research chats are archived.
Their useful artifacts remain in [research/](research/), with provenance and chat
identities in [ARTIFACTS.json](research/ARTIFACTS.json).

## 1. Qualify generic counted Linux stopping

Addresses O1/O2/O3. Preserve the accepted
[V6 correctness baseline](research/evidence/w5a/preemption-v6/qualification.json)
and [independent acceptance](research/evidence/w3/v6-source-review/matrix-acceptance.json).
These qualify a fixed start and fixture-supplied instruction points.

- Run the prepared **one-process Linux count-only smoke**, target 1,100,000,
  margin 4,000, using the unchanged reviewed source/binary and fixed starting
  input. The proposal is retained in the preemption-cost artifacts; it has not
  run. Its correction tail excludes PIO. A matching expected RIP may validate
  the result, but must not select the stop.
- If it passes, qualify the changed mechanism with five fresh processes,
  full declared landing/continuation comparisons and independent review.
  Declare all expected attempts and preserve failures/receipts. Cover a natural
  exit during correction and count/RIP/REP ambiguity when extending that scope.
- Keep overshoot a failed target. A margin chosen from observed skid is not a
  universal bound; record the supported profile and its failure/admission policy.
  Branch-free work retains a declared progress/watchdog policy.
- Measure cost without tuning indefinitely toward a microsecond threshold.
  Bare 5.19 ms count-only correction is useful evidence for the workload runtime
  estimate, not a reason to abandon precise preemption.

The AMD host stays behind its existing lock, with kernel/microcode/CPU/KVM/PMU
inputs checked before and after runs. Generic correctness has first priority;
no extra hardware slice. Enabled-loaded workaround attempts remain suspended
until their sibling-bit reset is explained. Host patches need a concrete review;
none are assumed necessary by the bounded stock-KVM pass.

## 2. Close a small Linux profile and coherent starting state

Addresses O1/O2/O3. Reuse the retained KVM runner and clock/entropy preparation.

- Keep boundaries abstract: natural exit, counted stop, cooperation or lifecycle.
  Complete emulated I/O before publishing a boundary; administrative retries add
  no canonical event, virtual time or RNG choice. Retain stub-and-list.
- Close native_sched_clock divergence and uncontrolled TSC/pvclock/vDSO/direct
  instruction inputs. Integrate a controlled clock path and CPUID profile for
  the trusted kernel and first guest programs.
- Pin a rational **declared virtual-time model** for branch progress, epochs and
  remainders, plus any software/clock-read progress charges. Native-speed matching
  is optional. Host execution duration never determines guest time or ordering.
- Integrate seeded CRNG initialization, extraction and reseed/cache behavior.
  Check getrandom, /dev/urandom and kernel RNG with fresh equality, consumed-seed
  sensitivity and omitted-control negatives.
- Qualify a scoped post-boot VM/world starting input and fresh continuation,
  including CPU/KVM/RAM, devices, disks/caches, events, RNG and controller state.
  The W5a starting package is a mechanism fixture, not a complete snapshot.
  Make crash/restart templates agree with surviving storage and incarnation rules.
- Keep NO_HZ_IDLE and idle jumps where supported. Measure costs as needed to
  complete useful tests. JVM/JIT clocks, RNG and warm snapshots follow after
  admission of the initial small trusted programs.

## 3. Integrate causal progress, scheduling and faultable devices

Addresses O1/O3/O4. Reuse the native world protocol, causal-horizon argument and
witness contracts; do not build a competing scheduler.

- Replace scalar summed guest time with per-machine frontiers/remainders and a
  world commit frontier. Two guests reaching local 1 ms advance the world to
  1 ms. Start with synchronized safe epochs and explicit equal-time ordering.
- End execution before the earliest affecting deadline or any producer's proven
  earliest unpublished effect plus its minimum path latency. Include blocked
  wake bounds, controller/shared-device zero-latency paths and latency-reducing
  faults; empty queues alone do not justify advancing time.
- Source-time stamp scenario commands and apply/acknowledge them at affected-
  machine barriers. Preserve exact world-effect timing; Linux interrupt masking
  may delay IRQ acceptance, but cannot justify skipping a due device event.
- Seed real guest timer/IRQ/readiness choices that alter Linux task scheduling.
  Host pauses alone do not demonstrate guest race exploration.
- Connect world-owned network/disk devices and DMA to the ordered imperative
  control channel. Health queries use faultable application traffic. Fault
  handles remain scoped, composable and independent when healed.
- Enforce machine incarnation ownership for pending callbacks/I/O; define what
  survives a process crash, machine crash and reboot. Start with the retained
  provisional 1 ms link model; no hardware-performance fidelity claim is needed.

## 4. Run deciding race and fault simtests

Addresses O1–O5. Reuse small trusted fixtures first; Java is later coverage.

- Run a multithreaded lost-update fixture inside real Linux. Find a seed that
  exposes the invariant violation, reproduce its schedule/failure from fresh
  processes, and show an atomic fixed control passes the same scenarios.
- Run a modest real service workload with concurrent activity across machines.
  Reuse Redis primary/replica and a small trusted guest client where convenient.
  Inject partition/heal and crash/restart while work is active. Assert state
  consistency and recovery under the workload's declared durability contract.
- Prove faults reached traffic/state: nonzero drops during the partition,
  deliveries after healing, live operations during faults and verified recovered
  application state. Include a sensitive deliberately broken control/oracle;
  completing the scenario alone is insufficient.
- Compare declared VM/world state and semantic traces across fresh same-seed
  runs. For the deciding determinism result use five fresh processes and review;
  keep seeds/input profiles, schedules, faults and first divergence reproducible.
- Report operations, explored seeds, virtual duration and end-to-end wall time,
  with boot/restore separate and normal/check witness costs explicit. Optimize
  measured obstacles to these runs. Slow execution does not weaken invariants
  or permit late causal delivery.

The retained Redis/Jedis profile and native performance measurement tools are
historical preparation. Their old numeric gates do not define simtest acceptance;
reuse workload/oracle code later without requiring JVM admission for this milestone.

## 5. Consolidate and broaden bug finding

After generic Linux stopping and the modest race/fault demos pass, consolidate
qualified components into a versioned Rust runtime. Document supported programs,
starting-state coverage and remaining CPU/IRQ/debug/input restrictions.

Run the prepared **etcd 3.5.2 / 3.5.3** bug/fix campaign after its guest profile is
admitted: process kill/restart under writes, surviving disk, acknowledgment ledger
and same-revision HashKV. Declare a host-hour budget and report unreproduced
results if exhausted. This real bug target supplements the small race control.

Then add spinning/SQPOLL, fork/persistence, shared-memory plus remote TCP, focused
Linux API cases, more fault families, and broader runtime/JVM admission. Move the
immune scenario into a privileged control VM later; build branch exploration,
shrinking and coverage on the same protocol. Intel/ARM qualification is deferred.

## Optional local-emulation experiment after integration

Addresses O1/O3/O5: a local guest backend could relieve the one-AMD-host bottleneck.
Native HVF is parked after the source/SDK check. If local full-guest testing is
still a bottleneck after the first AMD simtest, cap a QEMU TCG feasibility run at
1–2 hours. Reuse compatible x86 Linux inputs, fixed instruction-count time and
controlled RTC/CPU/devices, with no external input or recording/replay prerequisite.
Compare five fresh deterministic-boundary CPU/RAM and clock/entropy/application
witnesses, then measure a small race/fixed-control workload with boot separate.
Passing establishes a candidate, not multi-machine world integration; KVM snapshots
are not automatically portable. Keep single-world-thread and device ownership
constraints explicit. No experiment has run; do not start a competing backend
implementation or restore the discarded modeled-Linux prototype.

## Evidence and coordination discipline

Write for readers: result first, key numbers inline, limits once. Five-process
qualification, strict validators and independent review belong to **deciding
correctness/determinism** results. Documented behavior and exploratory measurements
get a smoke test; broaden only for a new unresolved concern.

Preserve useful source/input/raw results in the repo, including failed deciding
attempts and provenance. Temporary runs are scratch; exclude caches/superseded code.
Keep host inventory/hashing outside measured stop cost. Do not create additional
planning/status documents or AMD hardware slices. A second suitable host would enable
independent seed runs and cross-host comparison. Delegation is optional; the
coordinator owns integration, conclusions and acceptance.
