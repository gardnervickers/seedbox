# Seedbox

A deterministic world of real Linux machines on KVM, run by one host thread.
Applications are described in Compose-shaped YAML and packaged as OCI images.
A seed plus the retained inputs (kernel, images, resolved configuration) regenerate
the same execution; seeds vary guest and simulator randomness, including timer and
interrupt timing.

## Quick start

`validate`, `init`, `inspect`, `diff`, `export` and `import` run on Linux or macOS. `run`,
`explore` and `replay` need the
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
cargo run -- diff .seedbox/runs/ATTEMPT .seedbox/runs/REPLAY
```

`seedbox run` with no arguments runs the Compose project in the current
directory. Every runtime input has a default, so no flags are required:

| Input | Default | Override |
| --- | --- | --- |
| Compose file | `compose.yaml` in the given directory (default `.`) | positional path |
| Directed link baselines | no overrides; global latency/jitter and zero loss | `x-seedbox.network.links` |
| Connection selectors | disabled; discover and select through the guest control API | `world.connections`, `world.drop_connection` |
| Seed | `x-seedbox.seed` from the Compose file | `--seed` |
| Exploration count | `1` | `--count` |
| Exploration workers | `1`, sequential execution | `--workers` (1..32; automatic CPU selection for parallel execution) |
| Torn disk writes | off (drop all unflushed sectors on crash) | `--torn-writes` |
| CPU | first free core other than CPU 0's, under the shared lease | `--cpu`, `SEEDBOX_CPU` |
| Guest kernel | `bzImage` in `$SEEDBOX_ARTIFACTS`, `$SEEDBOX_WORK/artifacts`, `./artifacts`, then `~/seedbox-sim/artifacts` for the user and any sudo caller | `--kernel`, `SEEDBOX_KERNEL` |
| VMM | `seedbox-vmm` next to an installed `seedbox`, then this checkout's `cargo build-vmm` output, then on `PATH`; `cargo run` builds it if missing | `--vmm`, `SEEDBOX_VMM` |
| Guest supervisor | built into `seedbox` from [`guest/compose-init.c`](guest/compose-init.c) with musl | `--guest-init`, `SEEDBOX_GUEST_INIT` |
| BusyBox | `SEEDBOX_BUSYBOX` (set by `nix develop`), then a static `busybox` on `PATH` | `--busybox` |
| mke2fs (only for named volumes) | `SEEDBOX_MKE2FS` (set by `nix develop`), then a static `mke2fs` on `PATH` | `--mke2fs` |
| Build epoch | absent; preserve the builder's ordinary timestamp policy | `--build-epoch` (prepare/run/explore; no environment backing) |
| Engine qualification | pinned engine and guest kernel; dev artifacts require explicit opt-in | `--allow-dev-engine` (run/explore/replay/doctor; no environment backing) |
| State | `.seedbox/` | `--state`, `SEEDBOX_STATE` |
| Diff inputs | Two required retained run directories; no runtime inputs or state lookup | positional `RUN_A RUN_B` |
| Bundle transfer cap | 32 GiB each for unique objects and materialized logs | `--max-bytes` |

Directed baselines are optional Compose settings. For example, with `server` and
`client` services:

```yaml
x-seedbox:
  network:
    latency: 1ms
    links:
      - {from: server, to: client, latency: 20ms, jitter: 2ms, loss_ppm: 50000}
