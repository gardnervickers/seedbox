# Windowed scheduler

Status: the x86 world scheduler, unqualified. `seedbox-engine-core` holds
the planner and executors (`src/window/`) and the adapter over the shared
world (`src/world_windowed.rs`), generic over `WorldMachine`. The x86 VMM
runs every world through it: static worlds, faults, runtime control,
console triggers, scenario input, `kill_restart`, boot forks and runtime
checkpoints. The legacy loop (`world::simulate`) remains in `engine-core`
only because the arm64 backend still calls it; nothing on x86 does.
`vmm.json` names the scheduler under `scheduler.name`. There is no flag to
choose. `--machine-workers N` and `--machine-cpus` set the thread count and
pinning (performance only); the `seedbox` CLI does not pass them yet, so
CLI runs are windowed on one thread.

The question this prototype answers: can several machines of one world run
concurrently, with results identical for any worker count, and does that
make timebases whose virtual time ignores host compute (PV, or an
instruction-count timebase) worth running?

## Contract

The module documentation (`crates/seedbox-engine-core/src/window/mod.rs`) is normative.
In short:

- A round executes `[T, B)`. `T` is the earliest time any live machine can
  act. `B = T + latency - early`, capped by the duration and any global
  boundary. A frame sent at `s >= T` is due no earlier than `s + latency`
  and deliverable no earlier than `s + latency - early >= B`, so nothing
  produced in a round is due inside it.
- Each active machine gets an immutable assignment: its wake time, the
  queued events it may deliver, its stop, and the stop's tolerance (exact at
  a boundary or the end; otherwise at most `early` and half the round).
- Outputs keep their actual send times. After every machine finishes, the
  coordinator sorts them by (send time, machine, machine sequence), routes
  them through the coordinator-owned policy, rejects any that would be due
  inside the round (`Error::Causality`), and queues them under
  `EventKey = (time, class, source, source sequence)`.
- A designated machine's exit takes effect at the round boundary: the world
  ends after that round commits. `Outcome::Exited` records the exit time as
  an observation and the committed boundary as the world's end.
- Output buffering is bounded per machine per round; exceeding it fails the
  attempt. A machine or worker failure fails the attempt after every worker
  has finished the round, and the round is not committed. Of several
  failures, the lowest machine's is reported.

Three refinements came out of testing. They hold for any machine
implementation:

- **Wake before delivery.** An idle machine whose local clock lags its key
  must first advance to its wake time. Otherwise it delivers events early at
  a stale time and replies before `T`. The assignment carries `wake`, and
  commit rejects outputs before it.
- **Progress with large `early`.** With `2 * early >= latency`, a horizon
  stop tolerance of `early` can leave a machine with nothing to do in the
  next round. Horizon stops may therefore land at most half a round early.
- **Idle machines may stop short.** A halted guest wakes only when it can
  act. If its wake time falls in `[slice end, B)`, it stays where it is.
  A report may end before the slice end only when the machine's next
  action and its first undelivered event are both at or after the slice
  end. Real guests found this case; the randomized tests now include
  machines that wake lazily.

## Evidence so far

`cargo test -p seedbox-engine-core` (portable):

- Planner unit tests cover window arithmetic, exact boundaries and duration,
  early delivery bounded by boundaries, idle-time jumps, quiescence,
  simultaneous-send ordering, a frame due exactly at `B + early`, invalid
  reports, output limits, designated exit, and a negative control: a link one
  nanosecond under the bound must fail with nothing committed.
- The shared mock machine (`src/world_mock.rs`) runs the legacy loop's
  scenarios through the adapter (`src/world_windowed_tests.rs`): control,
  trigger and fault boundaries at equal times, requests and readiness
  arming later boundaries, strict delivery, crash and reboot, pause with
  held frames and owed I/O, time forks with owed I/O, launch barriers and
  release, runtime checkpoints, failed faults, a poisoned world, rejected
  adapter observations, and a world with pause, crash, resume, reboot, a
  trigger, a request, serial input and frames that must give one receipt
  and one routed-effect digest on 1, 2 and 3 workers with and without
  reversed completion order.
