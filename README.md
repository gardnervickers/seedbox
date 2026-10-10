# Seedbox

**Run real Linux applications. Reproduce the same execution.**

Seedbox runs each service in its own Linux virtual machine and controls time,
networking and disk faults. Describe your application with Compose, choose a
seed, and replay a failure using the exact inputs retained by the run.

[Quick start](#quick-start) · [Your application](#your-application) · [Runtime guide](docs/runtime.md) · [Contributing](docs/contributing.md)

## Quick start

You need **Nix** for the build tools and a running **Docker-compatible engine**
(such as Docker or Podman) for application images. Apple Silicon Macs also need
Apple's Command Line Tools. Linux x86-64 needs read/write access to `/dev/kvm`.

Build Seedbox and its current guest inputs together:

```sh
nix develop
python3 tools/build.py
```

Then run the etcd example directly, without Nix:

```sh
./build/current/seedbox examples/etcd
```

The command publishes a complete build only after the guest inputs and native
CLI succeed. The CLI finds its kernel beside the executable. Separate worktrees
have independent builds; Nix reuses unchanged guest dependencies.

Seedbox finds your Docker or Podman socket, builds the example's workload oracle
with the engine's normal cache, prepares the images, and runs the Linux guests.
Progress appears on stderr. Set `DOCKER_HOST` if your engine uses a custom socket.
Application progress comes from automatic binary instrumentation on both
platforms; see the [determinism catalogue](docs/determinism-catalogue.md).

The CLI connects to your local simulation server and starts it automatically
when needed. The server owns execution; closing a client does not stop a
simulation. `--detach` returns its short ID immediately. Foreground Ctrl-C sends
an explicit stop request. Use `seedbox rerun ID` to repeat a simulation,
`seedbox compare FIRST_ID SECOND_ID` to compare results, or `seedbox replay ID`
to rerun and compare in one command. Replay reports `MATCH`, `MISMATCH` or
`NOT_COMPARABLE`; that comparison does not change the workload oracle verdict. Host watchdog timing uses the engine default.
Use `seedbox get simulation ID --watch` to follow status from another client.
Ctrl-C detaches the watcher; it does not stop the simulation.

Oracle exit zero passes; any nonzero exit fails. `x-seedbox.check.exit_code`
is not configurable and must be omitted from Compose files.

Simulation preparation has a 15-minute server deadline, separate from execution
limits. On expiry the server cancels preparation and waits for cleanup. Without
an oracle exit, a preparation or execution timeout reports `TIMEOUT` (exit 124)
and the simulation's test verdict remains incomplete.


**The kernel is the only separate guest asset.** The complete build embeds
its supervisor, filesystem tool and progress runtime in the executable. Copy
`seedbox` and `kernel` together to run elsewhere. Running or replaying Seedbox
never invokes Nix, Cargo, or an asset builder.

Guest time starts at the latest build time recorded in the prepared images, so
independent fresh runs of the same images start at the same time and can be
compared directly. Set `x-seedbox.wall_clock_epoch` (Unix seconds, at least
86400) in the Compose file to choose the start time. Replay always retains the
original guest clock.

### Your application

Compose can reference an existing image or a Dockerfile build context:

```yaml
services:
  database:
    image: quay.io/coreos/etcd:v3.5.2
  oracle:
    build: ./workload
```

The full [etcd example](examples/etcd/compose.yaml) adds commands, the seed, limits,
and the workload check. Edit the oracle and run the same command again: the
engine reuses unchanged build layers. Seedbox has no separate source-hash cache.
Images must match the guest platform: `linux/arm64` on Apple Silicon or
`linux/amd64` on Linux x86-64. Existing local images are used before registry pulls.
Build contexts honor `.dockerignore`; `build: {context: ..., dockerfile: ...}`
selects a different Dockerfile inside the context. Docker uses BuildKit; Podman uses its native builder. Dockerfile feature support
follows the selected engine. Pull private images with your engine first; registry
credential helpers are not yet integrated into Seedbox.

### Replay and inspect

A run prints its result and short simulation ID. Inspect or rerun it from
another client:

```sh
./build/current/seedbox get simulations
./build/current/seedbox describe simulation SIM_ID
./build/current/seedbox rerun SIM_ID
./build/current/seedbox stop SIM_ID
./build/current/seedbox events SIM_ID
./build/current/seedbox playback SIM_ID
./build/current/seedbox pause SIM_ID
./build/current/seedbox step SIM_ID
./build/current/seedbox rate SIM_ID 1/10
./build/current/seedbox resume SIM_ID
./build/current/seedbox logs SIM_ID              # All machines.
./build/current/seedbox logs SIM_ID MACHINE_NAME # One machine.
```

Describe emits YAML with the resolved specification, status and timings. Save it
and submit its positional path with `seedbox run simulation.yaml`. After preparation, its input
reference uses retained bytes, so rerun needs neither the container engine nor
the original source, image registry or kernel. References remain local to this
server's storage. Each rerun receives a new ID and preserves the original.

Server state defaults to `$XDG_STATE_HOME/seedbox` or
`$HOME/.local/state/seedbox`; `SEEDBOX_STATE` selects another location. Use
`seedbox serve` to run the server in the foreground. Starting it never builds
assets or starts Docker/Podman. Offline artifact inspection and transfer remain
available for retained run directories. Offline `prepare`, `export` and `import`
use `--state` or `SEEDBOX_ARTIFACT_STATE`, defaulting to `.seedbox`.

## Learn more

| Topic | Guide |
| --- | --- |
| Configuration, faults and replay | [Runtime guide](docs/runtime.md) |
| Flags, defaults and advanced usage | [CLI reference](docs/cli-reference.md) |
| Build or replace guest assets | [Asset builds](tools/runtime/README.md) |
| Platform requirements | [Mac](tools/mac-runtime/README.md) · [Linux](tools/linux-runtime/README.md) |
| Design and direction | [Vision](docs/vision.md) |
| Determinism mechanisms and their invariants | [Technique catalogue](docs/determinism-catalogue.md) |

Compose preparation records one `wall_clock_epoch` (Unix seconds) in the
prepared inputs and each `SBXLAUNCH4` boot recipe. `x-seedbox.wall_clock_epoch`
sets it explicitly (at least 86400, because Linux cannot set a wall clock earlier
than the guest's uptime). Otherwise it is the latest `created` time in any
prepared image's config or history, and at least 86400. Builders stamp those
times after each step, so a certificate minted during a build is already valid,
and the same images always start at the same time on every host; preparation
never samples the host clock. The shared supervisor sets `CLOCK_REALTIME` before
the launch barrier on x86 and arm64. Replay uses retained bytes. Rebuilt images
can change the epoch and witnesses; replay of recorded inputs is unchanged. Both application kernels support unprivileged user namespaces,
including UID/GID maps and nested mount, PID, network, IPC and UTS namespaces.
The supervisor recursively mounts the image at the namespace root before
entering it, preserving named volumes and allowing `CLONE_NEWUSER`.

## Layout

| Path | Contents |
| --- | --- |
| [`crates/seedbox/`](crates/seedbox/README.md) | The `seedbox` CLI and private retained worker entrypoint |
| `crates/seedbox-application/` | Configuration parsing, OCI preparation and orchestration |
| `crates/seedbox-api/`, `crates/seedbox-spec/` | Public resource contracts and resolved experiment types |
| `crates/seedbox-client/`, `crates/seedbox-server/` | Local HTTP transport, durable catalogue and execution ownership |
| `crates/seedbox-vmm/` | KVM world engine linked into the Linux x86-64 CLI; isolated standalone workspace and lockfile for low-level fixtures |
| `crates/seedbox-engine-protocol/` | Worker requests, world configuration and retained evidence formats; no execution policy |
| `crates/seedbox-artifacts/` | Offline records, content storage, logs, comparison and bundles; no container engine or hypervisor dependency |
| `crates/seedbox-engine-core/` | Canonical deterministic policies, bounded virtio queues, shared MMIO transport, console/block/network models, the shared world coordinator and the [windowed scheduler](docs/windowed-scheduler.md) |
| `crates/seedbox-arm64/` | ARM PV machines, native HVF application world, and isolated fixture workspace |
| `tools/mac-runtime/` | Mac signing and platform notes |
| `examples/` | Compose applications: etcd, Redis, [TCP faults/recovery](examples/faults/README.md), and [one-service MinIO conditional writes](examples/minio/README.md) (MinIO runtime qualification pending) |
| `kernel/` | Guest kernel patch, config and build script (Linux 6.12.112) |
| `guest/` | Generic OCI guest supervisor plus low-level guest fixtures and builders |
| `docs/` | [Vision](docs/vision.md), [runtime instructions](docs/runtime.md), [backends](docs/backends.md), [acceptance record](docs/acceptance.md) |
| `harness/` | YAML acceptance scenarios, portable checks, low-level world fixtures and their batch runner ([migration map](harness/worlds/README.md)) |
| [`tools/mac-tcg/`](tools/mac-tcg/README.md) | Experimental local Mac TCG runner and tests; separate from the production KVM backend |

## Contributor checks

Run `nix develop --command bash harness/check.sh` for portable checks, or name
the platform suites to run: `bash harness/check.sh linux` on a Linux KVM host,
`bash harness/check.sh mac` on Apple Silicon (`--help` lists every suite).
See [Contributing](docs/contributing.md) for Linux host validation and fixtures.

### Interactive faults

Use `seedbox inject SIM_ID ACTION_FILE` to submit a typed YAML or JSON action.
For example, a file containing `action: stop` and `machine: db` stops the
application on machine `db`. Use `seedbox heal SIM_ID HANDLE` to remove a healable
network, clock or throttle fault. Both commands accept `--request-id` for safe
retries and emit an admission receipt; stream `seedbox events SIM_ID` to observe
the resulting effects. `ACTION_FILE` may be `-` for standard input.

Rerunning a completed simulation reapplies its retained interventions at the
same committed positions. Pausing, stepping and pacing remain available during
rerun; new fault injection is rejected. A server restart can recover an
interrupted simulation's durable intervention journal for rerun.

### Checkpoint branches (in development)

`seedbox checkpoint ID --request-id NAME` captures a paused simulation at a
committed boundary. List captures with `seedbox get checkpoints ID` and inspect
one with `seedbox describe checkpoint ID NAME` (both accept `--json`). Wait
for `checkpoint.state: captured` before running
`seedbox branch ID NAME --detach`. The branch gets a new simulation ID, preserving
its source seed, configuration and fault history up to the checkpoint. Its
metadata names the parent simulation and checkpoint. Follow its application
output with `seedbox logs BRANCH_ID [MACHINE]` and its internal events with
`seedbox events BRANCH_ID`. Historical application logs are not replayed.

Both Linux KVM and Apple Silicon HVF have native validation for capture, branch
logs, matching source results, fault isolation, and branch rerun after a server
restart.
`seedbox resume ID` continues a paused simulation; it does not create a branch.
After admitting a fault, step through one committed round before capturing a
checkpoint so the snapshot includes its effects.

New simulations use exact stop boundaries, with page timer and deferred TX
disabled. These engine settings have no Compose overrides. Machine memory,
timer jitter, network behavior and disk fault policy remain configurable.

### Exploring seeds

`seedbox explore . --seed 1 --count 10 --detach` asks the server to run ten
consecutive seeds using one preparation. Inspect them with
`seedbox get simulations SOURCE_ID`, or stop the range with
`seedbox stop SOURCE_ID --all-seeds`. Each member has an ordinary simulation ID
for describe, application logs and events. Closing the client does not cancel
accepted work. The server chooses concurrency; no worker-count flag is needed.

### Removing simulations

Use `seedbox delete simulation ID` to delete a simulation and its descendants,
including reruns, branches and seed-range members. The server stops the source
and descendants, waits for termination, and removes their catalogue resources,
checkpoints, retained input descriptors and private execution state. New
descendants are blocked while deletion is in progress. Active simulations can
be deleted directly. Repeating deletion succeeds. Shared content-addressed
objects remain available to other simulations; garbage collection is deferred.

Completed seed-range members may be deleted. `seedbox get simulations SOURCE_ID`
omits them and reports their count as `deletedMembers`.
