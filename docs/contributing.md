# Contributing

## Contributor checks

The shared `seedbox-control` source is compiled by
`crates/seedbox-vmm/control/Cargo.toml`, a small package inside the VMM workspace.
Its distinct package name lets the combined CLI use the same protocol source
without Cargo path-ID collisions; the schema is not copied.
The canonical `seedbox-engine-core` policies use the same package isolation via
both backends' `engine-core` packages. Their directory links and source watchers
cover every policy module, including added files. Canonical and wrapper build
scripts also track the automatic build-only checkout input from `.cargo/config.toml`,
so a shared cache cannot reuse another checkout's code merely because its timestamps
are older. Run Cargo from the checkout root (including standalone aliases) for caching.
Portable tests import the core crate directly; UART register admission remains in
the KVM adapter.

Run tests from the project Nix shell by naming the platform suites you want:

```sh
nix develop --command bash harness/check.sh   # portable (the default)
bash harness/check.sh linux-kvm               # any Linux KVM host, including GitHub-hosted Ubuntu runners
bash harness/check.sh linux --cpu 14          # a Linux KVM host
bash harness/check.sh mac                     # Apple Silicon
bash harness/check.sh linux-apps --list       # check requirements, build nothing
bash harness/check.sh --help                  # list suites and groups
```

The runner never selects tests by probing the host. Every named suite runs, and
a suite whose host requirements are missing fails with exit 2 before anything
builds. The `linux` group covers generic KVM, OCI and applications under a
CPU/SMT lease; `mac` covers signed native HVF tests and
applications. The guest-fixture suites need a hand-built kernel and initrd, so
they are named explicitly (`linux-guests`, `mac-guests`). Application acceptance
uses the release CLI, after building guest assets with `bash tools/build-assets.sh`.

Portable checks must pass before hardware execution. Hardware-feature suites
are compiled with `--no-run` during portable checks, even without virtualization.
VMM checkpoint and trigger tests also compile by default, with conditional
`ignore` attributes controlling execution.

See [running and adding tests](docs/testing.md) for the matrix and fixture inputs.
Rust and Python tests are discovered automatically; adding tests to an existing
suite requires no runner edits. Check a different revision in a Git worktree
with the same entrypoint. The runner does not stamp revisions or checkout state.


## Low-level engine fixtures

Application acceptance and seed sweeps use the YAML scenarios below with
`seedbox run` / `seedbox replay`. The old application campaign runner, its console
result parser, duplicate etcd/race world JSON and the race-specific boot script
have been removed. Their last versions are retained in Git at `c7d7c22`.

`harness/batch.sh` remains for the raw kernel/network fixtures and the one-VM
network comparison below, whose placement differs from one service per VM.
Those fixtures still need the shared guest builders and CPU/SMT lease handling.
See [the fixture map](../harness/worlds/README.md).

`--watchdog-ms` affects speed only, never results: it sets how often the host
watchdog observes a guest that makes no natural exits.

Charged timer-port accesses apply their
device effect or sample their read response once and conserve their full 1000 ns
cost across world and timer boundaries. The guest resumes only after that cost
is paid and KVM completes the pending instruction. Terminal witnesses retain a
pending operation, including its remaining cost and transferred bytes. Other
clock advances, including guest time reads, retain the strict hard-stop check;
crossing it fails with `clock bound exceeded`.

## Acceptance scenarios

Acceptance tests are ordinary YAML projects with guest Python drivers. From the
repository root on the qualified Linux host, after setting up the CLI runtime
inputs described above:

```sh
seedbox run harness/acceptance/workloads/short-kill-3.5.2.yaml
seedbox run harness/acceptance/workloads/sanity.yaml
seedbox run harness/acceptance/workloads/race.yaml
seedbox run harness/acceptance/workloads/kill-3.5.3.yaml
seedbox run harness/acceptance/workloads/volume-crash.yaml
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
particular seed to lose a fixed number of updates. The volume scenario puts
keys into a single etcd node whose data directory is a named volume, crashes and
reboots that machine through the control API, and requires every acknowledged
key to be read back.

Each command uses normal CLI outcomes and exit codes. There is no host-side
console parsing, scenario verdict, seed pin, automatic retry, or acceptance-only
execution path. Change the seed with `--seed`, request state digests with
`--full-state`, or compare performance settings using the retained run:

```sh
seedbox replay /path/to/retained/run
seedbox replay /path/to/retained/run --watchdog-ms 5

seedbox explore harness/acceptance/workloads/kill-3.5.3.yaml --seed 4000 --count 11
```

The CLI owns image preparation, CPU leases, restoration, retained evidence and
replay comparison. YAML owns the simulated and wall-time limits. Run the commands
sequentially unless the host has sufficient resources for multiple worlds; each
etcd scenario runs four guests.

`bash harness/acceptance.sh portable` runs source/unit tests and validates the
YAML without starting guests. It has no hardware tiers. The low-level batch
runner remains available for the distinct fixtures below. Hardware results and known runtime failures
are recorded in [docs/acceptance.md](acceptance.md).

The genuine historical etcd 3.5.2 failure remains separate from the planted
checker control. Its exact engine/profile and bounded invocation without retry
remain unresolved in [issue #36](https://github.com/gardnervickers/seedbox/issues/36).

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
matched optimized comparison, qualified guest-local jitter, the full
seeded kill/restart schedule, eight writers for 60 seconds, and etcd 3.5.3
controls over the same seeds. The historical ABI1 timings are in
[the historical networking report](https://github.com/gardnervickers/seedbox/blob/788fc4376b7e2cc96d1bc51ed2ae3d4635d16abd/research/single-vm-network/REPORT.md)
and must not be attributed to these revised sources.

See [the experimental arm64 backend quickstart](arm64-backends.md#experimental-arm64-quickstart) for the isolated HVF/KVM fixture.
