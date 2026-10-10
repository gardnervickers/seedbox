# Seedbox

**Run real Linux applications. Reproduce the same execution.**

Seedbox runs each service in its own Linux virtual machine and controls time,
networking and disk faults. Describe your application in `seedbox.yaml`, choose a
seed, and replay a failure using the exact inputs retained by the run.

![Seedbox TUI: isolate an etcd node, observe dropped traffic, then heal the partition](docs/assets/seedbox-tui.gif)

*An actual three-node etcd simulation: inject a network partition, watch traffic
and application logs, then heal it. The fault timeline is saved with the simulation.*

[Quick start](#quick-start) · [Your application](#your-application) · [Runtime guide](docs/runtime.md) · [Contributing](docs/contributing.md)

## Quick start

You need **Nix** for the build tools and a running **Docker-compatible engine**
(such as Docker or Podman) for application images. Apple Silicon Macs also need
Apple's Command Line Tools. Linux x86-64 needs read/write access to `/dev/kvm`.

Build Seedbox and its current guest inputs together:

```sh
nix develop
cargo xtask build-assets
cargo build --release
```

`cargo xtask build-assets` builds the guest inputs; `cargo build --release`
embeds them, including the architecture-matched kernel, into the CLI. Separate
worktrees have independent builds; Nix reuses unchanged guest dependencies.

### Explore a real etcd cluster

Start the example paused, then open the TUI using the simulation ID it prints:

```sh
./target/release/seedbox run examples/etcd --paused --detach
./target/release/seedbox tui <sim-id>
```

Press **Space** to start the cluster. Once the machines are ready, press **f**
and choose **Partition network**. Name the fault, put `n1` on one side and
`n2` / `n3` on the other, then activate **Submit** with Enter. The menu pauses
simulation time while you configure the fault and restores playback afterward.

Use **←/→** to browse choices, **Space** to toggle machines in a group, and
**Enter** to confirm and advance. Choose **Heal fault** to restore the link.
Click panes to focus them and scroll application logs with the mouse wheel.
The **3 timeline** tab shows the recorded faults; edit the timeline and rerun
it to try a different sequence.

Save the complete definition or verify the completed run with replay:

```sh
./target/release/seedbox describe simulation <sim-id> > simulation.yaml
./target/release/seedbox run simulation.yaml --paused --detach
./target/release/seedbox replay <sim-id>
```

Definitions retain their binary inputs in the server's cache across restarts.
The simulation catalogue and live logs remain temporary.

Seedbox finds your Docker or Podman socket, builds the example's workload oracle
with the engine's normal cache, prepares the images, and runs the Linux guests.
Progress appears on stderr. Set `DOCKER_HOST` if your engine uses a custom socket.
Application progress comes from automatic binary instrumentation on both
platforms; see the [determinism catalogue](docs/determinism-catalogue.md).

If no local server is running, an interactive client asks before starting one.
The server stays running for subsequent commands. Noninteractive callers must
start `seedbox serve` explicitly. Use `seedbox shutdown` to stop the server and
its active work gracefully. `--detach` returns the simulation ID immediately. Foreground Ctrl-C sends
an explicit stop request. Use `seedbox rerun ID` to repeat a simulation,
`seedbox compare FIRST_ID SECOND_ID` to compare results, or `seedbox replay ID`
to rerun and compare in one command. Replay reports `MATCH`, `MISMATCH` or
`NOT_COMPARABLE`; that comparison does not change the workload oracle verdict. Host watchdog timing uses the engine default.
Use `seedbox get simulation ID --watch` to follow status from another client.
Ctrl-C detaches the watcher; it does not stop the simulation.

The dedicated oracle exits zero to pass; any nonzero exit fails. Success codes
are not configurable.

Simulation preparation has a 15-minute server deadline, separate from execution
limits. On expiry the server cancels preparation and waits for cleanup. Without
an oracle exit, a preparation or execution timeout reports `TIMEOUT` (exit 124)
and the simulation's test verdict remains incomplete.


**The complete executable includes its guest assets.** The build embeds the
kernel, supervisor, filesystem tool and progress runtime for its architecture
(x86-64 or AArch64). Copy just `seedbox` to another host of the same platform.
At runtime the kernel is extracted to `$XDG_CACHE_HOME/seedbox/kernels`
(or `$HOME/.cache/seedbox/kernels`), keyed by its contents. Running or replaying Seedbox
never invokes Nix, Cargo, or an asset builder.

Guest time starts at the latest build time recorded in the prepared images, so
independent fresh runs of the same images start at the same time and can be
compared directly. Resolved simulation definitions retain the exact guest clock.

### Your application

A native project describes application machines and a dedicated oracle:

```yaml
name: database-test
machines:
  database:
    image: quay.io/coreos/etcd:v3.5.2
oracle:
  build:
    context: ./oracle
network:
  latency: 1ms
simulator:
  blocksPerProgressCall: 10000
```

See the [annotated project format](docs/project-format.md) for all authoring fields.
The [etcd example](examples/etcd/seedbox.yaml) includes its application commands
and oracle. Edit the oracle and run the same command again: the engine reuses
unchanged build layers. `seedbox.yaml` is preferred during directory discovery;
existing Compose files remain readable.

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
./target/release/seedbox get simulations
./target/release/seedbox describe simulation SIM_ID
./target/release/seedbox rerun SIM_ID
./target/release/seedbox stop SIM_ID
./target/release/seedbox events SIM_ID
./target/release/seedbox playback SIM_ID
./target/release/seedbox pause SIM_ID
./target/release/seedbox step SIM_ID
./target/release/seedbox rate SIM_ID 1/10
./target/release/seedbox resume SIM_ID
./target/release/seedbox logs SIM_ID              # All machines.
./target/release/seedbox logs SIM_ID MACHINE_NAME # One machine.
./target/release/seedbox tui [SIM_ID]             # Interactive console.
```

Describe emits YAML with the resolved specification, status and timings. Save it
and submit its positional path with `seedbox run simulation.yaml`. After preparation, its definition
references retained binary assets, so rerun needs neither the container engine nor
the original project, image registry, or kernel path. References remain local to this
server's storage. Each rerun receives a new ID and preserves the original.

Server state defaults to `$XDG_STATE_HOME/seedbox` or
`$HOME/.local/state/seedbox`; `SEEDBOX_STATE` selects another location. Use
`seedbox serve` to run the server in the foreground. Starting it never builds
assets or starts Docker/Podman. Offline artifact inspection and transfer remain
available for retained run directories. Offline `prepare`, `export` and `import`
use `--state` or `SEEDBOX_ARTIFACT_STATE`, defaulting to `.seedbox`.

Start at the initial execution boundary with `seedbox run PROJECT --paused --detach`,
then open `seedbox tui ID` to inspect and resume it. `--paused` also works with
`explore`; every member starts paused. Preparation still runs before the pause.
In the TUI, press `f` to choose a server-discovered fault, edit its generated
parameter form, and activate Submit with Enter. Enter confirms a displayed
choice and advances to the next field; Tab moves between fields and Submit.
The menu pauses execution while you
edit, then restores the previous speed after admission (or cancellation).
Without `--detach`, the command waits for completion while another client controls
playback. Initial playback state does not change deterministic results; `rerun`,
`replay` and checkpoint branches start with normal playback.

Logs fill the bottom pane; click to focus and use the mouse wheel or Page Up/Page
Down to scroll. End returns to live output. Switching views never changes playback.

The TUI's `3 timeline` view saves named recordings locally with application, seed
and date. Remove/restore entries with `d`, clear with `C` twice, undo with `u`,
and press Enter to run an edited recording as a new paused simulation after its
definition is ready. `N` selects the next fault; `p` runs to just before it. Rerunning an unchanged definition preserves its ordered overrides. Fault forms accept schema-advertised units
such as `100ms` and `2.5%`, and keep values when the server rejects a submission.

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
| `crates/seedbox-client/`, `crates/seedbox-server/` | Local HTTP transport, in-memory catalogue and execution ownership |
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
same deterministic simulated-time positions. Pausing, stepping, pacing, and new
external overrides remain available during rerun. New interventions form a new
working definition. Restarting the server discards its catalogue; saved definitions
remain usable when their binary assets are available.

### Checkpoint branches (in development)

`seedbox checkpoint ID --request-id NAME` captures a paused simulation at a
committed boundary. List captures with `seedbox get checkpoints ID` and inspect
one with `seedbox describe checkpoint ID NAME` (both accept `--json`). Wait
for `checkpoint.state: captured` before running
`seedbox branch ID NAME --detach`. The branch gets a new simulation ID, preserving
its source seed, configuration and fault history up to the checkpoint. Its
annotations may name the source simulation and checkpoint; the new definition
is independent of that source. Follow its application
output with `seedbox logs BRANCH_ID [MACHINE]` and its internal events with
`seedbox events BRANCH_ID`. Historical application logs are not replayed.

Both Linux KVM and Apple Silicon HVF have native validation for capture, branch
logs, matching source results, fault isolation, and branch rerun after a server
restart.
`seedbox resume ID` continues a paused simulation; it does not create a branch.
After admitting a fault, step through one committed round before capturing a
checkpoint so the snapshot includes its effects.

Native projects use fixed machine and engine defaults. Baseline latency and
PV progress budget are optional authoring settings; advanced faults belong in
the oracle or interactive control API.

### Exploring seeds

`seedbox explore . --seed 1 --count 10 --detach` asks the server to run ten
consecutive seeds using one preparation. Inspect them with
`seedbox get simulations SOURCE_ID`, or stop the range with
`seedbox stop SOURCE_ID --all-seeds`. Each member has an ordinary simulation ID
for describe, application logs and events. Closing the client does not cancel
accepted work. The server chooses concurrency; no worker-count flag is needed.

### Removing simulations

Use `seedbox delete simulation ID` to delete a simulation and its owned resources,
including checkpoints and seed-range members. The server stops owned executions,
waits for termination, and removes their catalogue entries and private execution
state. New dependents are blocked while deletion is in progress. Independent
reruns and submitted checkpoint definitions carry only provenance annotations;
deleting their source does not delete them. Active simulations can be deleted
directly. Repeating deletion succeeds. Shared binary assets remain available to
other simulations for the server session.

Completed seed-range members may be deleted. `seedbox get simulations SOURCE_ID`
omits them and reports their count as `deletedMembers`.

### Resource tables and shell completion

`seedbox get simulations` and other `get` commands show concise tables;
`seedbox describe simulation ID` shows the full resource as YAML. Use `--json`
for structured output. Logs include recent buffered application output, including
for completed simulations while the server still holds it.

Generate shell integration with `seedbox completions bash`,
`seedbox completions zsh`, or `seedbox completions fish`. Completion queries the
existing local server for simulation IDs and machine names; it never starts a
server or prompts. Unavailable or slow servers simply yield no dynamic suggestions.

Server metadata and log buffers are session-only and live in memory. Execution
uses private scratch directories under the server state's `sessions/` directory.
Immutable binary inputs (kernels, engines, boot archives and captured checkpoint
binaries) and image preparation cache entries live separately under `cache/` and
survive server restarts. Saved simulation definitions can reuse these assets on
the same server state directory; simulation IDs, run evidence and observations
remain session-only. The binary cache is not automatically pruned.
Graceful shutdown removes the session directory after workers stop and reports
cleanup failures. The next server startup, under the exclusive server lock,
removes abandoned session directories left by a crash. Existing offline runs
and older persistent catalogue data are not deleted or imported automatically.
Resource tables show newest simulations first; checkpoint lists use descending
simulation timeline time, with uncaptured requests last.
