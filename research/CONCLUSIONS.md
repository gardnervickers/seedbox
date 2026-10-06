# Seedbox design conclusions

## Primary objectives

| ID | Objective | Required outcome |
| --- | --- | --- |
| O1 | Seeded determinism | The same seed and pinned inputs regenerate execution, with controlled scheduling, clocks, entropy, devices and faults. No recording/replay prerequisite. |
| O2 | Linux fidelity | Real Linux, including shared mappings, signals, io_uring and autonomous kernel work, under an explicit supported profile. |
| O3 | Machines | Independent machines support local processes/threads/shared memory, remote communication and correct crash/reboot lifetimes. |
| O4 | Programmable faults | An immune imperative scenario drives scoped/correlated faults on simulated time and checks actual recovery. |
| O5 | Practical simtests | Modest concurrent workloads exercise bugs and races under injected faults, finish in manageable time and reproduce failures by seed. Report runtime and costs; no fixed speedup or native-performance-equivalence requirement. |

This is the design narrative. [EVIDENCE.md](EVIDENCE.md) records findings and limits;
[STATUS.md](STATUS.md) tracks the round; [NEXT_STEPS.md](NEXT_STEPS.md) holds remaining
work. Retained research code and inputs live in [research/](./).

## What we are building, and why

Seedbox runs real Linux machines inside a Rust deterministic world. Linux supplies
processes, threads, shared memory, filesystems and TCP; the world owns virtual
devices, storage, networking, clocks, entropy and faults. Containers package
applications inside machines. Initially each x86-64 machine is a KVM VM with one vCPU.

The first deliverable is a bug-finding simtest: concurrent application activity,
seeded guest scheduling, injected faults and assertions that expose failures.
A failing seed must reproduce the same failure; a fixed implementation provides
a passing control. Virtual CPU speed is a declared simulation input.

**D001: one userspace host thread per world.** It serializes guest execution,
world/device changes, scenario actions and canonical observations. Parallelism is
across independent seeds and snapshot branches. Kernel auxiliary work must not
introduce uncontrolled effects. Mixed architectures and weak-memory exploration
are deferred.

**D002: precise counted preemption is primary; cooperation is fallback.** Count
retired guest conditional branches, overflow early and correct to an exact count
and instruction point. Linux schedules normally using controlled timer/device
interrupts; natural exits remain boundaries. Cooperation uses the same interface
but excludes loops that never reach a hook. SQPOLL defeats an exhaustive yield list.
The primary mechanism must pass **Linux correctness** and permit modest simtests.
Millisecond stop overhead is measured work, not an automatic architecture rejection.

**D003: reuse Linux under KVM.** Modeled Linux/Unicorn and Shadow/Hermit-style
syscall interception are dropped: they recreate Linux and lack real machine
reboot semantics. UML time-travel also stalls in never-yielding loops and exposes
host instruction inputs. Full emulation is outside the primary KVM path. Its prior
**100–1,000x slowdown** figure is an unmeasured planning estimate; measure a modest
workload before using cost to reject it as a possible local test backend. Borrow
focused tests and mechanisms from existing projects.

A stopped boundary carries machine/incarnation, kind, count/profile, RIP and REP
registers. Complete emulated I/O before publishing it. Correction administration
adds no semantic boundary/time/RNG choice; guest instructions still count, and
natural device exits during correction remain semantic events.

