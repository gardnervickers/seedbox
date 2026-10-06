# Seedbox simulator

A deterministic world of real Linux machines on KVM, run by one host thread.
Applications are described in Compose-shaped YAML and packaged as OCI images.
A seed plus the retained inputs (kernel, images, resolved configuration) regenerate
the same execution; seeds vary guest and simulator randomness, including timer and
interrupt timing.

## Layout

| Path | Contents |
| --- | --- |
| `seedbox/` | Rust configuration, OCI preparation, orchestration and run/replay CLI ([API and scope](seedbox/README.md)) |
| `examples/` | Runnable Compose applications: etcd cluster and Redis, each with a workload image |
| `vmm/` | Rust VMM and world scheduler (`seedbox-vmm`) |
| `kernel/` | Guest kernel patch, config and build script (Linux 6.12.112) |
| `guest/` | Generic OCI supervisor plus low-level guest fixtures and builders |
| `worlds/` | Low-level engine fixtures and legacy fault campaigns; see [migration map](worlds/README.md) |
| `batch.sh`, `campaign.sh` | Thin compatibility commands over the Rust runner |
| `msr54.py` | Sets/clears the AMD Zen counter workaround (see below) |
| `acceptance.sh`, `acceptance/` | Rust acceptance entrypoint and configurable seed samples |

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
  Batch, campaign and acceptance commands use the same Rust executor and lease.

## Build and run applications

From the repository root on the qualified Linux host:

```sh
nix develop ./sim
bash sim/kernel/build.sh                            # or reuse a qualified kernel
cargo build --release --manifest-path sim/vmm/Cargo.toml
cargo build --release --manifest-path sim/seedbox/Cargo.toml --locked
bash sim/guest/build-compose.sh /tmp/seedbox-compose-init

seedbox=$PWD/sim/seedbox/target/release/seedbox
vmm=$PWD/sim/vmm/target/release/seedbox-vmm
"$seedbox" init etcd-demo --template etcd              # redis is also available
"$seedbox" validate etcd-demo/compose.yaml
sudo -n env PATH="$PATH" "$seedbox" run etcd-demo/compose.yaml \
  --seed 1009 --cpu 2 --kernel /path/to/controlled/bzImage --vmm "$vmm" \
  --guest-init /tmp/seedbox-compose-init --busybox "$SEEDBOX_BUSYBOX"

sudo -n "$seedbox" inspect .seedbox/runs/ATTEMPT --json
sudo -n "$seedbox" replay .seedbox/runs/ATTEMPT --cpu 2
```

Choose an available CPU; CPU 2 is illustrative. You can also run the checked-in
[`examples/etcd/compose.yaml`](examples/etcd/compose.yaml) or
[`examples/redis/compose.yaml`](examples/redis/compose.yaml) directly. Their
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
each run; the default is 1024 MiB per service. See [runtime instructions](seedbox/RUNTIME.md)
for compatibility, memory sizing, root access, cancellation and the supported subset.

The [acceptance record](seedbox/ACCEPTANCE.md) includes real etcd/Redis passes,
intentional failures, exact replays, full-state comparisons, timeout/cancellation,
lease contention and exact MSR restoration on the workstation.

## Legacy fault campaigns and engine fixtures