- Scripted machines run with serial, 1, 2 and 4 workers, with natural and
  reversed host completion order. They cover ping-pong, simultaneous
  senders, idle guests, a busy guest beside idle ones, exact boundaries,
  exit during a round, worker admission failure, machine failure and 40
  randomized worlds with 2–5 machines, seeded jitter and 2% loss. Machine
  state, round log, routed-effect digest, pending events and dropped frames
  must all match. Every delivery must also land in `[at - early, at]`, the
  "never late" gate.

`tests/window_overhead.rs` is ignored by default. It runs four CPU-burning
machines with 200 ms of simulated time on a 10-core M-series Mac:

| Window | Serial | 4 workers | Speedup |
| --- | --- | --- | --- |
| 1 ms | 0.215 s | 0.063 s | 3.4× |
| 100 µs | 0.217 s | 0.111 s | 2.0× |
| 10 µs | 0.219 s | 0.323 s | 0.7× |

A round costs roughly 10 µs of synchronization on the rayon pool. Windows
of 100 µs or more amortize it. The default 1 ms latency gives 0.5–1 ms
windows.

### AMD host (5950X, `nixos`)

`harness/windowed-matrix.sh KERNEL VMM CPUS` reproduces the thread-count
agreement as root in the dev shell, with the worlds in
`harness/worlds/window2.json` and `window4.json`. It exits nonzero if any
witness disagrees. The old-scheduler rows below are from the engine before
this one became the default.

`guest/window.init` makes four guests that each hash 1.25 GiB from
`/dev/zero` while pinging the next guest 100 times at 20 ms intervals.
Guest `a` then exits. The world uses 1 ms latency with 100 µs jitter and a
2 ms stop tolerance, so windows are 0.5 ms. The guest kernel is built from
`kernel/` at this revision. Each world ran with seed 7 and `--digest`, with
the VMM pinned by `taskset`, workers pinned with `--machine-cpus` to CPUs
4–7, and the Zen MSR workaround set on those cores and their siblings.

| Timebase | Old scheduler | Windowed, 1 worker | 2 workers | 3 workers | 4 workers | 4 workers, reversed |
| --- | --- | --- | --- | --- | --- | --- |
| PV | 3.24 s | 3.26 s | 1.72 s | 1.62 s | 0.99 s | 1.83 s |
| Counter | 4.61 s | 6.86 s | 3.86 s | 3.23 s | 2.31 s | 3.00 s |

All windowed runs of a timebase agree on world, network and full-state
witnesses, guest output, machine times and the committed end. That includes
"reversed", which has `SEEDBOX_WINDOWED_SKEW_US=300` delaying machine `i`
by `(4 - i) × 300 µs` per run. Machines moved between pool threads up to
1,651 times per run. The two-guest variant agrees the same way across 1
and 2 workers and reversed order (PV: 0.43 s serial, 0.23 s with two
threads). The windowed witnesses differ from the old scheduler's, as
expected for a different scheduler.

