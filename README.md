# Seedbox

A deterministic world of real Linux machines on KVM, run by one host thread.
Applications are described in Compose-shaped YAML and packaged as OCI images.
A seed plus the retained inputs (kernel, images, resolved configuration) regenerate
the same execution; seeds vary guest and simulator randomness, including timer and
interrupt timing.

## Quick start

`validate`, `init` and `inspect` run anywhere. `run` and `replay` need the
qualified host (see [host requirements](#host-requirements)) and root.

```sh
nix develop                          # Rust, OCI tools, static BusyBox, musl
bash kernel/build.sh                 # guest kernel -> ~/seedbox-sim/artifacts/bzImage
cargo build --release                # target/release/seedbox
cargo build-vmm                      # crates/seedbox-vmm/target/release/seedbox-vmm (alias)

cargo run -- validate examples/etcd
sudo -E env PATH="$PATH" target/release/seedbox run examples/etcd
sudo -E env PATH="$PATH" target/release/seedbox replay .seedbox/runs/ATTEMPT
cargo run -- inspect .seedbox/runs/ATTEMPT
```

`seedbox run` with no arguments runs the Compose project in the current
directory. Every runtime input has a default, so no flags are required:

| Input | Default | Override |
| --- | --- | --- |
| Compose file | `compose.yaml` in the given directory (default `.`) | positional path |
| Seed | `x-seedbox.seed` from the Compose file | `--seed` |
| CPU | first free core other than CPU 0's, under the shared lease | `--cpu`, `SEEDBOX_CPU` |
| Guest kernel | `bzImage` in `$SEEDBOX_ARTIFACTS`, `$SEEDBOX_WORK/artifacts`, `./artifacts`, then `~/seedbox-sim/artifacts` for the user and any sudo caller | `--kernel`, `SEEDBOX_KERNEL` |
| VMM | `seedbox-vmm` next to an installed `seedbox`, then this checkout's `cargo build-vmm` output, then on `PATH`; `cargo run` builds it if missing | `--vmm`, `SEEDBOX_VMM` |
| Guest supervisor | built into `seedbox` from [`guest/compose-init.c`](guest/compose-init.c) with musl | `--guest-init`, `SEEDBOX_GUEST_INIT` |
| BusyBox | `SEEDBOX_BUSYBOX` (set by `nix develop`), then a static `busybox` on `PATH` | `--busybox` |
| State | `.seedbox/` | `--state`, `SEEDBOX_STATE` |

`seedbox help run` lists every option. A run prints `OUTCOME: run-directory`;
exit status 0 is PASS (or replay MATCH), 1 FAIL (MISMATCH), 2 ERROR, 3
INCOMPLETE and 124 TIMEOUT.

## Layout

| Path | Contents |
| --- | --- |
| [`crates/seedbox/`](crates/seedbox/README.md) | Configuration, OCI preparation, orchestration and the `seedbox` CLI |
| `crates/seedbox-vmm/` | KVM VMM and world scheduler (`seedbox-vmm`); its own workspace and lockfile, because its binary hash is the run's engine identity |
| `examples/` | Runnable Compose applications: etcd cluster and Redis, each with a workload image |
| `kernel/` | Guest kernel patch, config and build script (Linux 6.12.112) |
| `guest/` | Generic OCI guest supervisor plus low-level guest fixtures and builders |
| `docs/` | [Vision](docs/vision.md), [runtime instructions](docs/runtime.md), [backends](docs/backends.md), [acceptance record](docs/acceptance.md) |
| `harness/` | Engine acceptance suite, legacy world-file campaign runners and fixtures ([migration map](harness/worlds/README.md)), `msr54.py` |
| [`tools/mac-tcg/`](tools/mac-tcg/README.md) | Experimental local Mac TCG runner and tests; separate from the production KVM backend |

## How execution is controlled

- **Progress and time.** Guest progress is the host perf count of retired
  conditional branches (guest only, kernel and user). Virtual time advances
  1 ns per branch; idle machines jump to their next event.
- **Stops.** Events (timer deadlines, frames, scenario commands) are delivered
  at deterministic points only: a natural VM exit, the guest's next syscall
  (a small kernel hook offers that exit), or an exact branch count reached by
  overflow interrupt plus hardware-breakpoint stepping (no TF). A timer
  interrupt fires at the first such point at or after its deadline, at most
  `stop_tolerance_ns` late. An overflow interrupt that skids past a stop, or
  past a syscall that would have been one, fails the run rather than moving
  the stop.
- **Guest kernel changes** (`kernel/`): kernel TSC reads return VMM-published
  virtual time; user RDTSC/RDTSCP trap via CR4.TSD and are emulated; a syscall
  hook offers yield exits.
- **Network and scheduling.** virtio-net over virtio-mmio into an in-memory
  fabric with seeded latency/jitter and directed partitions. The world runs the
  machine furthest behind in slices that never pass its causal horizon (the
  other machines' times plus link latency) or its next event. A slice may end
  at the first deterministic point in the last half-latency before that
  boundary (capped by `stop_tolerance_ns`), and events are delivered at the
  first stop up to that long before they are due: early by a bounded amount,
  never late, and never before they were sent.
- **Witness.** Each machine hashes every deterministic exit; `--digest` adds
  a hash of registers and all guest RAM; `--trace` writes every exit.

## Host requirements

- x86-64 AMD Zen host with KVM, run as root (guest-only perf counting).
- **The Zen SpeculativeLockMap workaround must be on** for the cores running
  simulations (MSR 0xC0011020 bit 54, rr's workaround). Without it, branch
  counts drift on lock-heavy code (e.g. Go) and runs diverge. The Rust executor
  acquires the shared CPU/SMT leases, sets
  it with readback, then restores the exact originals after reaping its child.
  The legacy `batch.sh` runner uses the same lease protocol.

## Applications

`seedbox init my-app --template etcd` (or `redis`) creates a self-contained
project; the checked-in [`examples/etcd`](examples/etcd/compose.yaml) and
[`examples/redis`](examples/redis/compose.yaml) run directly. Their
Dockerfiles package the workload; image defaults and Compose overrides launch the
services. No application-specific initramfs, boot arguments, or world JSON needs
to be authored. The existing etcd acknowledgement-ledger checker is unchanged and
now lives with its Compose example. Generated projects include their own source.

Each attempt retains exact image manifests/layers, kernel, guest archives,
configuration, engine, logs and result. Replay uses the retained content store
without registry pulls or builds. PASS, FAIL, ERROR, TIMEOUT and INCOMPLETE are
separate results; a reproduced FAIL is a successful MATCH. `--full-state` adds
per-machine RAM/register comparisons. `--margin` and `--undo` change performance without
changing semantic identity. Application storage is private RAM-backed state for
each run; the default is 1024 MiB per service. See [runtime instructions](docs/runtime.md)
for compatibility, memory sizing, root access, cancellation and the supported subset.

The [acceptance record](docs/acceptance.md) includes real etcd/Redis passes,
intentional failures, exact replays, full-state comparisons, timeout/cancellation,
lease contention and exact MSR restoration on the workstation.

## Legacy fault campaigns and engine fixtures

The no-fault application definition `worlds/cluster.json` has been replaced by the
etcd Compose example. The old `kill-3.5.2`, `kill-3.5.3` and fault-bearing `sanity`
worlds remain reproducible through `batch.sh`/`campaign.sh`. Compose controllers use
the [runtime fault RPC](crates/seedbox/README.md#structured-faults), with a
[Python recovery example](examples/faults/README.md).
The fixed-delay and single-VM worlds below remain engine comparison fixtures;
they have different placement and scheduling semantics from the Compose examples.
See [the migration map](harness/worlds/README.md) before choosing a runner.

Legacy batches pin runs to consecutive cores from `CPU0` (default 2), use `VMM`,
and require shared core leases and verified counter workaround state. Campaigns
retain separate attempt directories, accept a client result only alongside valid
terminal JSON and zero process status, and classify incomplete attempts as
UNRESOLVED. `SEEDBOX_ROOT` overrides their default `~/seedbox-sim` directory.
A PMU overshoot aborts a run; these campaigns may retry with a larger
`SEEDBOX_MARGIN`. A deterministic clock bound error is not fixed by changing it.

Stop placement settings affect speed only, never results:

- `SEEDBOX_UNDO` (default 256): undo-able precise stops. Before the final
  approach to a stop, the VMM checkpoints the machine (vCPU state plus a shadow
  of guest RAM synced from the KVM dirty log). It then aims the overflow
  interrupt this many branches short of the stop. If the interrupt skids past,
  the VMM rolls back and retries from twice as far. `0` disables this.
- `SEEDBOX_MARGIN` (`adaptive` or a branch count): how early before a soft stop
  the VMM asks for syscall exits, and the stepping distance when undo is off.
  An overshoot that cannot be undone aborts the run; `campaign.sh` reruns it
  with a larger margin for the identical result.

Deterministic clock advances (timer-port I/O charges and guest time reads)
must also fit in the hard stop. Crossing it fails with `clock bound exceeded`;
no device I/O cost is clamped away, and a larger PMU margin does not fix or
retry this error.

## Engine acceptance suite

This suite currently uses legacy world fixtures and preserves their fault coverage.
Build both versioned cluster archives as described in [the migration map](harness/worlds/README.md),
plus `race.cpio` from `guest/race.c` and `guest/race.init`, before hardware tiers.

`harness/acceptance.sh TIER` runs the checks from
[issue #4](https://github.com/gardnervickers/seedbox/issues/4) in one command,
inside `nix develop`. Tiers are cumulative: smoke runs portable first and
stops if it fails, and full runs smoke.

| Tier | When | Checks | Default cores |
| --- | --- | --- | --- |
| `portable` | Every change, any host | `bash -n` on scripts; runner and suite unit tests; VMM `cargo test` (Linux); `seedbox` crate tests | none |
| `smoke` | Every VMM/kernel change | 1 replay, 2 stop-placement invariance, 3 race demo, 4 checker negative control, 8 fault effects | `--cpus 2-7` |
| `full` | Before merging changes to execution semantics, kernel or runner | smoke plus 5 known failing seed, 6 fixed-version control, 7 cross-build equivalence | `--cpus 2-12` |

```sh
harness/acceptance.sh smoke
harness/acceptance.sh full --performance-only /path/to/old/seedbox-vmm   # also runs item 7
harness/acceptance.sh full --semantics-change                            # kernel/ABI or semantics changed
```

- **1 Replay.** Runs `short-kill-3.5.2` (10 s load) twice as fresh processes,
  then twice more with `--digest`. All witnesses must match: the world witness,
  every per-machine witness and, in the digest pair, every RAM/register digest.
  The report names both comparisons.
- **2 Stop-placement invariance.** The same seed with `SEEDBOX_UNDO=0`,
  `SEEDBOX_UNDO=16` and `SEEDBOX_MARGIN=32768` must give the default run's
  witnesses. Any difference is reported as a determinism bug. If the pinned
  margin completes only after an overshoot retry at a larger margin, the variant
  was not exercised and the check is UNRESOLVED.
- **3 Race demo.** Seeds of `race` must give at least two different lost-update
  counts, and the atomic control must never lose an update. The first failing
  seed is replayed in a fresh process and must match.
- **4 Checker negative control.** `sanity` must end in `RESULT fail` with client
  exit 1. Any other outcome fails the suite.
- **5 Known failing seed.** `kill-3.5.2` seed 4063 must give `RESULT fail` with
  the pinned world witness in 5 fresh processes. If the witness or outcome
  changes, the suite reports `IDENTITY CHANGED` in two cases: the kernel,
  initrd or world hashes differ from the pinned identity, or
  `--semantics-change` was passed. Otherwise it reports a regression. After a
  fresh hunt, record the new seed with
  `harness/acceptance.sh pin --seed S --witness W --build "..."`. This also records
  the current input identity. With no pinned identity, a moved witness is
  UNRESOLVED rather than a regression.
- **6 Fixed-version control.** `kill-3.5.3` seeds 4000–4010 must give zero
  failures and zero UNRESOLVED, and every run must have at least one
  acknowledged kill/restart.
- **7 Cross-build equivalence.** This check runs only for a change declared
  performance-only. The pinned seeds of items 1 and 5 must give identical
  witnesses with the old VMM build and the new one. Without the declaration the
  check is reported as skipped.
- **8 Fault effects and recovery.** In the default `short-kill-3.5.2` run
  (shared with items 1–2), at least one scheduled kill must be acknowledged by
  the member's guest on COM2 (`nX stopped`, then `nX started` in `nX.mgmt`),
  and the client must then report `RESULT pass`. The VMM's `kills` list is only
  the seeded schedule, so it is not taken as evidence that a fault happened. Partition/heal checks will follow when
  reusable partition faults exist outside the one-VM fixture.

Seeds, scenarios, settings and the pinned witness are in
`harness/acceptance/pins.json`, pinned for PR #18's build (ABI 2 kernel, corrected
scheduler). The pinned input identity uses that ABI 2 kernel. If it is not
`artifacts/bzImage`, pass its directory with `--artifacts`; otherwise item 5
reports IDENTITY CHANGED.

Runs go through `batch.sh`, so each one is a fresh process with fresh output
under `runs/acceptance-<tier>-<time>-<pid>/`, and every batch holds the shared
per-core lease. Runs with the same settings share one batch. Batches run
concurrently on disjoint core ranges whose SMT siblings are also disjoint. If a
core is leased elsewhere, the affected runs are UNRESOLVED (`--lease-wait S`
waits instead). Result acceptance follows `campaign.sh`:

- A run counts only with process status 0 and valid terminal JSON for its
  seed. A `client` workload must also print exactly one `RESULT pass|fail` line
  that agrees with its exit status 0/1. Other workloads, such as the race demo,
  are judged by their exit status.
- If `batch.sh` itself fails (for example an MSR restore failure), none of its
  runs are accepted.
- Overshoots and soft overshoots are retried once with margin 98304. A retry
  that still fails is UNRESOLVED.
- `clock bound exceeded` is never retried.
- `SEEDBOX_UNDO`/`SEEDBOX_MARGIN` from the caller's environment are ignored, so
  default runs really use the defaults.

`--artifacts DIR` takes the kernel and initrds by file name from another
directory, such as a newly built ABI kernel.

The report (`report.json` in the run directory) records seeds, input hashes
(kernel, initrds, world JSON, VMM builds), simulated and host seconds,
per-machine exit counts, applied faults, retries, failed attempts with their
evidence, evidence paths and per-check host time. No performance threshold is
applied. The exit status is 0 for PASS, 1 for REGRESSION, 3 for UNRESOLVED
(infrastructure, lease, missing inputs or an incomplete run) and 4 for IDENTITY
CHANGED. 2 is left to usage errors.

Checks never see world files, environment variables or cores. They yield
runner-neutral jobs: a scenario, a seed, a witness mode (`exits` or
`full_state`), performance-only settings, and an optional alternative engine.
They judge normalized results. Only `acceptance/launcher.py` knows about
`batch.sh`. The orchestration API from PR #17 is a Rust library, so Python
cannot call it directly. Its `seedbox run`/`replay` commands now exist; the Compose runtime suite in
`crates/seedbox/tests/` exercises them. A second launcher can read results with
`seedbox inspect --json`. It would map the same fields to a simulation config,
`WitnessMode`, performance parameters and cross-engine execution. Porting the fault jobs also depends on #3. The eventual migration target is
Rust integration tests over `Orchestrator::run`/`replay`. They are pure
comparisons over jobs and results, and `acceptance/pins.json` carries over.

## One-VM network fixture

The experimental one-VM layout runs three real etcd members and their client
in separate network namespaces. It shares a kernel, clock, memory and tmpfs;
it does not provide independent machine reboot or kernel failure coverage.
`kernel/config.options` includes the required veth, netem and ingress filter
features, so one guest kernel can serve both layouts. Adding these features
changes the kernel hash and replay identity for every layout.

From a Linux checkout root in the project shell, with an available assigned
core and an existing etcd binary:

```sh
nix develop
export SEEDBOX_WORK="$PWD" SEEDBOX_ROOT="$PWD"
export CARGO_BUILD_JOBS=1 GOFLAGS=-p=1 GOMAXPROCS=1
mkdir -p src artifacts runs
BUILD_JOBS=1 taskset -c 14 kernel/build.sh
(taskset -c 14 cargo build-vmm)
taskset -c 14 guest/build-netns.sh artifacts/single.cpio src/etcd/v3.5.2/etcd single
taskset -c 14 guest/build-netns.sh artifacts/multi.cpio src/etcd/v3.5.2/etcd multi
CPU0=14 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/single-vm.json netns-one "1009" --digest
CPU0=14 SEEDBOX_TIMEOUT=120 harness/batch.sh harness/worlds/cluster-fixed.json netns-four "1009" --digest
```

Use a fresh output prefix for every invocation. The example CPU is the lane
used for this investigation, not a requirement of the shared tooling. Check
host topology and other jobs before choosing cores; keep SMT siblings unused
by other workloads during measurements. `batch.sh` uses `harness/msr54.py`, keeps
an invocation-specific snapshot, and restores the assigned cores after its
children have finished. Do not reuse the historical fixed-CPU research runner
for new campaigns.

`single-vm-controls.json` exercises packet isolation and directionality;
`single-vm-sanity.json` plants a missing acknowledgement to check the checker;
`single-vm-fault.json` applies a directed partition/heal and a process
stop/restart. These use ordinary world files and the same shared runner.
The whole guest is named `client` so its checker output is consumed by
`campaign.sh` / `campaign-result.py`. A planted checker failure is a control,
not a discovered etcd bug.

COM2 accepts `partition n1` / `heal n1` for a whole-node cut and
`partition n1 n2` / `heal n1 n2` for a directed source-to-destination cut.
`stop n2` waits for the owned child to exit before acknowledgement and before
a later `start n2` can reuse its data directory. A start acknowledgement means
**launched**, not service-ready. `FAULT` acknowledgements include guest
monotonic nanoseconds after the action completes, on the same guest clock as
the client phase marker. Scenario `at_ns` uses the VMM time domain; their
epoch offset is not calibrated here, so subtracting the two values does not
measure delivery latency. Retain both the requested schedule and the guest
acknowledgements when examining faults relative to client activity.
The shared client accepts `--writes N` for N successful writes per worker,
with the load duration remaining a deadline; zero retains timed-load mode.
Both layouts read `seedbox.concurrency`.

These worlds deliberately retain a small fixed-delay, zero-jitter fixture.
They are compatible with the campaign runner but do not port the full
`kill-3.5.2` hunt or establish its bug-finding rate. Before backend selection,
[issue #10](https://github.com/gardnervickers/seedbox/issues/10) requires a
matched optimized PV/undo comparison, qualified guest-local jitter, the full
seeded kill/restart schedule, eight writers for 60 seconds, and etcd 3.5.3
controls over the same seeds. The historical ABI1 timings are in
[the historical networking report](https://github.com/gardnervickers/seedbox/blob/788fc4376b7e2cc96d1bc51ed2ae3d4635d16abd/research/single-vm-network/REPORT.md)
and must not be attributed to these revised sources.
