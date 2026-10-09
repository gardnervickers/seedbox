# Build and run on Linux

Install Nix and a Linux Docker or Podman engine. Enable KVM and give your user
read/write access to `/dev/kvm`. From the repository root:

```sh
nix develop
cargo build --release
./target/release/seedbox run examples/etcd --timebase pv
./target/release/seedbox replay .seedbox/runs/ATTEMPT
```

The release build compiles the native CLI. Guest kernels, static helpers and
filesystem tools are built independently in native Linux CI and downloaded on
first use. Cargo needs no container engine or guest build step. There is one
Seedbox executable. See [guest assets](../runtime/README.md) for the independent
release/cache contract and manual Nix fallback, and the
[shared workflow](../mac-runtime/README.md) for container-backed preparation.

PV execution requires KVM but no privileged performance-counter or MSR access.
It should run as your ordinary user. For `/dev/kvm` permission errors, follow the
specific instructions printed by Seedbox and log out and back in after changing
group membership. NixOS users should declare the `kvm` group in their system
configuration. Seedbox checks access before image preparation.

The default `counter` timebase retains its AMD/performance-counter requirements;
select `--timebase pv` for the ordinary unprivileged workflow. Replay remembers
the timebase and needs neither a container engine nor guest build tools.
Debug builds and explicit legacy assets retain the native Linux development
preparation path; use the release executable for application runs.

The default kernel downloads on the first `run`, `explore` or `prepare`, with
progress on stderr, and is cached under `~/.cache/seedbox/kernels` (or
`$XDG_CACHE_HOME/seedbox/kernels`). Subsequent calls reuse it offline. Private
release downloads require `GH_TOKEN` or `GITHUB_TOKEN` with repository read
access. Only `--kernel` or `SEEDBOX_KERNEL` overrides it; old artifact-directory
environment settings do not affect release runs. `doctor` uses a cached or
explicitly supplied kernel and never downloads one. See
[kernel releases](../../kernel/README.md). `--build-epoch` explicitly selects the existing native preparation path
and requires the project-Nix Podman, Skopeo and umoci tools. It still uses the
released guest assets. Native extraction needs image ownership permissions;
rootless users can run `prepare` inside `podman unshare`.
