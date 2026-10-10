# Build Seedbox and guest inputs

From the checkout (inside `nix develop` on Linux):

```sh
cargo xtask build-assets
cargo build --release
./target/release/seedbox examples/etcd
```

The first command builds the current guest kernel and helpers using Nix. Linux
x86-64 selects `bzImage`; Linux AArch64 selects `Image`. Apple Silicon runs the
ARM64 Nix build inside Docker or Podman. Start that engine first; optionally
select it with `SEEDBOX_ASSET_BUILDER`. Its Nix store persists in an engine volume.

Successful asset builds atomically update the checkout's `build/assets` link.
Failed builds leave the previous assets available. Concurrent asset builds are
serialized, and old asset directories remain available. Rerun the first command
when changing guest sources or recipes; Cargo does not build guest inputs.

The second command embeds the prepared kernel and helpers into the executable.
Cargo tracks the asset link and selected files, and rejects assets built for a
different target architecture. Explicit `SEEDBOX_BUILD_KERNEL` and
`SEEDBOX_BUILD_HELPERS` override the prepared inputs. Without prepared assets,
plain Cargo development builds retain explicit runtime-asset lookup.

Copy just `target/release/seedbox` to another host of the same platform. Runtime
commands never invoke Nix, Cargo, containers to build assets, or asset downloads.
Application image preparation still uses the user's Docker-compatible engine.
The embedded kernel is extracted to the user cache, keyed by its contents.
`--kernel` and `--runtime-assets` remain available for development overrides.

Hardware acceptance uses these same build steps and copies its selected inputs
and executable into a private report directory before running guests.
