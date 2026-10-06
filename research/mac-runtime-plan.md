# Mac runtime exploration: rough plan for issue #8

Plan and progress, 2026-10-06. Originally based on `fa51542`; now rebased onto
`origin/main` at `4c7ed71aefc86d214644cd997512204293bdd205` after PR #16 merged.
The production runtime is unchanged. The initial input-control probe remains
in [mac-tcg/README.md](mac-tcg/README.md). The
[service integration report](mac-tcg/SERVICE_REPORT.md) records reuse of the
merged fixture, real etcd faults/recovery, five matching fresh runs and a scoped
execution witness. The merge watch has stopped.

## Recommendation

Pursue an optional local, single-kernel QEMU TCG profile with fixed instruction
counting, using the small whole-run contract demonstrated by the bounded probe.
Extract production interfaces only alongside #2's run API. Keep the existing
qualified KVM path working throughout. Independent-machine local TCG requires
the separate two-machine probe below; remote KVM remains complementary.

Separate three decisions: execution engine (KVM/TCG/etc.), deployment topology
(independent machines or services sharing one kernel), and run location
(local/remote). A single-guest service experiment can answer the cheap Mac
feasibility question without deciding the general multi-machine architecture.
Do not silently map #2's one-service-per-machine interface onto shared-kernel
namespaces. That needs an explicit topology/profile choice and stated limits.

## Coordination with active issue #10

**Current handoff:** PR #16 merged on 2026-10-06. Its final source and
`research/single-vm-network/REPORT.md` were read before TCG integration. The
Mac adapter reuses the reviewed network, member lifecycle and checker code.
The historical coordination notes below explain the wait; their preliminary
speed ratios are not current qualification of the final fixture or Mac TCG.

Issue #10 owns the single-guest namespace/network/fault fixture and its KVM
qualification. Reuse its pinned artifact/source handoff rather than building a
second fixture here. On 2026-10-06, the active chat "Investigate Seedbox performance
architectures" reported repeated healthy/fault witnesses and state digests, a
detected planted failure, and a matched fixed-work comparison of 4.83 seconds
versus 27.31 seconds for four guests. Evidence packaging and a draft PR were
still in progress; this is a provisional handoff report, not independently
reviewed evidence, Mac performance or a bug-finding-rate result.
The later active-chat update reports ongoing fixes to process-stop ownership/
reaping, directed pair cuts and completed-action timestamps. Wait for the final
pinned fixture/evidence; the earlier metrics do not qualify that changed code.

Proceed now with TCG boot, clock/entropy/device closure, build preparation and
the contract sketch. Consume a pinned #10 revision once its evidence is ready;
do not modify its active worktree, jobs or fixture. Requalify the reused fault
controls on TCG: changing the kernel, clock and execution engine may change
guest scheduling and fault timing even with identical service binaries.

For the first Mac slice, prioritize the whole-run contract used by #2: prepared
inputs plus seed/profile in, artifacts and structured result out. The machine
advance contract below is the candidate for sharing the multi-machine world;
it is not a requirement to force guest-local packets through the host or to
port that scheduler before measuring the single-guest path. Shared fault
semantics and evidence requirements can have host-fabric and guest-local
implementations, each explicitly qualified.

If #10's evidence holds and TCG qualifies, a restricted shared-kernel local
profile can be the first #8 recommendation. Keep the two-machine probe as a
separate follow-up for independent-machine local support, not a prerequisite
for that recommendation. If #10 exposes a fault-control limitation, preserve
it as a capability restriction or blocker; do not quietly substitute loopback.

## What the current implementation tells us

- `sim/vmm/src/main.rs` owns scheduling, ordered event queues, the network
  fabric, partitions, named RNG streams, and scenario generation. However,
  `World` contains a concrete `Machine` and `Counter`; boot and reporting also
  access KVM-specific fields. There is no interchangeable runtime today.
- `Machine::run_until(counter, limit, early, frames)` is the useful starting
  seam. It stops deterministically, returns early for transmission, and must
  never pass a hard limit. `State`, `next_deadline`, `has_pending_irq`, `wake`,
  frame delivery, and management input complete the scheduler interaction.
- `machine.rs` combines Linux loading, CPUID/entropy/time policy, device
  emulation, execution control, witnesses and metrics. Wrapping KVM ioctls or
  swapping only `Counter` would leave most portability coupling intact.
- Partition booleans are checked when routing a send. Already queued frames
  remain deliverable. Process faults are serial commands to `cluster.init`;
  `/data` lives in tmpfs and survives process restart. The host schedules
  commands but does not yet validate structured application acknowledgments.
  These are current behaviors, not completion of #3's reusable fault contract.
- `research/world/protocol-contract.json` (removed; see commit `bc07f35`) supplies useful command ownership,
  handles, acknowledgments and ordering ideas. Its modeled storage/snapshots
  are not implemented VMM guarantees; its branch/RIP-specific boundary shape
  should not become the generic backend ABI.