```

Each entry affects one direction. Latency is required; jitter and loss default to
zero. Global latency remains the fixed scheduling lower bound for every link.
`seedbox validate` checks the services, duplicate pairs, and policy bounds before
preparation. Guest controllers can temporarily override links with
`world.shape(["server"], ["client"], latency=20_000_000, jitter=2_000_000, loss_ppm=50000)`
and release the returned handle with `world.heal(handle)`; see
[network policy](docs/runtime.md#directed-link-policy).

For one TCP direction, use `world.connections("server", "client")`, select an
exact returned record with `world.drop_connection(record)`, then
`world.heal(handle)` and require its `dropped` count to be positive. Discovery is
paginated and selectors bind an observed incarnation, so tuple reuse does not
inherit the old fault. See [connection selectors](docs/runtime.md#connection-selectors)
for lifecycle and unsupported-traffic behavior, and the
[AMD validation procedure](docs/connection-selectors-amd.md).

For development artifacts, pass `--allow-dev-engine` to `run`, `explore`, `replay`
or `doctor`. Unpinned engine or guest-kernel bytes record the semantic parameter
`engine_qualification = "dev-unpinned"` and are reported as an unqualified dev
engine. Replay requires this flag and the exact retained engine/kernel hashes;
bundles preserve the marker. Passing the flag with both pinned artifacts adds no
marker. All host, CPU, PMU, MSR, lease, ABI and control-channel checks still apply.

`seedbox run --torn-writes` enables seeded partial survival of unflushed
512-byte disk sectors on crash. It is recorded in run identity and replayed
automatically. It applies to retained disks; Compose storage is still RAM-backed
until named-volume disk support lands. See [disk semantics](docs/runtime.md#disk-crash-policy).

`seedbox help run` lists every option. The guest Python API supports
[bounded committed console triggers](docs/triggers.md) for future stop/partition
faults in the development engine; see that contract and AMD validation handoff. A run prints `OUTCOME: run-directory`;
exit status 0 is PASS (or replay MATCH), 1 FAIL (MISMATCH), 2 ERROR, 3
INCOMPLETE and 124 TIMEOUT. Offline `diff` exits 0 for MATCH, 1 for DIFFERENT,
and 2 for usage/setup or invalid retained evidence.

On a VMM failure, text output from `run`, `replay`, `explore` and `inspect`
also prints up to five non-empty lines from the final 8 KiB of that attempt's
`output/vmm.stderr` to stderr, prefixed with `vmm stderr:`. Missing or unreadable
logs do not change the outcome. Retained records and `--json` output are unchanged.
`inspect` keeps the recorded `error:` line on stdout alongside the run status.

Pause a running world from another terminal with `sudo seedbox pause RUN`, then
continue it with `sudo seedbox resume RUN`. Use the executing attempt directory
(for replay, its new attempt), identifiable by `RUN/output/host-control`. Both
commands wait for a boundary acknowledgement. All guests and virtual time freeze;
wall-time limits and cancellation remain active. See [host pause](docs/runtime.md#host-pause)
for the mechanism and development-engine verification requirements.

[Draft format and runtime scope](docs/compatibility.md) distinguishes inspection
and transfer from qualified execution. Same software versions, seed and exact
retained inputs must replay on a qualified compatible profile; changing versions
may change execution. Release compatibility remains a V0 qualification gate.

## Finite exploration

`seedbox explore [COMPOSE] --seed FIRST --count COUNT` runs consecutive seeds
with one worker by default, preparing the complete input closure once. Compose
defaults to `.`, the first seed to the Compose seed, and count to 1.
`--workers N` (1..32) permits at most N owned attempts at once; parallel exploration
requires automatic CPU selection, so omit `--cpu` and `SEEDBOX_CPU`. Before
preparation, the executor must be able to reserve and admit N distinct eligible
physical cores together, including their SMT siblings; insufficient capacity is
ERROR. Those transient reservations are released before OCI work. Each running
world still owns its ordinary core/sibling lease. Contention afterward is an
infrastructure error, with no retry or waiting-for-capacity policy. Empty/overflowing ranges fail
before state creation or preparation. Immutable runtime inputs and host admission are
checked before the sole preparation; refusal retains batch ERROR without dispatch.
Each seed uses the ordinary retained-input and leased host rechecks. Application FAIL
and witnessed simulated TIMEOUT continue; ERROR, INCOMPLETE and cancellation stop
dispatch. No seed is retried.

Each exploration retains `exploration.json`, preparation diagnostics and ordinary
per-seed run directories under `attempts/SEED/`. Inspect and replay those run paths
with the existing commands. An unfinished checkpoint remains INCOMPLETE, including
when a process dies after committing a run but before updating the batch. The final
exit precedence is ERROR (2), INCOMPLETE (3), TIMEOUT (124), FAIL (1), PASS (0). `--json` emits one
report with per-attempt outcomes, undispatched count, elapsed time and CLI process-family
resource measurements; application logs remain arbitrary bytes. Checkpoint version 2
records seed indexes in ascending dispatch order regardless of completion order,
with `finished: false` for dispatched attempts whose terminal result is not yet
checkpointed. The pending suffix remains undispatched. Infrastructure or retention
uncertainty cancels only the batch's owned attempts and joins them before returning;
a backend blocked in cleanup prevents a prompt completion claim. Version 1 batch
checkpoints need their prior reader; their bytes and ordinary run manifests remain
unchanged.

`peak_running` counts active orchestration attempts. `active_attempt_seconds` sums
their whole wall intervals, including admission, materialization and cleanup; it is
not exact lease time or measured CPU time. Once all owned attempts return, this
one-world-per-worker backend can use the sum as a conservative guest-core charge.
An unfinished checkpoint omits still-running/uncheckpointed intervals and is not
a complete cumulative charge. CLI process-family CPU totals include
preparation and reaped children. Parallel scheduling and chosen cores are performance
choices excluded from run identity; actual serial/parallel and per-core equality
remain runtime qualification gates.

Exploration `build:` admission currently accepts only the inspected immutable
project-Nix Linux Podman launcher/native closure in explicit local mode. Unknown
wrappers and daemon/remote delegation are rejected. Cancelled builds produce
ERROR / cleanup-unverified and retain their unique private tag; process-group
cleanup alone does not prove builder cessation. Ordinary run/prepare continue
supporting custom builders; the optional native timestamp policy below has a
narrower admission requirement. See [preparation evidence and cancellation](docs/runtime.md#exploration-preparation).

For optional build timestamp control, `--build-epoch 0` selects the admitted
project-Nix native Podman's creation-time and new-layer timestamp policy.
It changes guest-visible new-layer file and directory timestamps, so it is
optional and never applied to registry or base layers. The range is
0..=4294967295 Unix seconds; custom builders are unsupported with this option.
Moving base tags, network downloads and arbitrary build programs can still
change inputs. Replay uses the complete retained application release closure
without this option; deterministic execution does not require independently
rebuilt OCI images to have identical bytes.


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

## Contributor checks

Portable checks must pass at the PR or merge-train head before spending AMD
host time. Use the fast pre-check on a Mac or any other non-KVM machine:

```sh
bash harness/mac-check.sh main     # any local revision or branch
bash harness/mac-check.sh          # current checkout, including local changes
```

Run from within the checkout on any host inside the project's Nix dev shell.
With a revision, the script creates a detached worktree under `$TMPDIR` (or `/tmp` if unset) and
deletes it on a pass; on failure it keeps the worktree and prints its path.
Without a revision, it checks the existing checkout, including local changes.
Its first stdout line is `revision: FULL_SHA`. It uses the caller's development
shell to run, in order:

- `cargo fmt -p seedbox --check`
- `cargo clippy -p seedbox --all-targets --locked -- -D warnings`
- `bash harness/acceptance.sh portable`

The portable suite includes workspace tests, both Python unittest suites and
YAML validation. Workspace builds share a persistent target directory at
`$XDG_CACHE_HOME/seedbox/precheck-target` when set, otherwise
`~/Library/Caches/seedbox/precheck-target` on macOS or
`~/.cache/seedbox/precheck-target` elsewhere. `SEEDBOX_PRECHECK_TARGET` overrides
that path; an existing `CARGO_TARGET_DIR` is respected when the override is unset.
Relative overrides resolve from the invoking directory, before entering a
revision worktree. Cargo fingerprints determine which build outputs can be reused.

All steps run even if an earlier step fails. Command output goes to stderr;
stdout ends with timed step/phase lines in `harness/host-check.sh`'s exact format:
`printf '%-8s %s %4ss\n' STEP pass|FAIL SECONDS`, with `phase ` lines preserved.
The gate exits 0 only if every step passes, and 1 if any step fails; setup errors
also exit nonzero. The runner comes from the invoking checkout, so it can check
revisions that predate the script.

On macOS, the pre-check skips the Linux-only VMM tests and batch supervisor
lifecycle tests. On Linux, the portable suite includes those tests; VMM tests
keep their standalone target directory instead of using the workspace cache.
The pre-check does not execute guests. VMM builds and KVM acceptance remain
Linux host checks. After the pre-check passes a committed revision, run:

```sh
bash harness/host-check.sh --skip-portable REV CPU
```

This skips fmt, clippy and the portable suite (including workspace tests), while
keeping the CLI build, VMM build (unless `SEEDBOX_VMM` selects a stored engine),
VMM tests and AMD smoke checks. Both gates print the full SHA; match them before
using the skip option. A pre-check with local changes only verifies those local
bytes and does not establish a pass for the committed SHA.

## How execution is controlled

- **Progress and time.** Guest progress is the host perf count of retired
  conditional branches (guest only, kernel and user). Virtual time advances
  1 ns per branch; idle machines jump to their next event.
- **Stops.** Events (timer deadlines, frames, scenario commands) are delivered
  at deterministic points only: a natural VM exit, or an exact branch count
  reached by overflow interrupt plus hardware-breakpoint stepping (no TF).
  Syscalls are not stopping points. A timer interrupt fires at the first
  natural exit at or after its deadline, at most `stop_tolerance_ns` late, and
  then by an exact count. An overflow interrupt that skids past a counted
  stop is rolled back and retried closer, or fails the run, rather than moving
  the stop.
- **Guest kernel changes** (`kernel/`): kernel TSC reads return VMM-published
  virtual time; user RDTSC/RDTSCP trap via CR4.TSD and are emulated. The
  kernel's syscall yield hook remains, but the engine never requests it.
- **Network and scheduling.** virtio-net over virtio-mmio into an in-memory
  fabric with per-direction latency, jitter, loss and partitions. The world runs the
  machine furthest behind in slices that never pass its causal horizon (the
  other machines' times plus link latency) or its next event. A slice may end
  at the first deterministic point in the last half-latency before that
  boundary (capped by `stop_tolerance_ns`), and events are delivered at the
  first stop up to that long before they are due: early by a bounded amount,
  never late, and never before they were sent.
- **Witness.** A network hash covers pending frames, link streams and active policies.
  Each machine hashes every deterministic exit; `--digest` adds
  a hash of registers and all guest RAM; `--trace` writes every exit.

## Host requirements

- x86-64 AMD Zen host with KVM, run as root (guest-only perf counting).
  Seedbox picks a branch counter profile from CPUID at startup
  (`crates/seedbox/src/counter.rs`, `crates/seedbox-vmm/src/profile.rs`) and
  records it in each run's identity, so a run replays only with the counter
  that made it. Only Zen is qualified. Non-hybrid Intel cores have profiles
  (`intel-0x5101c4` up to Comet Lake, `intel-0x5111c4` from Ice Lake) that
  have never run on hardware; `run` and `replay` refuse them unless given
  `--allow-unqualified-cpu`, and then warn that results are unverified.
- **The Zen SpeculativeLockMap workaround must be on** for the cores running
  simulations (MSR 0xC0011020 bit 54, rr's workaround). Without it, branch
  counts drift on lock-heavy code (e.g. Go) and runs diverge. The Rust executor
  acquires the shared CPU/SMT leases, sets
  it with readback, then restores the exact originals after reaping its child.
  The legacy `batch.sh` runner uses the same lease protocol.

`--allow-unqualified-cpu` also applies to `explore`. This explicit non-Zen
path is experimental: it retains the counter in identity, validates exact
supported release inputs and applicable KVM capabilities, and holds every SMT
lease through child cleanup. It carries no qualified AMD host-policy proof and
never opens AMD workaround MSRs. `doctor` refuses an unqualified counter rather
than reporting qualified compatibility. The source policy now pins the frozen engine52eb and accepted guest kernel.
The artifact is built but unqualified: this staged release still requires fresh
runtime acceptance before use. Disabled selected-event readiness is checked
before preparation and again under held leases; no guest counter is enabled by
admission. Intel determinism is not established.

The versioned `amd-zen3-engine52eb-v1` admission policy checks AMD family 25,
model 33, stepping 0 and microcode 0xa20102e, exact booted/current host kernel and
controlled guest-kernel bytes, accepted engine bytes, KVM capabilities and the
engine's complete existing CPUID policy. Before image preparation, it temporarily
reserves an eligible core, checks each online/allowed SMT sibling and opens the
exact perf event disabled. Descriptors close and affinity/reservations restore
before preparation. The leased execution core is checked again before MSR writes.
This is readiness admission; it does not prove guest counting, overflow behavior
or cross-host determinism. Matching a second machine admits compatibility without
host-ID enrollment. Support evidence remains the tested host and inputs.

Qualified Zen run identities bind the full policy and its fingerprint. Existing records
remain inspectable and transferable, but replay through this CLI rejects missing
or drifted host-policy proof as ERROR/NOT_COMPARABLE (exit 2), even with
`--allow-engine-change`. A retained previous CLI can still be used for explicitly
historical evidence; its execution does not establish new-policy admission.
A new engine/kernel/policy requires its own acceptance for qualification.
`--allow-dev-engine` admits unpinned engine/kernel bytes for development with a
retained unqualified marker; it never bypasses missing or drifted host proof. Admission source checks
are separate from the still-required focused Linux guest/profile acceptance.

## Check execution compatibility

`seedbox doctor` checks the installed engine, guest kernel and selected host core
against the same versioned admission policy used by execution. It needs a Linux
KVM host and root, uses the normal `--cpu`, `--kernel` and `--vmm` defaults, and
never builds a missing VMM, prepares images or starts guests. `--json` prints the
exact admitted observations, plus `engine_qualification: "dev-unpinned"` for dev
artifacts. With `--allow-dev-engine`, unpinned artifacts print `dev engine:
unqualified` and receive no qualification claim. Success prints `VALID` after probe resources and
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

## Compare retained runs

`seedbox diff RUN_A RUN_B [--json]` compares two retained directories on Linux or
macOS without executing guests. It reports the world witness, each machine's
exit/full-state witnesses, the first differing retained trace record (or terminal
record), and changed identity fields including seed, engine, kernel, images and
semantic parameters. Import bundles first, then pass their printed run paths.

Only manifest-declared, checksum-verified evidence is read. The adapter does not
record traces by default: terminal times cannot identify an earlier exit where
execution diverged. Missing witnesses, traces and capture omissions are explicit.
MATCH means equality of the compared evidence, even if incomplete; it does not
qualify replay or prove equality of unrecorded state. Performance counters,
margin/undo, attempt metadata and arbitrary console output are excluded.
See [comparison details](docs/runtime.md#compare-retained-runs).

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
PMU approach and stepping distance when undo is off (default `adaptive`). Neither
setting decides where a stop lands: the first natural exit at or after the soft
target and below the hard target, otherwise an exact count at the hard target. The
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
seedbox replay /path/to/retained/run --undo 0
seedbox replay /path/to/retained/run --undo 16
seedbox replay /path/to/retained/run --margin 32768

seedbox explore harness/acceptance/workloads/kill-3.5.3.yaml --seed 4000 --count 11
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