Keep the world interface in terms of deterministic progress and stopped boundaries,
with counter semantics and CPU state owned by the execution backend. Intel should
reuse the x86/KVM path after qualification. Graviton needs an ARM/KVM backend and
ARM guest images. The Mac source/SDK check found no supported counted-preemption
path, a host-time-derived guest timer and incompatible VM/thread ownership for a
direct port. Park native HVF; this is an API/qualification gap, not an impossibility
proof. After the first integrated simtest, a bounded QEMU instruction-counting
experiment could assess local software-emulated Linux. World devices, faults and
scenarios remain shared interfaces; an emulator is not already an integrated backend.
Repeatability is within a pinned execution profile; identical traces across CPU
vendors or architectures require separate qualification. See the
[portability source review](EVIDENCE.md#future-cpu-and-host-portability).

## Time, causality and seeded schedules

**D004: declare virtual CPU progress.** Pin a rational virtual-time conversion
from the qualified branch metric, including epochs and fractional remainders.
Native calibration is optional later work. Choose the model to exercise concurrent
behavior and timeout/fault interactions; document any clock-read or software
progress charges. Branch-free work needs a declared progress/admission policy.
Wall-clock execution cost must not move guest time or alter semantic ordering.

**D005: exact delivery, initially synchronized safe epochs.** Track per-machine
frontiers and a world commit frontier: two machines each reaching local 1 ms advance
the world to 1 ms, not 2 ms. A safe horizon is the earliest affecting deadline or
any producer's proven earliest unpublished effect plus its minimum path latency.
Blocked producers need sound wake bounds; empty queues do not exclude future sends.
Close equal-time possibilities before guest continuation at the horizon.

Start with a **provisional 1 ms minimum link latency**; earlier deadlines can
shorten slices. Control/shared-device/zero-latency paths require coordination.
Scenario commands carry source simulation time and apply at affected-machine
barriers; latency-reducing faults invalidate horizons. The native model's summed
clock/overdue dispatch needs replacement. Bounded lateness is a separate later profile.

Jump to the next event when all producers are blocked. Batch timestamped work
only while preserving fault, event, guest and controller ordering. Named seed
streams choose legal timer/IRQ/preemption points; a host stop alone does not switch
Linux tasks. Verify an actual lost-update race varies across seeds and an atomic
control passes.

## Faults, state and snapshots

An immune imperative scenario sleeps, partitions/heals, crashes/reboots and
queries applications through a command/observation protocol. Its assertions can
fail the test. The control channel is reliable; health traffic traverses the
faultable topology. Resource-scoped handles express directed/partial AZ faults,
overlapping rules, malformed packets, stale DNS, routing and disk failures.
Healing one handle neither erases another nor proves recovery.

World/controller state is plain serializable data: events/payloads, faults,
storage, RNG streams and continuation. Callbacks carry incarnation ownership;
old callbacks cannot mutate a rebooted guest. Already transmitted packets and
physical writes survive according to their resource contracts. Later run the
scenario in an immune privileged guest VM, allowing other languages and complete
scenario snapshots.

**D006: snapshots move earlier.** Scoped pinned starts isolate preemption from
cold boot; post-boot starts enable the first simtests. JVM warmup snapshots follow
when that runtime is admitted. Qualify fresh
continuation and CPU/KVM/RAM/device/world/control coverage before claiming complete
restore. Reboot templates must agree with surviving disks and caches. Warmed
application/provider RNG state needs an explicit seed contract.

**D007: normal and check witness modes.** Normal keeps required boundary checks,
assertions and coarse checkpoints. Check adds declared CPU/world/device/control
state, dirty guest pages union host DMA writes, and periodic full audits. Both
use identical canonical inputs and scheduling. Report both costs; full per-stop
RAM hashing is outside the normal path.

## Deciding simtests and practical runtime

The first milestone needs two demonstrations. A small multithreaded lost-update
fixture must fail under some seeded guest interleavings and pass with its atomic
fix. A modest real Linux service workload must continue through a scoped network
partition/heal and crash/restart, with application invariants that detect lost
acknowledged state, corruption or failure to recover under its declared contract.
Show that faults reached live traffic/state, rather than just logging commands.

Reuse the existing Redis fixtures and a small trusted guest client where useful;
JVM/JIT admission need not block the first demo. The retained three-VM Redis/Jedis
profile (**100 operations/s**, **60 simulated seconds**) is later coverage.
It has never run in the simulator. Its old 10x, slowdown and microsecond budgets
are superseded by the simtesting objective.

Report end-to-end wall time, operations, explored seeds, preemption and witness
costs, with boot/restore separate. Optimize measured obstacles to useful test runs.
Idle jumps, coarse checkpoints and NO_HZ_IDLE remain useful mechanisms. Correct
causal ordering and reproducible failures stay required when choosing slice sizes.
A fixture-supplied future RIP cannot select generic stops on a first execution.

The bug target is three-member **etcd 3.5.2**, killed/restarted under writes with
surviving disk, an acknowledgment ledger and same-revision `hashkv`. Its non-atomic
apply/consistent-index bug was fixed in **3.5.3**, the paired campaign control.
[Maintainer postmortem](https://github.com/etcd-io/etcd/blob/main/Documentation/postmortems/v3.5-data-inconsistency.md).
The proposed **4 host-hour** campaign is unexecuted; zero fixed-version failures
would be bounded evidence.

## Open risks and research exit

Bounded original-profile Linux stops pass **44/44 planned attempts**, including
idle/load, five fresh processes per positive target, negatives and independent
review. Stock KVM suffices from this pinned start. Exploratory smaller-margin bare
stops take **5.30 ms with a known-RIP breakpoint / 5.19 ms count-only**, excluding
hashing and overflow return. A still-smaller margin overshoots by **215 branches**
and is rejected. Count-only Linux stopping and integrated simtest runtime remain
unshown; no fixed 100 µs threshold blocks integration.

Remaining risks are generic targeting, broader Linux IRQ/debug fidelity,
clock/entropy/JVM/JIT closure, causal scheduling, coherent snapshots and real
virtio/network/disk fault integration. Leave research mode when Linux stops pass
correctness, modest race/fault workloads run with fresh state equality, a failing
seed reproduces and the corresponding fixed control passes. Broader runtime
admission and throughput improvements can follow that scoped milestone.

The pinned AMD host runs **generic Linux stopping first, simtest integration next**.
Keep kernel
**6.18.54** and microcode **0xa20102e** unchanged and reject profile drift. A second
suitable Linux host enables independent seed runs and the first cross-host
comparison; Intel is unavailable.

The coordinator owns integration and the central docs. Use subagents for bounded
independent work when useful; creating a chat for every slice is unnecessary.
The completed seven-chat research round is archived, with useful results retained.
