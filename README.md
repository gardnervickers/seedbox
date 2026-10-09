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

Build the guest assets and Seedbox once:

```sh
nix develop
bash tools/build-assets.sh
cargo build --release
```

Then run the etcd example:

```sh
./target/release/seedbox --kernel /path/to/kernel examples/etcd/compose.yaml
```

The asset command prints the directory containing `kernel`. You can also omit
`--kernel` to use that locally built kernel automatically:

```sh
./target/release/seedbox examples/etcd
```

Seedbox finds your Docker or Podman socket, builds the example's workload oracle
with the engine's normal cache, prepares the images, and runs the Linux guests.
Progress appears on stderr. Set `DOCKER_HOST` if your engine uses a custom socket.
Application progress comes from automatic binary instrumentation on both
platforms; see [automatic progress](docs/progress.md).

On Linux, `--watchdog-ms 200` raises the host CPU interval (default 20 ms)
after which a guest that makes no exits is interrupted so the engine can
report where it is stuck. It changes performance only and can also be set on
`replay` or `explore`.

**The kernel is the only separate guest asset.** The release executable embeds
its supervisor and filesystem tool. No private GitHub access or runtime guest
build is needed. Locally built kernels live under `/tmp/seedbox-UID/ARCH` (the invoking user's UID, even under `sudo`); rebuild
if the OS removes them. Seedbox never downloads kernels automatically.


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

A run prints its result and directory. Replay needs neither the container engine
nor the original kernel, source files or image registry:

```sh
./target/release/seedbox replay .seedbox/runs/ATTEMPT
./target/release/seedbox logs .seedbox/runs/ATTEMPT
./target/release/seedbox inspect .seedbox/runs/ATTEMPT
```

Retained runs keep their own inputs. Delete run directories when you no longer
need them; clearing temporary build assets does not remove run data.

## Learn more

| Topic | Guide |
| --- | --- |
| Configuration, faults and replay | [Runtime guide](docs/runtime.md) |
| Flags, defaults and advanced usage | [CLI reference](docs/cli-reference.md) |
| Build or replace guest assets | [Asset builds](tools/runtime/README.md) |
| Platform requirements | [Mac](tools/mac-runtime/README.md) · [Linux](tools/linux-runtime/README.md) |
| Design and direction | [Vision](docs/vision.md) |

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
| [`crates/seedbox/`](crates/seedbox/README.md) | Configuration, OCI preparation, orchestration and the `seedbox` CLI |
| `crates/seedbox-vmm/` | KVM world engine linked into the Linux x86-64 CLI; isolated standalone workspace and lockfile for low-level fixtures |
| `crates/seedbox-engine-core/` | Canonical deterministic policies, bounded virtio queues, shared MMIO transport, console/block/network models, the shared world coordinator and the [windowed scheduler](docs/windowed-scheduler.md) |
| `crates/seedbox-arm64/` | ARM PV machines, native HVF application world, and isolated fixture workspace |
| `tools/mac-runtime/` | Mac signing and platform notes |
| `examples/` | Compose applications: etcd, Redis, [TCP faults/recovery](examples/faults/README.md), and [one-service MinIO conditional writes](examples/minio/README.md) (MinIO runtime qualification pending) |
| `kernel/` | Guest kernel patch, config and build script (Linux 6.12.112) |
| `guest/` | Generic OCI guest supervisor plus low-level guest fixtures and builders |
| `docs/` | [Vision](docs/vision.md), [runtime instructions](docs/runtime.md), [backends](docs/backends.md), [acceptance record](docs/acceptance.md) |
| `harness/` | YAML acceptance scenarios, portable checks, low-level world fixtures and their batch runner ([migration map](harness/worlds/README.md)), `msr54.py` |
| [`tools/mac-tcg/`](tools/mac-tcg/README.md) | Experimental local Mac TCG runner and tests; separate from the production KVM backend |

## Contributor checks

Run `nix develop --command bash harness/check.sh` for portable checks, or name
the platform suites to run: `bash harness/check.sh linux` on a Linux KVM host,
`bash harness/check.sh mac` on Apple Silicon (`--help` lists every suite).
See [Contributing](docs/contributing.md) for Linux host validation and fixtures.