- `CONCLUSIONS.md` D005 describes exact delivery, whereas the current runner
  permits bounded early network/scenario delivery and tolerated timer stops.
  Record the implemented timing profile explicitly; do not accidentally change
  that behavior during an extraction or label all profiles equivalent.

## Minimum candidate contracts

These are responsibilities to validate, not a frozen Rust trait or plugin ABI.
An in-process adapter is enough initially; no dynamic plugin framework is needed.

| Contract | Minimum information or operations | Ownership |
| --- | --- | --- |
| Admit and create | Pinned boot inputs, stable machine identity/incarnation, named entropy seed, execution/timing profile, required capabilities; reject unsupported inputs | Backend handles CPU/kernel/device admission; preparation and manifests remain outside it |
| Observe and advance | Current simulated time and running/blocked/exited state; sound next possible activity bound; advance to a hard horizon with explicit legal stop window; stopped boundary and committed timestamped outputs | World chooses horizons/order; backend owns counters, stepping, guest timers and CPU state |
| Apply controlled input | Timestamped frame/control delivery and explicit acceptance/error at a stopped boundary; idle advancement only to a proven safe point | World owns routing/fault policy; backend or guest adapter owns device/control transport |
| Collect evidence | Versioned execution witness, workload completion, console/control observations, optional declared state digest, backend-specific metrics | Shared result vocabulary; backend declares exactly what its evidence covers |

Illustratively: `advance(until, stop_window) -> Boundary { time, state,
outputs, witness_delta }`. This must also accommodate observations needed to
recompute wake bounds. Progress units (conditional branches versus instructions)
and their virtual-time conversion are part of the pinned profile, not a common
raw counter field. CPU registers and PMU details belong in backend diagnostics.
Move the shared perf-counter lifetime inside the KVM backend/session, preserving
its existing serialized use across machines.

A boundary publishes only committed effects, after required I/O completion.
Speculative rollback, debug steps and host scheduling must not consume world
RNG, publish frames/observations or change witnesses. A send ends execution soon
enough for the world to recompute causal horizons. Equal-time order, early/late
allowances, zero-latency control paths and idle jumps need explicit rules.
Backend worker processes must never make host arrival order semantic. QMP
pause/resume alone is not evidence of a deterministic stopping interface.

Keep these outside the execution contract:

