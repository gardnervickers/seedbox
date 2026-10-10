# Build Seedbox and guest inputs

From the checkout's development shell:

```sh
python3 tools/build.py
./build/current/seedbox examples/etcd
```

The build resolves the current guest kernel and runtime through Nix, then passes
that exact helper archive to Cargo. Linux builds the guest inputs directly;
Apple Silicon uses a Docker-compatible Linux container. Start Podman or Docker
Desktop first; `SEEDBOX_ASSET_BUILDER` can select either explicitly. Mac builders
retain Nix's store in an engine volume, scoped to the builder image, so subsequent
builds reuse dependencies. Nix tracks the guest sources and recipes; there are no
Git revision stamps or runtime qualification checks.

Each successful invocation prints a completed directory under `build/`, containing
`seedbox`, `kernel`, and `runtime.tar.gz`. `build/current` points to the latest
complete CLI build. Publication is atomic; a failed build leaves the previous
one available. Concurrent invocations in one checkout serialize; separate
worktrees have separate outputs and Cargo caches. Existing completed directories
remain usable until explicitly removed. The native Cargo cache is `build/cargo`.

The CLI embeds the helpers and finds `kernel` beside its executable. Copy those
two files together to run elsewhere. Runtime commands never invoke Nix, Cargo,
a build container, or asset downloads. Application image preparation still uses
the user's Docker-compatible engine. Replay uses retained inputs and needs
neither the builder nor the original build directory.

Hardware application checks invoke this same complete build and use its returned
directory explicitly. They cannot inherit a stale global asset slot. Portable
checks build ordinary unbundled Cargo executables and require no guest build.
A plain `cargo build --release` also remains unbundled unless
`SEEDBOX_BUILD_HELPERS=/path/to/runtime.tar.gz` explicitly selects an archive;
Cargo never builds guest inputs itself.

## Explicit inputs

`python3 tools/build.py --assets-only` builds a separate kernel/runtime pair and
prints its directory without changing `build/current`. On Linux the individual
Nix outputs are also available:

```sh
nix build .#guestKernel --out-link kernel-output
nix build .#runtimeArchive --out-link runtime-assets.tar.gz
```

Use `--kernel PATH` and `--runtime-assets PATH` (or `SEEDBOX_KERNEL` and
`SEEDBOX_RUNTIME_ASSETS`) to run intentionally selected historical or experimental
inputs. `SEEDBOX_ASSET_DIR` still explicitly selects the legacy `ARCH/kernel`
and `ARCH/runtime.tar.gz` directory layout. It is never read by the complete
build or used implicitly by Cargo. Archive extraction validates names, types,
duplicates, required files, and size limits; no provenance manifest is needed.
