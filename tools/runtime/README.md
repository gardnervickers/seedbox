# Build guest assets

From the repository root:

```sh
bash tools/build-assets.sh
```

On Linux, this uses Nix directly. On Apple Silicon macOS, start a
Docker-compatible Linux engine such as Podman first. The script runs Nix in a
throwaway Linux build container. Seedbox uses the engine API for application images; Cargo never invokes it.
Use `SEEDBOX_ASSET_BUILDER=podman` or `docker` to choose one explicitly.

The command builds the patched kernel, static supervisor and mke2fs.
It installs `kernel` and `runtime.tar.gz` under
`/tmp/seedbox-UID/ARCH` (override the parent directory with `SEEDBOX_ASSET_DIR`).
The directory is private to your user. Rebuilds replace these two files rather
than retaining a version history. `cargo build --release` embeds the helper
archive in Seedbox; the archive is no longer needed to run that executable.
Seedbox extracts its embedded helpers into a private per-invocation temporary
directory and removes that extraction when finished. Only the kernel remains
a separate runtime file.
Helper archives contain only `compose-init`, `mke2fs`, and upstream `NOTICE.txt`.
No helper hash manifest or provenance metadata is required. Archive extraction
still checks member names, types, duplicates, required files, and size limits.
Nix and the container engine retain their own ordinary build/image storage.

The first build needs public source downloads, but no private GitHub credentials.
If the OS clears the temporary assets, rerun the command. Runtime commands never
build or download missing guest assets.

## Build outputs separately

On a native Linux builder, these are the same recipes used by the script and CI:

```sh
nix build .#guestKernel --out-link kernel-output
nix build .#runtimeArchive --out-link runtime-assets.tar.gz
```

Select your own outputs with `--kernel PATH` and `--runtime-assets PATH`, or
`SEEDBOX_KERNEL` and `SEEDBOX_RUNTIME_ASSETS`. The kernel is `kernel-output/Image`
on ARM64 and `kernel-output/bzImage` on x86-64. Match the guest architecture.
These overrides accept locally built files without release/version admission.

CI can still publish guest artifacts. Automatic runtime downloads are deferred
until public distribution is available; they are not part of the quick start.
Replay uses its retained inputs and needs neither temporary assets nor a builder.

Set `SEEDBOX_BUILD_HELPERS=/path/to/runtime.tar.gz` to embed an explicit archive
at build time. An explicitly selected missing archive is a build error. If the
default archive is absent, release builds warn and omit embedding; runtime
commands then need locally built helpers and report the asset-build command
when those are missing.
Debug builds omit embedding for portable checks and can use the local helper
archive or an explicit `--runtime-assets` override.
