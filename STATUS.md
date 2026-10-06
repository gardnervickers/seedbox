# Seedbox status

Updated 2026-10-05. [Design and O1–O5](CONCLUSIONS.md),
[evidence](EVIDENCE.md), [remaining work](NEXT_STEPS.md).

The goal is **reproducible bug and race exploration with modest workloads and
fault injection**. Real Linux/KVM, one VM/vCPU per machine, one userspace thread
per world. Precise preemption is primary; cooperation is fallback.
The integrated simtest milestone has not passed.

## Current work

The coordinator owns integration. All seven research chats are **archived**;
their sources, inputs, results and chat identities are retained under
[research/](research/) and [ARTIFACTS.json](research/ARTIFACTS.json).
Subagents are optional for useful independent questions or experiments.

One AMD 5950X, one measurement lock. **Generic Linux stopping first, modest
race/fault simtests next.** Kernel **6.18.54**, microcode **0xa20102e** and the
[host profile](research/host-profile.json) remain pinned. No new AMD hardware slice
or native performance benchmark is queued.

The read-only Mac feasibility check is complete. Park native HVF: no supported
counted-preemption path found, host-time-derived timer and VM/thread ownership
constraints. Portable core tests can run here; a bounded software-emulated Linux
test is an optional later candidate. No Mac guest execution has been tested.

## Open items

- Qualify Linux count-only stopping without a fixture-supplied future RIP.
  The prepared one-process Linux smoke has not run; follow with deciding fresh
  repeats and review if it passes. Broader IRQ/debug transparency stays explicit.
- Close clocks/entropy for a small trusted guest profile; pin a declared virtual
  CPU progress model. Native calibration and JVM/JIT admission can follow.
- Connect per-machine causal frontiers, seeded guest IRQ scheduling, owned
  network/disk devices and the imperative fault protocol.
- Qualify scoped post-boot VM/world starts and crash/restart lifetimes.
- Demonstrate a seed-sensitive race with a fixed control, plus a real modest
  service workload through partition/heal and crash/restart. Check invariants,
  actual fault effects and same-seed failure reproduction across fresh processes.
- Record end-to-end wall time and optimize obstacles to running useful tests.
  There is no fixed 10x speedup, native slowdown or 100 µs stop requirement.

## Closed findings

- Architecture: real Linux/KVM; one thread per world; modeled Linux/Unicorn dropped.
- Execution control: original-profile bounded Linux correctness passes **44/44**,
  with fresh repeats, negative controls and independent acceptance from a pinned
  start. This does not establish arbitrary Linux input closure.
- Exploratory bare stops: **5.30 ms known-RIP / 5.19 ms count-only**, excluding
  hashing and overflow return. Margin 1,500 overshoots by **215 branches** and is
  rejected. The enabled-loaded workaround profile also remains rejected after
  **31/31** sibling-bit persistence failures.
- Components: native world/fault protocol and plain-state restore, synthetic
  witnesses, owned KVM/IRQ, functional Redis, bounded clock/entropy probes,
  workload fixtures and source audits. These are integration inputs.
- Preservation and cleanup: useful artifacts are durable; obsolete prototypes
  and caches were excluded; completed research chats are archived.

A second suitable Linux host would permit independent seed runs and the first
cross-host comparison. Intel and ARM qualification remain deferred.
