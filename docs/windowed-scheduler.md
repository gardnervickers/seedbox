# Windowed scheduler (experimental)

Status: experimental, unqualified. `seedbox-engine-core` holds the
planner and executors (`src/window/`) and the adapter over the shared
world (`src/world_windowed.rs`), generic over `WorldMachine`. The x86 VMM
runs static worlds through it with the engine flags `--scheduler windowed`,
`--machine-workers N` and `--machine-cpus`; the `seedbox` CLI does not pass
them yet. Without the flag, the VMM's witnesses match the preceding engine.

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

`harness/windowed-matrix.sh KERNEL VMM BASE_VMM CPUS` reproduces this as
root in the dev shell, with the worlds in `harness/worlds/window2.json` and
`window4.json`. It exits nonzero if any witness disagrees.

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

- **Default path unchanged.** The same worlds without `--scheduler` give the
  same world witnesses as the VMM at `6d4f443` (#169's head), for PV and
  counter.
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
- **Router.** The world's link policy (`faults.route`, link RNG and FIFO
  state) is applied by the coordinator in commit order. Receiver outcomes
  and management readiness are applied after the run in sorted order. Frames
  still in flight go back into `world.events`, so the report's network
  witness covers them.
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
- **Scope.** Runtime control, faults, scenarios, `kill_restart`, boot forks
  and checkpoints are rejected before boot. Console triggers, control
  requests and fault effects fail the run if they appear.
  `--machine-workers` and `--machine-cpus` require `--scheduler windowed`.

## Remaining plan

1. **CLI.** `--scheduler windowed` becomes part of run identity and is
   inherited by replay. `--machine-workers N` is performance-only and
   overridable on replay. The executor leases `N` SMT-protected cores for
   the counter timebase, or any `N` CPUs for PV, and passes `--machine-cpus`.
   Neither can be combined with `explore --workers > 1` or with the
   unsupported features. Update help, README, runtime docs and the project
   template.
2. **Reports.** Show the end machine's exit time beside the committed end in
   the CLI report. Today they appear only in `vmm.json`, under `scheduler`.
3. **Sticky placement.** Each time a vCPU's `KVM_RUN` comes from a new
   thread, KVM reassigns `vcpu->pid` and calls `synchronize_rcu()`.
   Preferring the thread that last ran a machine, and stealing only when
   idle, would keep the balance and drop most of that cost. Measure it once
   the CLI passes the flags.
4. **Counter overhead.** Measure larger windows and soft stops at natural
   exits near `B`.
5. **Real workload.** Run the four-machine etcd Compose workload through
   `seedbox run` / `seedbox replay` once the CLI passes the flags.
6. **arm64.** The adapter is generic, so the arm64 backend can use the
   serial path as is. The parallel path also needs its machine to be `Send`.