- Compose/OCI preparation and the configure/run/inspect/replay UX (#2).
- Scenario programs, simulated sleeps, fault handles and application oracles
  (#3). A guest control channel reports correlated requested/applied/failed
  actions with simulated timestamps. It stays available through application
  network partitions; health checks still use the affected application path.
- A local/remote transport wrapper around a complete run. Remote execution
  transfers pinned inputs and retrieves artifacts, not live scheduling decisions.

Snapshots, machine reboot and durable storage are optional future capabilities,
with absent capabilities rejected explicitly. Do not add empty mandatory hooks
or equate #1's private undo checkpoint with a complete world snapshot. Preserve
room for machine incarnations, stale-command rejection, surviving writes and
volatile/durable distinctions required by #6 and #7.

## Options and current evidence

| Path | Why investigate | Main unresolved obligation |
| --- | --- | --- |
| Local x86 Linux on QEMU TCG | Retains amd64 service binaries; software instruction budgets avoid the host PMU dependency | Close clock/entropy/device inputs; measure real services on the Mac; establish deterministic control of multiple machines |
| Native ARM virtualization on macOS | Potential hardware speed | Prior SDK review found gaps in counted exits, clock/entropy closure and VM ownership; also qualify ARM atomics and guest artifacts before promising support |
| Linux inside a Mac VM | Could reuse Linux preparation/tooling | ARM Linux is not the qualified x86 AMD profile; independently verify nested architecture, KVM and guest counters. TCG inside it adds another layer and still needs the same controls |
| Mac client, remote qualified KVM | Reuses current execution profile for larger campaigns | Input/artifact transfer, host reservations, connectivity and local inspection/replay UX; execution is not offline/local |

[QEMU's icount documentation](https://www.qemu.org/docs/master/devel/tcg-icount.html)
describes instruction budgets and virtual time, with no multithreaded TCG support.
Use a pinned fixed conversion; wall-clock adaptation cannot select simulation
ordering. Its [record/replay facility](https://www.qemu.org/docs/master/system/replay.html)
records external nondeterministic inputs. Successful replay of such a log would
not establish Seedbox's fresh seed-only generation requirement.

The [bounded experiments reported on #1](https://github.com/gardnervickers/seedbox/issues/1#issuecomment-6018146628)
found repeatable console observations in three icount microbenchmark runs on
Linux, but not a Seedbox service integration or a Mac measurement. Their stock
QEMU guest did not implement Seedbox's private time-page/PV ABI. Reuse retained
inputs if available and verify their hashes; do not treat issue prose as runnable
evidence or reuse its ordinary-KVM speed ratio as a Seedbox performance forecast.
The [local SDK review](evidence/mac-feasibility/findings.json) is also source
evidence, not a runtime qualification of native HVF or nested virtualization.

## Work sequence and decision gates

1. **Inventory and agree on semantics.** Write a compact capability/timing
   matrix and map the four contracts above onto current code. Identify which
   guarantees are required for independent machines versus a shared guest.
   Coordinate with #1's in-flight KVM/PV work and #10's network fixture; do not
   refactor their execution loops or change the default backend in this phase.
   Explicitly propose any changes needed to D001's literal one-host-thread rule
   or D002's branch-count mechanism; QEMU's auxiliary threads are not silently
   covered by a claim of serialized vCPU execution.

2. **Probe local service feasibility.** Build a private, Nix-pinned TCG fixture
   on the Mac with one x86 Linux guest and one vCPU. Use three real etcd members
   plus the existing client/checker, reusing #10's pinned namespace fixture
   once available. Do not duplicate its networking implementation. Pin a
   QEMU-compatible kernel separately from the KVM PV
   kernel; prepare/cache amd64 guest artifacts through project Nix tools and
   document any Linux builder needed for first setup. Cached runs should work
   offline. Audit guest clocks, boot/runtime entropy, CPU features, timers and
   every enabled input/device; preload seeded scenario commands so host serial
   timing cannot choose fault application times. Use controlled guest-local
   packet paths with explicit latency, partition/heal and process restart.
   Do not call unseeded netem jitter deterministic because its timers are virtual.

   Start with a boot/control smoke, then a declared bounded service window and
   ten-minute aggregate execution budget; retain timeouts and stop/reassess if
   boot consumes it. Attempt five fresh processes for one seed and at least two
   additional seeds. Compare a declared execution witness, fault application
   history and application outcome; console equality alone is insufficient.
   Require observed blocked traffic, post-heal delivery and recovery, process
   lifecycle evidence, seed sensitivity, a no-fault control and the existing
   planted checker-failure control. Run the 3.5.3 control over the same bounded
   seed set when 3.5.2 runs complete; failure to discover the historical bug in
   this small budget is not itself a backend failure. Report boot/setup/wall
   time, memory, operation counts and completed seeds. This is a feasibility
   gate, not a campaign-effectiveness or cross-backend witness claim.

3. **Decide whether to pursue two-machine integration next.** A successful
   restricted single-guest profile can complete the initial recommendation;
   independent-machine support remains an explicit follow-up. When pursuing
   it, test whether a small QEMU adapter can stop at a specified
   instruction/time budget, emit timestamped sends, accept controlled incoming
   frames and advance a halted guest without host-time input. Use a source/API
   spike first; stock process orchestration may be insufficient and a narrow
   QEMU modification may be necessary. Bound that work before committing to a
   fork. Exercise competing sends/deadlines, idle wakeups, CPU-bound loops,
   overlapping partitions, in-flight packets and recovery. If this fails, label
   a successful shared-guest prototype as a restricted profile and retain
   independent-machine KVM support; document the remaining local multi-machine
   blocker rather than broadening claims.

4. **Extract the demonstrated common boundary.** First establish the whole-run
   adapter and shared result/profile vocabulary without changing the existing
   KVM execution loop. Only when the multi-machine probe establishes what is
   needed, separate portable world/scenario/result logic from KVM
   loading/execution/devices in small changes. Preserve KVM timing and witnesses
   for identical pinned inputs across the extraction; do not require TCG and
   KVM to share witnesses, seed schedules or snapshots. Use an adversarial fake
   backend for portable scheduling/contract checks, plus hardware integration
   checks on a reserved qualified runner. Gate any changed kernel/profile as a
   new run identity. Keep backend-only dependencies out of portable builds.

5. **Recommend and integrate.** Produce a measured options report and concrete
   follow-ups, including maintenance cost for any QEMU changes. Feed backend,
   topology, timing/capability profile, binary/device versions and witness schema
   into #2's immutable run manifest. Replay verifies recorded inputs and uses
   the original supported profile. #4 runs the applicable shared acceptance
   cases; #5 owns admission/preflight. Outcomes retain PASS/FAIL/ERROR/TIMEOUT/
   INCOMPLETE distinctions. Unsupported fault/topology requests fail validation.

## Project goals that must remain intact

Network/process fault semantics stay shared and testable: independent handles,
validated targets, requested versus applied times, deterministic equal-time
order, declared in-flight policy, explicit failures and demonstrated recovery.
The current send-time partition behavior should remain unchanged during a
mechanical extraction; any stronger/different policy is a versioned #3 change.
Scenario sleeps and progress-dependent decisions remain in simulated time.

A shared-kernel fast profile cannot stand in for independent reboot, per-machine
clock skew or durable-device failure. #6's synthetic suffix loss must be labeled;
real power-loss claims need a device durability model. #7 needs persistent data
through process restart and verified catch-up. Neither requirement should be
implemented as a generic backend kill operation.

No large refactor is a prerequisite to learning whether useful local simulations
are practical. The first deliverable is evidence plus a revised contract sketch;
production interface changes follow that evidence.
