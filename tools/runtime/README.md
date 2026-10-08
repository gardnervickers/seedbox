# Guest helper releases

`cargo build --release` compiles the native CLI and signs it on Mac. It never
starts a container, invokes Nix or builds Linux helpers. Native Linux CI builds
those helpers separately through `nix/runtime.nix`:

- `runtimeAssets`: static supervisor, BusyBox, mke2fs and license notices.
- `preparationImage`: Skopeo, umoci, Python and tar for Linux filesystem handling.
- `runtimeArchive`: both outputs in one ordinary `.tar.gz` release asset.
- `runtimeSources`: the helper recipe, locked Nix inputs and upstream helper sources.

The filesystem-tools image contains no Seedbox executable. It runs only during
application preparation, through the user's Docker/Podman endpoint. The native
CLI owns configuration, validation, identity and boot archive assembly.

## Download and cache

The [Guest assets workflow](../../.github/workflows/kernels.yml) builds native
ARM64 and x86-64 archives. Pull requests upload temporary artifacts; pushes to
`main` publish `seedbox-runtime-aarch64.tar.gz`, `seedbox-runtime-x86_64.tar.gz`,
corresponding helper source and checksums. Both builds must succeed before
publication. Failed draft uploads can resume; published assets are never replaced.

`tools/runtime/inputs.json` lists the helper sources, filesystem-tool recipe and
locked Nix inputs. Cargo and CI hash the same files to select `runtime-<SHA256>`.
An existing release skips helper builds. Kernel recipes, patches and configuration
are excluded; changes to the actual shared supervisor header still count.
Kernel releases have their own independent input list. Changes to application
images, CLI code or documentation rebuild neither family of assets.

On first preparation, Seedbox announces the download on stderr and verifies
GitHub's SHA-256 asset digest before installing it atomically. The archive cache
is `~/Library/Caches/seedbox/assets` on Mac or `~/.cache/seedbox/assets` on Linux;
`XDG_CACHE_HOME` overrides the parent. Each recipe and architecture has its own
entry. Concurrent invocations share one download; interrupted transfers never
become usable entries. Cached bytes are checked locally on reuse, without HTTP.
The extracted tools live in a separate content-addressed `seedbox/runtime` cache.

Because this repository is private, downloads need `GH_TOKEN` or `GITHUB_TOKEN`
with repository read access. Credentials are not stored in the cache or sent
to redirected asset hosts. `--runtime-assets PATH` or `SEEDBOX_RUNTIME_ASSETS`
selects a local archive and bypasses its download. Overrides are not restricted
to a release hash. An invalid archive/cache reports an error before preparation.

`doctor` needs only a cached or explicit kernel and never fetches helpers.
Replay uses retained inputs and never reads helper caches or starts a container.

## Build without private release access

From an existing checkout on native Linux, use the same recipes as CI:

```sh
nix build .#guestKernel --out-link kernel-output
nix build .#runtimeArchive --out-link runtime-assets.tar.gz

# Linux x86-64:
./target/release/seedbox run examples/etcd --timebase pv \
  --kernel ./kernel-output/bzImage --runtime-assets ./runtime-assets.tar.gz
```

On ARM64 Linux the kernel output is `kernel-output/Image`. For an Apple Silicon
Mac, build on ARM64 Linux, copy `Image` and the archive to the Mac, then run:

```sh
./target/release/seedbox run examples/etcd \
  --kernel ./Image --runtime-assets ./runtime-assets.tar.gz
```

These Nix outputs need a Linux builder. Neither command requires Seedbox release
access; the first build still fetches public source/toolchain dependencies.
Fully offline builds need those inputs already present. A running Docker/Podman
engine is needed to prepare application images, including with manual assets.
