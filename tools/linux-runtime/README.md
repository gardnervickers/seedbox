# Linux x86-64

Install Nix and grant your user read/write access to `/dev/kvm`. From the checkout:

```sh
nix develop
bash tools/build-assets.sh
cargo build --release
```

The asset command builds the kernel and static helpers directly with Nix. Linux
asset builds need no container engine. Seedbox prepares application images through your Docker or Podman engine, then
runs guests through KVM; see the [quick start](../../README.md#quick-start).

Use `--timebase pv` for ordinary unprivileged KVM execution. The default counter
timebase additionally requires supported performance-counter hardware and
permissions. Setup errors explain the missing capability and how to resolve it.

The kernel lives under the OS temporary directory; `--kernel` overrides it.
Release builds embed the guest helpers. Seedbox never downloads guest assets or
starts an engine daemon. Replay uses retained inputs without contacting an engine.
