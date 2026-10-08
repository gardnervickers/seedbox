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

The normal release build downloads and patches the ARM Linux kernel, builds the
static guest supervisor and packages BusyBox and mke2fs. It also builds a Linux
filesystem-tools image containing Skopeo, umoci, tar and Python. No second
Seedbox executable or Rust compiler is shipped in that image. Cargo signs the
Mac CLI with the Hypervisor entitlement automatically after linking.

The Linux build uses the `runtimeAssets` and `preparationImage` Nix packages in
`nix/runtime.nix`. The latter uses Nix's image builder and minimal Python;
Cargo copies these artifacts from one Linux build container. It does not
assemble Nix closures or export a second daemon-built image itself.

Assign at least 4 GiB RAM to the container engine for guest asset builds.
The first build needs network access and a running container engine. Unchanged
guest inputs reuse a verified content-addressed cache under
`~/Library/Caches/seedbox/assets` (`$XDG_CACHE_HOME/seedbox/assets` when set).
Kernel patches, configuration, guest sources, recipes and the flake lock are
part of the cache key. Ordinary application changes do not rebuild the kernel.
Debug builds support portable development and readers; use the release
executable for application runs.

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
