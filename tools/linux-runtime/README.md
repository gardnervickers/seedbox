# Build and run on Linux

Install Nix and a Linux Docker or Podman engine. Enable KVM and give your user
read/write access to `/dev/kvm`. From the repository root:

```sh
nix develop
cargo build --release
./target/release/seedbox run examples/etcd --timebase pv
./target/release/seedbox replay .seedbox/runs/ATTEMPT
```

The release build prepares and embeds the patched x86 Linux kernel, static
supervisor, BusyBox, mke2fs and a filesystem-tools container image. Kernel builds
and their dependencies execute inside the container engine. No second Seedbox
executable is built. Assign at least 4 GiB RAM to the container engine for guest asset builds.
The first build needs network access and a running engine;
unchanged guest inputs reuse `$XDG_CACHE_HOME/seedbox/assets` or
`~/.cache/seedbox/assets`. See the [shared workflow](../mac-runtime/README.md)
for image preparation and cache behavior.

PV execution requires KVM but no privileged performance-counter or MSR access.
It should run as your ordinary user. For `/dev/kvm` permission errors, follow the
specific instructions printed by Seedbox and log out and back in after changing
group membership. NixOS users should declare the `kvm` group in their system
configuration. Seedbox checks access before image preparation.

The default `counter` timebase retains its AMD/performance-counter requirements;
select `--timebase pv` for the ordinary unprivileged workflow. Replay remembers
the timebase and needs neither a container engine nor guest build tools.
Debug builds and explicit legacy assets retain the native Linux development
preparation path; the release executable is the supported packaged workflow.

The packaged kernel is the default. Only `--kernel` or `SEEDBOX_KERNEL`
overrides it; old artifact-directory environment settings do not affect packaged
runs. `--build-epoch` explicitly selects the existing native preparation path
and requires the project-Nix Podman, Skopeo and umoci tools. It still uses the
packaged guest assets. Native extraction needs image ownership permissions;
rootless users can run `prepare` inside `podman unshare`.
