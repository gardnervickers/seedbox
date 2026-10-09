# Windowed scheduler

Status: the world scheduler on both backends. `seedbox-engine-core` holds
the planner and executors (`src/window/`) and the adapter over the shared
world (`src/world_windowed.rs`), generic over `WorldMachine`. The x86 VMM
runs every world through it: static worlds, faults, runtime control,
console triggers, scenario input, `kill_restart`, boot forks and runtime
checkpoints. The arm64 world (`seedbox-arm64/src/application/world.rs`)
runs its launch barrier and main segment through it too; each ARM machine
is a stopped worker process, so the pool threads carry no clock. The legacy
loop (`world::simulate`) remains in `engine-core` only for its tests; no
backend calls it. `vmm.json` names the scheduler under `scheduler.name`.
There is no flag to choose. `--machine-workers N` (both engines) and
`--machine-cpus` (x86) set the thread count and pinning (performance only);
the `seedbox` CLI does not pass them yet, so CLI runs are windowed on one
thread.

The question this prototype answers: can several machines of one world run
concurrently, with results identical for any worker count, and does that
make a PV clock, whose virtual time ignores host compute, worth running?

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

### Linux KVM host (5950X, `nixos`)

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
the VMM pinned by `taskset` and workers pinned with `--machine-cpus` to CPUs
4–7.

| Old scheduler | Windowed, 1 worker | 2 workers | 3 workers | 4 workers | 4 workers, reversed |
| --- | --- | --- | --- | --- | --- |
| 3.24 s | 3.26 s | 1.72 s | 1.62 s | 0.99 s | 1.83 s |

All windowed runs agree on world, network and full-state
witnesses, guest output, machine times and the committed end. That includes
"reversed", which has `SEEDBOX_WINDOWED_SKEW_US=300` delaying machine `i`
by `(4 - i) × 300 µs` per run. Machines moved between pool threads up to
1,651 times per run. The two-guest variant agrees the same way across 1
and 2 workers and reversed order (PV: 0.43 s serial, 0.23 s with two
threads). The windowed witnesses differ from the old scheduler's, as
expected for a different scheduler.

- **Legacy path unchanged** at the time of measurement: the same worlds
  under the old scheduler gave the same world witnesses as the VMM at
  `6d4f443` (#169's head).
- Windowed execution costs nothing serially, and four threads run
  3.3× faster than the old scheduler.
- **Three workers** for four machines show the cost of a straggler round:
  one thread runs two machines.
- **Reversed rows** include 300–1200 µs of artificial sleep per round; they
  test determinism, not speed.
- `cargo test-vmm --features kvm-tests` passes.

The [window4 host profile](windowed-scheduler-profile.md), based on main at
`cc2af34`, has five interleaved trials at one and four workers, with
separate scheduler and whole-process medians, ranked costs and replay evidence.

## Why this matters for PV

Under PV, compute costs no virtual time. A compute-heavy guest spends host
time while its virtual clock stands still, and today every other machine
waits for it on the single world thread. That is exactly the work
concurrent windows overlap. PV needs no exclusive core and runs nested, so
workers are cheap on ordinary cloud VMs. Expected speedup approaches
`min(workers, busy machines)`, limited by the slowest machine in each window.
The scheduler does not depend on the clock model.

## Integration

`crates/seedbox-engine-core/src/world_windowed.rs` adapts the shared world.
It works for any `WorldMachine`; the parallel path needs `M: Send`.

- **Work-stealing pool.** `window::run_parallel` runs each round's
  assignments on a rayon pool. The parallel collect is the round barrier.
  Idle threads take the next machine, so a machine can run on a different
  thread every round. Each thread creates its context (the clock) on
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
  - Each pool thread opens its own `PvClock` and pins itself to its
    `--machine-cpus` entry.
  - The thread's `PvClock` keeps only the watchdog timer and per-run
    scratch; the stall counter the watchdog reports from belongs to each
    `Machine`, and `Machine::restore` resets it.
  - Only the owning raw pointers (guest RAM, time page, snapshot mappings)
    are `Send`, through the `SendPtr` newtype, so `Machine`
    is `Send` by derivation. A future non-`Send` field still fails to
    compile.
- **Scope.** `--machine-workers` and `--machine-cpus` apply to every run.
  A delivery that fails (a control frame the guest cannot take) fails the
  run instead of ending it as `fault_failed`.

## Remaining plan

1. **CLI.** `--machine-workers N` is performance-only and
   overridable on replay. The executor leases `N` CPUs and passes
   `--machine-cpus`.
   Neither can be combined with `explore --workers > 1` or with the
   unsupported features. Update help, README, runtime docs and the project
   template.
2. **Reports.** Show the end machine's exit time beside the committed end in
   the CLI report. Today they appear only in `vmm.json`, under `scheduler`.
3. **Sticky placement.** Linux 6.18.54 made no `synchronize_rcu` calls on
   migration; measure other host kernels before changing placement.
4. **Real workload.** Run the four-machine etcd Compose workload through
   `seedbox run` / `seedbox replay` once the CLI passes the flags.
5. **Legacy loop.** With arm64 on the windowed scheduler, `world::simulate`
   and `simulate_launch` have no backend caller; move their remaining tests
   onto the windowed adapter and delete the loop.

## arm64

The arm64 world calls `world_windowed::simulate` (one thread) or
`simulate_parallel` (`--machine-workers N`, 1..64) for its launch segment
(`fork: Some(None)`, `require_barrier: false`) and its main segment. `ArmMachine`
is `Send`: its console and management captures sit behind `Arc<Mutex<_>>`,
shared only between a machine and its crash successor, and its transcript is
its own. Each machine hashes the slice horizons it was given and the causal
events it reported, in its own order; the world witness combines those
per-machine digests in machine order (`seedbox-arm64-world-witness-v2`). That
replaced the old world-wide chain hashed in execution order, which is why
arm64 witnesses changed once with this port and why they no longer depend on
which thread ran a machine or when a round finished.

Measured on an M-series Mac (14 cores), `harness/acceptance/workloads/mac-capabilities.yaml`
(3 machines), engine run directly on the retained world with `--machine-workers`
1, 2, 4 and 4 again: identical `world_witness`, `network_witness`,
`horizon_witness`, per-machine witnesses and state digests, 163 rounds each,
with 343, 353 and 361 migrations on 2, 4 and 4 workers. A CLI `run` with
`--full-state` followed by `replay` reports MATCH.
