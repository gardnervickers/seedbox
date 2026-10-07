# Seedbox

A deterministic world of real Linux machines on KVM, run by one host thread.
Applications are described in Compose-shaped YAML and packaged as OCI images.
A seed plus the retained inputs (kernel, images, resolved configuration) regenerate
the same execution; seeds vary guest and simulator randomness, including timer and
interrupt timing.

## Quick start

`validate`, `init`, `inspect`, `export` and `import` run on Linux or macOS. `run` and `replay` need the
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
| Bundle transfer cap | 32 GiB each for unique objects and materialized logs | `--max-bytes` |

`seedbox help run` lists every option. A run prints `OUTCOME: run-directory`;
exit status 0 is PASS (or replay MATCH), 1 FAIL (MISMATCH), 2 ERROR, 3
INCOMPLETE and 124 TIMEOUT.

[Draft format and runtime scope](docs/compatibility.md) distinguishes inspection
and transfer from qualified execution. Same software versions, seed and exact
retained inputs must replay on a qualified compatible profile; changing versions
may change execution. Release compatibility remains a V0 qualification gate.

## Layout

| Path | Contents |
| --- | --- |
| [`crates/seedbox/`](crates/seedbox/README.md) | Configuration, OCI preparation, orchestration and the `seedbox` CLI |
| `crates/seedbox-vmm/` | KVM VMM and world scheduler (`seedbox-vmm`); its own workspace and lockfile, because its binary hash is the run's engine identity |
| `examples/` | Runnable Compose applications: etcd cluster and Redis, each with a workload image |
| `kernel/` | Guest kernel patch, config and build script (Linux 6.12.112) |
| `guest/` | Generic OCI guest supervisor plus low-level guest fixtures and builders |
| `docs/` | [Vision](docs/vision.md), [runtime instructions](docs/runtime.md), [backends](docs/backends.md), [acceptance record](docs/acceptance.md) |
| `harness/` | YAML acceptance scenarios, portable checks, low-level world fixtures and their batch runner ([migration map](harness/worlds/README.md)), `msr54.py` |
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

The versioned `amd-zen3-engine168ad-v1` admission policy checks AMD family 25,
model 33, stepping 0 and microcode 0xa20102e, exact booted/current host kernel and
controlled guest-kernel bytes, accepted engine bytes, KVM capabilities and the
engine's complete existing CPUID policy. Before image preparation, it temporarily
reserves an eligible core, checks each online/allowed SMT sibling and opens the
exact perf event disabled. Descriptors close and affinity/reservations restore
before preparation. The leased execution core is checked again before MSR writes.
This is readiness admission; it does not prove guest counting, overflow behavior
or cross-host determinism. Matching a second machine admits compatibility without
host-ID enrollment. Support evidence remains the tested host and inputs.

New run identities bind the full policy and its fingerprint. Existing records
remain inspectable and transferable, but replay through this CLI rejects missing
or drifted host-policy proof as ERROR/NOT_COMPARABLE (exit 2), even with
`--allow-engine-change`. A retained previous CLI can still be used for explicitly
historical evidence; its execution does not establish new-policy admission.
A new engine/kernel/policy requires its own acceptance. Admission source checks
are separate from the still-required focused Linux guest/profile acceptance.

## Check execution compatibility

`seedbox doctor` checks the installed engine, guest kernel and selected host core
against the same versioned admission policy used by execution. It needs a Linux
KVM host and root, uses the normal `--cpu`, `--kernel` and `--vmm` defaults, and
never builds a missing VMM, prepares images or starts guests. `--json` prints the
exact admitted observations. Success prints `VALID` after probe resources and
thread affinity are verified cleaned up; problems use the ordinary error and
exit status 2. This is compatibility readiness, not cross-host replay qualification.

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

## Sharing a retained run

Export a run with its exact retained inputs and logs, then import it into fresh
state on Linux or macOS:

```sh
seedbox export .seedbox/runs/ATTEMPT -o failure.seedbox.tar
seedbox import failure.seedbox.tar --state imported
seedbox inspect IMPORTED_RUN
seedbox replay IMPORTED_RUN --state imported
```

Use the run directory printed by `import` as `IMPORTED_RUN`. Import executes
nothing. Replay uses the bundled engine and inputs without image preparation,
registry access or the original checkout, and requires a compatible installed
Seedbox release and qualified AMD Linux host. Transfer does not qualify a new
host. A reproduced application FAIL is replay MATCH.

