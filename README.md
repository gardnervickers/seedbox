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
| `research/` | Design conclusions and evidence records ([status](research/STATUS.md)); research crates removed, see commit `bc07f35` |

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

Acceptance scenarios are ordinary YAML applications in
`harness/acceptance/workloads/`, with Python drivers in
`harness/acceptance/driver/`. The etcd YAML selects its Python driver as both
`x-seedbox.control.service` and `x-seedbox.check.service`. Inside the guest, the
driver waits for etcd readiness, starts the existing Go workload/checker, injects
confirmed process restarts and peer partitions through `seedbox.World`, heals the
partitions, and checks the workload's acknowledgement-ledger result. The race
driver runs the existing C example and verifies its atomic control.

The host Python suite stages self-contained projects with the Python binding and
existing workload sources. It invokes ordinary `seedbox run`, `replay`, and
`inspect --json` commands. The first execution of each seed/witness mode prepares
images; repeats and performance variants replay its retained inputs. The CLI owns
execution, CPU/SMT leases and restoration. No test-only Rust bridge or raw-world
fault overlays are involved. Scenario policy stays outside the production binary.

From the repository root on the qualified Linux host:

```sh
nix develop
cargo build-vmm
bash harness/acceptance.sh portable
bash harness/acceptance.sh smoke --root "$PWD" --kernel /path/to/bzImage --cpus 2-5
bash harness/acceptance.sh full --root "$PWD" --kernel /path/to/bzImage --cpus 2-5
```

The suite builds this checkout's CLI by default; `--seedbox PATH` chooses another
build. `--vmm PATH` chooses the engine (default: `cargo build-vmm` output).
Use the fault-capable kernel described in `kernel/config.options`, including
virtio-console. OCI preparation requires the Nix shell's tools, registry access
and passwordless sudo, as does CLI execution. YAML controls the simulated and
wall-time limits; `--lease-wait SECONDS` waits briefly for busy cores. Both tiers
default to four workers on CPUs 2–5, excluding overlapping SMT siblings. Size
`--cpus` for available host memory as well as CPU capacity; an etcd scenario
runs four guests per worker.

| Tier | Checks |
| --- | --- |
| `portable` | Shell/Python tests, public YAML validation, Rust tests and Linux VMM tests |
| `smoke` | Fresh exit/full-state replay; undo 0/16 and margin invariance; varied race outcomes and atomic control; planted checker failure; confirmed process/network fault recovery |
| `full` | Smoke plus etcd 3.5.3 seeds 4000–4010; optional cross-engine equivalence |

`acceptance/cases.json` selects scenarios and seed samples. There are no historical
failing-seed or witness pins. `--performance-only OLD_VMM` explicitly enables
cross-engine comparison. Scenario faults and checks rerun inside the guest during
replay; confirmed fault histories must also match. PMU overshoots retry once with
margin 98304; clock-bound errors do not. A fixed-margin variant requiring retry
is UNRESOLVED.

Evidence under `runs/acceptance-<tier>-<time>-<pid>/` includes staged projects,
CLI command/output logs, the CLI content store and private retained runs, plus a
Python `report.json`. The suite verifies output hashes against `inspect` records.
An infrastructure error or failed CLI cleanup cannot count as a completed run.
Exit status is 0 for PASS, 1 for a failed property, 3 for UNRESOLVED and 2 for
usage errors. The legacy batch/campaign runners and research fixtures remain
available separately.

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
[`research/single-vm-network/REPORT.md`](research/single-vm-network/REPORT.md)
and must not be attributed to these revised sources.
