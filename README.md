# Seedbox

## Build

Requires Nix and a running Docker or Podman engine on Linux x86-64 or Apple
Silicon macOS. macOS also needs Apple's Command Line Tools.

```sh
git clone https://github.com/gardnervickers/seedbox.git
cd seedbox
seedbox_flake=$(bash tools/dev-flake.sh) && nix develop "$seedbox_flake"
cargo xtask build --release
alias seedbox="$PWD/target/release/seedbox"
```

## Run the etcd example

PV uses hardware virtualization (KVM on Linux, HVF on macOS); TCG emulates the
CPU in software and is slower but needs no hardware virtualization access.
Linux PV requires access to `/dev/kvm`.

```sh
seedbox server start

# PV
seedbox run examples/etcd --backend pv --paused --detach

# Or TCG
seedbox run examples/etcd --backend tcg --paused --detach
```

## Open the TUI

Use the simulation ID printed by `run`:

```sh
seedbox tui <sim-id>
```

Press **Space** to resume or pause, and **f** to inject or heal faults.