- **Legacy path unchanged** at the time of measurement: the same worlds
  under the old scheduler gave the same world witnesses as the VMM at
  `6d4f443` (#169's head), for PV and counter.
- **PV:** windowed execution costs nothing serially, and four threads run
  3.3× faster than the old scheduler.
- **Counter:** serial windowed is 40% slower than the old scheduler. Every
  machine takes a counted stop at every round end, where the old scheduler
  let the lagging machine run up to its horizon. Four threads still beat the
  old scheduler by 2.0×. Larger windows (more latency or less early) or
  stops at natural exits would cut that overhead.
- **Three workers** for four machines show the cost of a straggler round:
  one thread runs two machines.
- **Reversed rows** include 300–1200 µs of artificial sleep per round; they
  test determinism, not speed.
- `cargo test-vmm --features kvm-tests` passes.

### Counter stop refinement (2026-10-08)

Profiling `b90a315` on the same 5950X found that the serial regression was
mostly the final breakpoint approach to counted stops. A successful
overflow could land hundreds of branches inside the margin grown by prior
overshoots; the engine then stepped all those branches. The windowed
scheduler exposes this cost more often: `window4` had 656 precision stops,
versus 251 under the old scheduler.

`crates/seedbox-vmm/src/machine/counted.rs` now saves a successful,
unobservable overflow landing and attempts another overflow closer to the
same hard target when more than 128 branches remain. It uses the existing
undo machinery and resets the approach margin to the smaller of the initial
undo margin and a quarter of the remaining count. After rollback, time
equals the held checkpoint's time, so the engine grows the margin as before
instead of restarting the same failed approach. Successful refinements
strictly advance the checkpoint. Short approaches still step immediately.

This changes host work only. It neither publishes guest time nor injects
interrupts, adopts timers, polls devices or draws RNG at an overflow
landing. Natural exits and exact counted stops remain the observable
boundaries. Undo disabled (`--undo 0`) and the PV timebase take their
existing paths. Window arithmetic, admission and report validation,
coordinator order, routed effects, CLI contracts and arm64 are unchanged.

The table gives median **whole-process wall time**, including setup and
full-state hashing, from five interleaved trials per row. The old engine is
`d8caba1`, windowed before is `b90a315`, and after includes the refinement.
All use the same guest kernel (Linux 6.12.112 built from `kernel/`), initramfs,
seed 7, counter timebase, default undo and `--digest`. The host ran Linux
6.18.54. Serial runs used `taskset -c 14,15,30,31`; parallel runs used
`taskset -c 12-15,28-31 --machine-workers 4 --machine-cpus 12,13,14,15`.

| World | Old scheduler | Windowed before, 1 worker | After, 1 worker | Before, 4 workers | After, 4 workers |
| --- | --- | --- | --- | --- | --- |
| `window4` | 5.95 s | 7.95 s | 6.01 s | 3.41 s | 2.58 s |
| `window2` | 1.25 s | 1.66 s | 1.46 s | 1.22 s | 1.09 s |

`window4` improves 24.4% serially and 24.3% on four workers, which now run
2.31 times faster than the old scheduler. `window2` improves 12.2% and
10.4%; only two machines can run concurrently. Shared-host contention
produced parallel outliers: the `window4` before/after four-worker ranges
were 3.39–4.88 s and 2.53–3.65 s. Its serial ranges were 7.78–8.09 s and
5.87–6.07 s.

Profiles of the unmodified engine and before/after machine timers separate
the suspected causes:

- **Precision stepping:** old versus windowed serial `window4` spent
  1.20 versus 2.88 s stepping. After refinement this is 0.86 s, with median
  step exits falling from 385,919 to 122,870. Checkpoint plus rollback time
  grows from 102 to 133 ms. The round count (601), machine-run count (2,400),
  0.5 ms windows and 656 precision stops remain the same. For `window2`,
  stepping falls from 0.506 to 0.265 s and 71,795 to 39,127 exits; extra
  checkpoint/rollback work costs 13 ms. These are sums of machine timers;
  parallel sums are CPU work, not elapsed run time.
- **Counter arming:** entry/return kprobes on `perf_ioctl` measured 7,243
  calls taking 24.7 ms total on one worker, and 7,232 taking 25.0 ms on
  four workers. This includes counter period/enable operations. Direct
  counter ioctl cost cannot explain the approximately 2 s serial gap.
- **vCPU migration:** `synchronize_rcu` entry/return probes recorded zero
  calls on both one- and four-worker runs, despite roughly 1,650–1,750
  machine-to-thread changes in parallel runs. Scheduler-switch stacks
  contained no RCU wait. Sticky placement is not the remedy on this host
  kernel; other kernels need their own measurement.
- **Rayon/coordinator:** one worker calls `run_serial`, so there is no pool
  barrier. The measured coordinator cost was about 2 ms per `window4` run.
  The existing `window_overhead` no-work benchmark measured about 2 ms of
  synchronization over 200 rounds with four workers (about 10 microseconds
  per round); that estimates 6 ms over 601 rounds, not the serial regression.
- **Window size:** diagnostic runs of the unmodified engine with network
  latency increased to 2 and 4 ms (1 and 2 ms windows) took median 6.61 and
  5.52 s over three trials, versus 7.95 s for the original 0.5 ms windows.
  Stepping took 1.61 and 0.76 s, with 555 and 471 rounds. These change network
  semantics and witnesses, so the fix retains the original latency, early
  allowance and window size.

CPU sampling used `perf record -e cpu-clock -F 997 --call-graph dwarf,8192`;
the RCU/counter measurements used paired entry/return kprobes, and blocked
stacks used `sched:sched_switch`. Probes and the Zen MSR workaround were
removed/restored after measurement.

Before/after runs agree on world, network, machine exit and full-state
witnesses, guest console bytes, machine times, routed-effect digest and
committed end. This includes all five trials at one and four workers and
full `--undo 0` runs of both worlds. The windowed witnesses already differed
from the old scheduler's at `b90a315`; that relationship is unchanged.
`harness/windowed-matrix.sh` passes before and after for PV and counter:
one and two workers on `window2`, one through four on `window4`, and
reversed completion order. Each matrix agrees on full state, world and
network witnesses, machine times and the committed end.
Applying only this refinement to the historical old engine also preserves
its original counter and PV witnesses. Full `seedbox run` / `seedbox replay`
checks with a hashing Compose guest give PASS/MATCH before and after, both
with default undo and `--undo 0`, and agree on full-state witnesses and
console bytes. The focused KVM test
`machine::io_boundary_tests::refined_counter_approaches_preserve_ram_and_exact_stops`
compares exact count, exit witness and RAM/register state with undo margins
0, 16, 256 and 1,024 while guest code writes RAM on every loop branch.

## Why this matters most for PV

Under the counter timebase, compute costs virtual time. A busy guest
reaches `B` after a bounded amount of host work, and every round ends in a
counted stop: an overflow approach, plus breakpoint stepping or undo. That
per-round stop cost determines whether counted windows pay. Counted
execution also needs an exclusive, SMT-protected PMU core per worker.

Under PV, compute costs no virtual time. A compute-heavy guest spends host
time while its virtual clock stands still, and today every other machine
waits for it on the single world thread. That is exactly the work
concurrent windows overlap. PV also needs no PMU and no exclusive core, and
it runs nested, so workers are cheap on ordinary cloud VMs. Expected PV
speedup approaches `min(workers, busy machines)`, limited by the slowest
machine in each window. The scheduler does not depend on the timebase. Any
timebase whose time depends only on guest execution, such as instruction
counting, benefits the same way.

## Integration

`crates/seedbox-engine-core/src/world_windowed.rs` adapts the shared world.
It works for any `WorldMachine`; the parallel path needs `M: Send`.

- **Work-stealing pool.** `window::run_parallel` runs each round's
  assignments on a rayon pool. The parallel collect is the round barrier.
  Idle threads take the next machine, so a machine can run on a different
  thread every round. Each thread creates its context (the timebase) on
  itself with `broadcast`, and drops it there after the run. A context must
  therefore hold no per-machine state that affects results.
- **Machine-local loop.** `Guest::run` loops around `WorldMachine::advance`.
  A halted machine wakes at its own next action or the next event's
  nominal time. The loop delivers assigned frames up to `early` ahead, runs
  to the next event or the stop, and timestamps every drained frame at the
  machine's time. Equal-time stalls fail.
- **Coordinator.** `Coord` owns the world's policies for the segment and
  is both the executor's `Router` and its between-rounds `Hook`. The hook
  runs on the calling thread before every plan. It first absorbs the last
  round's effects in a fixed order: receiver outcomes, fault effects (a
  success with id 0 is management readiness), observed console bytes,
  exits, delivered control replies, then control requests, each sorted by
  (time, machine, channel order). It then applies every world boundary no
  live machine can act before, in the reference order (control, then
  triggers, then faults), exactly as the legacy loop did at the same
  times, and registers the next control, trigger, fault, checkpoint and
  fork times as exact round boundaries. Management commands and control
  replies are queued as `Msg::Control` world events at their boundary;
  they are never delivered early. Scenario serial input and partitions are
  ordinary world events; a partition is applied by the router when its
  sender delivers it, in order with the frames the sender routes.