Bundles include only retained logs. Any capture omissions remain visible and
are printed as `omitted files: N`; omitted local output is not transferred.
Existing files/runs are never replaced. `--max-bytes` changes the local transfer
cap without changing run identity. See [runtime instructions](docs/runtime.md#bundles)
for bounds and supported formats.

## Low-level engine fixtures

Application acceptance and seed sweeps use the YAML scenarios below with
`seedbox run` / `seedbox replay`. The old application campaign runner, its console
result parser, duplicate etcd/race world JSON and the race-specific boot script
have been removed. Their last versions are retained in Git at `c7d7c22`.

`harness/batch.sh` remains for the raw kernel/network fixtures and the one-VM
network comparison below, whose placement differs from one service per VM.
Those fixtures still need the shared guest builders, CPU/SMT lease handling and
MSR restoration. See [the fixture map](harness/worlds/README.md).

Stop placement settings affect speed only, never results. `--undo` controls the
precise-stop rollback window (default 256; 0 disables it). `--margin` selects the
PMU approach and stepping distance when undo is off (default `adaptive`). Syscall
exits remain available throughout a soft stop interval independently of that
setting; earlier yields do not publish a stop or inject an interrupt. The
low-level batch runner uses `SEEDBOX_UNDO` and `SEEDBOX_MARGIN` for the same settings.

A PMU overshoot aborts an execution. A separate invocation can use a larger
margin; the failed evidence remains. Charged timer-port accesses apply their
device effect or sample their read response once and conserve their full 1000 ns
cost across world and timer boundaries. The guest resumes only after that cost
is paid and KVM completes the pending instruction. Terminal witnesses retain a
pending operation, including its remaining cost and transferred bytes. Other
clock advances, including guest time reads, retain the strict hard-stop check;
crossing it fails with `clock bound exceeded`. A larger PMU margin does not fix
that error.

## Acceptance scenarios

Acceptance tests are ordinary YAML projects with guest Python drivers. From the
repository root on the qualified Linux host, after setting up the CLI runtime
inputs described above:

```sh
seedbox run harness/acceptance/workloads/short-kill-3.5.2.yaml
seedbox run harness/acceptance/workloads/sanity.yaml
seedbox run harness/acceptance/workloads/race.yaml
seedbox run harness/acceptance/workloads/kill-3.5.3.yaml
```

The YAML references the existing Go/C workload build contexts and loads the
Python drivers and control API through Compose `configs`. No preparation script,
generated project, or host scenario runner is required. Paths resolve relative
to each YAML file, so these commands also work from another directory with an
absolute YAML path.

Inside the simulation, the etcd driver waits for readiness, runs the existing Go
workload/checker, injects confirmed process restarts and peer partitions, checks
that packets were actually dropped, heals the partitions, and verifies the
acknowledgement ledger. The short scenario performs two fault rounds; the long
etcd 3.5.3 scenario performs twenty. The sanity scenario requires detection of its
planted missing acknowledgement and uses `check.exit_code: 1`, so the acceptance
command succeeds when that negative control behaves correctly. The race driver
checks its output and atomic control inside the guest; it does not require a
particular seed to lose a fixed number of updates.

Each command uses normal CLI outcomes and exit codes. There is no host-side
console parsing, scenario verdict, seed pin, automatic retry, or acceptance-only
execution path. Change the seed with `--seed`, request state digests with
`--full-state`, or compare performance settings using the retained run:

```sh
seedbox replay /path/to/retained/run
seedbox replay /path/to/retained/run --undo 0
seedbox replay /path/to/retained/run --undo 16
seedbox replay /path/to/retained/run --margin 32768

(
  for seed in {4000..4010}; do
    seedbox run harness/acceptance/workloads/kill-3.5.3.yaml --seed "$seed" || exit "$?"
  done
)
```

The CLI owns image preparation, CPU leases, restoration, retained evidence and
replay comparison. YAML owns the simulated and wall-time limits. Run the commands
sequentially unless the host has sufficient resources for multiple worlds; each
etcd scenario runs four guests.

`bash harness/acceptance.sh portable` runs source/unit tests and validates the
YAML without starting guests. It has no hardware tiers. The low-level batch
runner remains available for the distinct fixtures below. Hardware results and known runtime failures
are recorded in [docs/acceptance.md](docs/acceptance.md).

The focused race comparison records a fixed `SOURCE_DATE_EPOCH=0` for Podman image
build metadata and requires two public preparations to have identical complete
inputs before execution. Every run must retain that same input closure. It runs four seeds, requires distinct lost-update counts
and a positive loss, checks the atomic control, and replays a selected loss with
complete world/machine witnesses and full state. From the project Nix shell on
the qualified host, use `bash crates/seedbox/tests/accept-race.sh KERNEL CPU OUTPUT`
with a reserved core and a fresh output directory. It freezes source/base inputs
and rejects re-preparation drift; failed attempts remain in that directory.
No retries or broader seed search are performed.

The genuine historical etcd 3.5.2 failure remains separate from the planted
checker control. Its exact engine/profile and bounded invocation without retry
qualification remain unresolved in [issue #36](https://github.com/gardnervickers/seedbox/issues/36).
This race check does not qualify that historical bug or the engine.

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
for new fixture runs.

`single-vm-controls.json` exercises packet isolation and directionality;
`single-vm-sanity.json` plants a missing acknowledgement to check the checker;
`single-vm-fault.json` applies a directed partition/heal and a process
stop/restart. These use ordinary world files and the same shared runner.
The whole guest is named `client`; the batch runner retains its console and
terminal VMM JSON for inspecting these low-level experiments. A planted checker
failure is a control, not a discovered etcd bug.

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
They do not reproduce the historical full `kill-3.5.2` hunt or establish its
bug-finding rate. Before backend selection,
[issue #10](https://github.com/gardnervickers/seedbox/issues/10) requires a
matched optimized PV/undo comparison, qualified guest-local jitter, the full
seeded kill/restart schedule, eight writers for 60 seconds, and etcd 3.5.3
controls over the same seeds. The historical ABI1 timings are in
[the historical networking report](https://github.com/gardnervickers/seedbox/blob/788fc4376b7e2cc96d1bc51ed2ae3d4635d16abd/research/single-vm-network/REPORT.md)
and must not be attributed to these revised sources.
