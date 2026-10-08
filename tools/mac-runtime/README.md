# Build and run on Apple Silicon

Install Nix and Apple's Command Line Tools. Install and start a Linux container
engine: Docker Desktop or Podman with a running machine. The project shell
provides Rust, Python and container clients; it does not start the engine.

From the repository root:

```sh
nix develop
cargo build --release
./target/release/seedbox run examples/etcd
./target/release/seedbox replay .seedbox/runs/ATTEMPT
```

The normal release build packages the static guest supervisor, BusyBox, mke2fs
and Linux filesystem tools. It signs the Mac CLI with the Hypervisor entitlement
automatically after linking. It does not compile or download a kernel.

On the first `run`, `explore` or `prepare`, Seedbox reports the kernel download
on stderr, verifies the GitHub release asset's SHA-256 and caches the bytes under
`~/Library/Caches/seedbox/kernels` (or `$XDG_CACHE_HOME/seedbox/kernels`). Later
calls reuse that cache offline. The private repository requires `GH_TOKEN` or
`GITHUB_TOKEN` with read access for the first download. `--kernel PATH` skips it.
`doctor` uses a cached or explicitly selected kernel and performs no download.
See [kernel releases](../../kernel/README.md).

The independent kernel recipe lives in `nix/kernels.nix`. Guest helpers and the
filesystem-tools image use `runtimeAssets` and `preparationImage` in
`nix/runtime.nix`; Cargo copies those artifacts from one Linux build container.
The helper cache is under `~/Library/Caches/seedbox/assets`; ordinary application
and kernel patch changes do not rebuild the static helpers. Debug builds support
portable development and readers; use the release executable for application runs.

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