- **Lifecycle.** Pause, resume, crash and reboot run between rounds. A
  paused, down or parked machine is *suspended* in the planner: its key is
  never, so it neither runs nor receives until the coordinator resumes it
  and re-syncs its state. A crash drops the machine's queued management
  commands with the dead incarnation; a reboot drops frames queued before
  it, as before. The router's receiver-side policy (held frames while
  paused, none while down) reads the coordinator's committed lifecycle.
- **Forks and checkpoints.** A boot fork ends at the guest's launch
  barrier (`Stop::Barrier`) or, for a time fork, at an exact boundary at
  the fork time: a machine that reaches it parks once it owes no I/O, and
  the coordinator parks any machine that cannot act before it. The segment
  ends with `barrier` once every machine is parked. A runtime checkpoint
  ends a segment with `checkpoint` as soon as every machine key and world
  boundary is at or after its time, before anything happens there. A later
  segment starts from the world's committed machine states and queues
  (`World::with_machines`), so release, snapshot and restore stay outside
  the scheduler.
- **Router.** The world's link policy (`faults.route`, link RNG and FIFO
  state) is applied by the coordinator in commit order. Frames still in
  flight and undelivered management go back into `world.events` and
  `world.strict_events`, so the report's network witness covers them.
- **x86 VMM.**
  - `Machine` is `Send`, and the disk's shared base image is now an `Arc`.
  - Each pool thread opens its own `Counter` or `PvClock` and pins itself to
    its `--machine-cpus` entry.
  - Counter overflow accounting is thread-local (`counter.rs`), so another
    thread's overflow cannot cause a re-arm.
  - PV watchdog and spin-probe state (`PvProgress`) belongs to each
    `Machine`, not to the thread's `PvClock`. The clock keeps only the
    watchdog timer and per-run scratch, so no machine's watchdog or probe
    decisions carry into another's run on the same thread. A checkpoint
    inside a probe is refused by `Machine::snapshot`, and `Machine::restore`
    resets the state.
  - Only the owning raw pointers (guest RAM, time page, snapshot mappings,
    the undo shadow) are `Send`, through the `SendPtr` newtype, so `Machine`
    is `Send` by derivation. A future non-`Send` field still fails to
    compile.