The no-fault application definition `worlds/cluster.json` has been replaced by the
etcd Compose example. The old `kill-3.5.2`, `kill-3.5.3` and fault-bearing `sanity`
worlds remain reproducible through `batch.sh`/`campaign.sh` until
[issue #3](https://github.com/gardnervickers/seedbox/issues/3) supplies the shared
fault implementation. Compose rejects nonempty faults instead of dropping them.
The fixed-delay and single-VM worlds below remain engine comparison fixtures;
they have different placement and scheduling semantics from the Compose examples.
See [the migration map](worlds/README.md) before choosing a runner.

Legacy batches pin runs to consecutive cores from `CPU0` (default 2), use `VMM`,
and require shared core leases and verified counter workaround state. Campaigns
retain separate attempt directories, accept a client result only alongside valid
terminal JSON and zero process status, and classify incomplete attempts as
UNRESOLVED. `SEEDBOX_ROOT` overrides their default repository working directory.
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

Acceptance, batch runs and seed campaigns now call the Rust orchestration API
from #17. `fixture::Fixture` imports existing JSON worlds and retains their
kernel, guest archives and definition in the content store. It preserves guest
order, command lines and fault schedules; replay uses retained bytes even after
the source files change. Compose remains the application authoring interface;
the fixture adapter preserves existing engine/fault coverage while reusable
Compose fault configuration is developed in #3.

Run inside `nix develop ./sim`:

```sh
sim/acceptance.sh portable
# Build the existing guest archives first (see worlds/README.md).
sim/acceptance.sh full --vmm sim/vmm/target/release/seedbox-vmm \
  --artifacts artifacts --cpus 2-7 --output runs/acceptance-new
# Direct CLI invocation skips the portable prerequisites:
sudo -n sim/seedbox/target/release/seedbox acceptance smoke --sim sim \
  --vmm sim/vmm/target/release/seedbox-vmm --artifacts artifacts \
  --cpus 2-7 --output runs/smoke-new
```

`portable` runs shell syntax checks, the MSR utility's tests, Rust orchestration,
runner and acceptance tests, and the VMM tests on Linux. `smoke` adds fresh
exit/full-state replay, stop-placement invariance (undo 0/16 and margin 32768),
race exploration with a clean atomic control, a deliberately planted checker
failure, and acknowledged fault recovery. `full` also checks etcd 3.5.3 over
seeds 4000–4010. Hardware tiers require a qualified Linux KVM host and root.

There is **no historical failing-seed or witness pin**. Changing execution
semantics may change which seeds expose a bug. The suite compares runs made
from the same current retained inputs and checks application invariants and
planted controls. The race check replays a lost-update outcome observed in the
current sample, without requiring a particular seed or count. Seed samples and
performance variants live in `acceptance/cases.json`.

For a deliberately performance-only engine change, add
`--performance-only /path/to/other/seedbox-vmm` to compare a fresh retained run
against that engine. Ordinary semantic changes need no pin refresh or bypass.
A fixed-margin case that only finishes after a larger-margin retry is UNRESOLVED.

Every execution goes through `Orchestrator::run`/`replay` and `VmmExecutor`.
The executor exclusively owns CPU/SMT leases, child termination/reaping and
exact MSR restoration. Scheduling and suite assertions do not mutate host state.
Workers use disjoint cores, results preserve job order, and every attempt gets
its own manifest and retained logs. Lease contention is unresolved unless
`--lease-wait SECONDS` is supplied. Typed PMU overshoots retry once at margin
98304; clock-bound errors and arbitrary failures do not retry. Ambient VMM
performance environment variables are cleared.

The fixture adapter requires the structured VMM report and validates the
checker's single `RESULT pass|fail` against its actual exit status. A scheduled
kill alone is not proof of a fault: recovery requires ordered guest stop/start
acknowledgements. Legacy guests do not timestamp these acknowledgements, so they
are recorded separately from the orchestration API's timed fault effects.
Reports include world/per-machine witnesses, full-state digests when requested,
exit counters, host/simulated time, fault evidence, retry paths and replay status.
Timing and exit counts are diagnostic, not witness equality assertions.

`report.json` is written in the requested fresh output directory. Acceptance
returns 0 for PASS, 1 for a failed property, and 3 for unresolved infrastructure;
usage errors return 2. Each run's manifest remains inspectable if a suite is
interrupted before its summary is written.

The reusable interfaces are `acceptance::Launcher` for normalized observations,
`runner::JobRunner` for bounded scheduling, and the existing preparation/execution
traits underneath `Orchestrator`. Portable tests substitute those interfaces;
`seedbox/tests/runtime.rs` exercises the real Compose API, planted Redis/etcd
failures, replay, limits and cleanup. Its small shell wrapper only builds inputs
and launches the ignored Rust hardware test. One CLI signal test covers the
external cancellation boundary.

## Batch runs and campaigns

```sh
sudo -n sim/seedbox/target/release/seedbox batch sim/worlds/race.json \
  --seeds 1,2,2,3 --cpus 2-4 --vmm sim/vmm/target/release/seedbox-vmm \
  --artifacts artifacts --output runs/race-new
sudo -n sim/seedbox/target/release/seedbox campaign sim/worlds/kill-3.5.2.json \
  --first-seed 4000 --max-seeds 20 --seconds 3600 --cpus 2-7 \
  --vmm sim/vmm/target/release/seedbox-vmm --artifacts artifacts \
  --output runs/hunt-new
```

Campaigns stop dispatching after the exact seed cap, time budget, unresolved
attempt or an observed application failure; `--keep-going` continues through
application failures. Already-running jobs finish within their wall limits.
Application failures are useful completed campaign results, so batch/campaign
exit 0 when all attempted runs completed; unresolved attempts return 3. Inspect
`report.json` and the campaign's streamed `attempts.jsonl` for findings.

`batch.sh WORLD NAME "SEEDS" [--digest]` and
`campaign.sh WORLD NAME FIRST_SEED HOURS [MAX_SEEDS [continue]]` are thin
compatibility shims. Build the Rust CLI first. They retain familiar environment
options (`CPU0`, `SEEDBOX_CPUS`, `VMM`, `SEEDBOX_ROOT`, `SEEDBOX_TIMEOUT`,
`SEEDBOX_MARGIN`, `SEEDBOX_UNDO`) but use the new reports under `runs/NAME/`.
The old raw JSON/stdout/status triplets and `campaign-result.py` parser are
retired. Historical scripts under `research/*/raw/` describe their recorded
revision and are not current runner entrypoints.

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
nix develop ./sim
export SEEDBOX_WORK="$PWD" SEEDBOX_ROOT="$PWD"
export CARGO_BUILD_JOBS=1 GOFLAGS=-p=1 GOMAXPROCS=1
mkdir -p src artifacts runs
BUILD_JOBS=1 taskset -c 14 sim/kernel/build.sh
(taskset -c 14 cargo build --release --manifest-path sim/vmm/Cargo.toml)
taskset -c 14 sim/guest/build-netns.sh artifacts/single.cpio src/etcd/v3.5.2/etcd single
taskset -c 14 sim/guest/build-netns.sh artifacts/multi.cpio src/etcd/v3.5.2/etcd multi
CPU0=14 SEEDBOX_TIMEOUT=120 sim/batch.sh sim/worlds/single-vm.json netns-one "1009" --digest
CPU0=14 SEEDBOX_TIMEOUT=120 sim/batch.sh sim/worlds/cluster-fixed.json netns-four "1009" --digest
```

Use a fresh output prefix for every invocation. The example CPU is the lane
used for this investigation, not a requirement of the shared tooling. Check
host topology and other jobs before choosing cores; keep SMT siblings unused
by other workloads during measurements. The Rust executor keeps
an invocation-specific snapshot, and restores the assigned cores after its
children have finished. Do not reuse the historical fixed-CPU research runner
for new campaigns.

`single-vm-controls.json` exercises packet isolation and directionality;
`single-vm-sanity.json` plants a missing acknowledgement to check the checker;
`single-vm-fault.json` applies a directed partition/heal and a process
stop/restart. These use ordinary world files and the same shared runner.
The whole guest is named `client` so its checker output is consumed by
the Rust campaign/fixture result validation. A planted checker failure is a control,
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
[`research/single-vm-network/REPORT.md`](../research/single-vm-network/REPORT.md)
and must not be attributed to these revised sources.
