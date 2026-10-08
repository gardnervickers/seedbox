# Build and run on Apple Silicon

Install Nix and Apple's Command Line Tools. Application preparation needs a
running Linux container engine: Docker Desktop or a Podman machine. The project
shell provides Rust, Python and container clients; it does not start the engine.
The Cargo build itself needs no running engine.

From the repository root:

```sh
nix develop
cargo build --release
./target/release/seedbox run examples/etcd
./target/release/seedbox replay .seedbox/runs/ATTEMPT
```

The release build compiles the native CLI and automatically signs it with the
Hypervisor entitlement after linking. Linux guest assets are built in native
Linux CI, independently of Cargo.

On the first `run`, `explore` or `prepare`, Seedbox announces downloads of its
guest kernel and helper archive on stderr, verifies GitHub's SHA-256 digests and
caches the bytes under `~/Library/Caches/seedbox/kernels` and
`~/Library/Caches/seedbox/assets` (or beneath `$XDG_CACHE_HOME/seedbox`). Later
calls reuse those caches offline. The private repository requires `GH_TOKEN` or
`GITHUB_TOKEN` with read access for first use. `--kernel PATH` and
`--runtime-assets PATH` bypass the respective downloads. `doctor` uses only a
cached or explicit kernel and performs no download. See
[kernel releases](../../kernel/README.md) and
[guest assets](../runtime/README.md), including the manual Linux build fallback.

The kernel and helper recipes have independent release identities. Kernel patch
changes reuse the helpers; helper changes reuse the kernel. Application and CLI
changes rebuild neither. Debug builds support portable development and readers;
use the release executable for application runs.

At runtime Seedbox uses your container engine to pull or build application
images. It freezes build contexts before building and exports resolved image
bytes. An isolated, network-disabled helper container unpacks layers on Linux;
Seedbox reads the resulting archive without extracting Linux metadata onto the
Mac filesystem. Preparation needs neither Lima nor a separate Linux VM owned by
Seedbox. Data transfers use the container client, not host filesystem mounts,
so the endpoint may be remote. Use `--builder docker` or `--builder podman` to
select a client; its ordinary context/connection configuration applies.

The helper container is removed after preparation. Application machines run
directly through HVF, one worker process and one vCPU per service. Replay uses
the retained executable, kernel and boot archives; it needs no container engine,
image pulls, compiler or preparation helper.

Images must support `linux/arm64`. Guest memory is 64..3072 MiB per service.
Exploration currently supports one worker, and arbitrary ARM snapshot/fork reuse
remains unsupported. `--build-epoch` is supported only by the existing native
Linux Podman preparation path, not container-backed preparation. Kernel and
guest-helper overrides remain available for development. Progress goes to
stderr; result and JSON output retain their existing contracts.