- **Scope.** `--machine-workers` and `--machine-cpus` apply to every run.
  A delivery that fails (a control frame the guest cannot take) fails the
  run instead of ending it as `fault_failed`.

## Remaining plan

1. **CLI.** `--machine-workers N` is performance-only and
   overridable on replay. The executor leases `N` SMT-protected cores for
   the counter timebase, or any `N` CPUs for PV, and passes `--machine-cpus`.
   Neither can be combined with `explore --workers > 1` or with the
   unsupported features. Update help, README, runtime docs and the project
   template.
2. **Reports.** Show the end machine's exit time beside the committed end in
   the CLI report. Today they appear only in `vmm.json`, under `scheduler`.
3. **Sticky placement.** Measure migration costs on each host kernel before
   changing placement. Linux 6.18.54 showed no `synchronize_rcu` calls during
   these runs, despite frequent migration; see the counter refinement
   measurements above.
4. **Counter overhead.** The refinement above cuts precision stepping while
   preserving existing stops. Larger windows and soft stops at natural
   exits near `B` would need separate scheduler-semantics evidence.
5. **Real workload.** Run the four-machine etcd Compose workload through
   `seedbox run` / `seedbox replay` once the CLI passes the flags.
6. **arm64.** The adapter is generic and now covers everything the legacy
   loop did, so the arm64 backend can call `world_windowed::simulate` with a
   `Run` in place of `world::simulate` / `simulate_launch`, after which the
   legacy loop can go. The parallel path also needs its machine to be `Send`.
