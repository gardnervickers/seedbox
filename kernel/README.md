# Guest kernel releases

The `Guest assets` GitHub Actions workflow builds native ARM64 and x86-64 Linux
kernels and publishes ordinary files in this repository's Releases. It runs on
asset-related pull requests and pushes to `main`, and supports **Run workflow**
for an on-demand build on `main`. Pull requests upload temporary build artifacts;
only `main` publishes releases. No container registry or Docker engine is involved.

`kernel/inputs.json` lists the patches, configuration, upstream source and build
inputs. CI checks which paths changed and builds only the affected asset family.
Releases use ordinary `kernel-<CI run number>` labels; Cargo does not compute or
embed a source fingerprint. Unrelated CLI, application or guest supervisor
changes do not build a kernel. Configuration, Linux version, build recipe and
toolchain changes also trigger builds. `nix/kernels.nix` keeps kernel compilation
separate from helper compilation. The ARM instruction audit runs before publication.

A release includes `seedbox-kernel-aarch64`, `seedbox-kernel-x86_64`, diagnostic
archives with resolved config/licenses (and ARM audit or x86 symbol map),
`SHA256SUMS` and `seedbox-kernel-source.tar.gz`. The source archive includes the
exact upstream Linux source, Seedbox patches and a standalone Nix flake/lock for
rebuilding with `nix build .#guestKernel`. Both architecture builds must succeed
before a draft release is published. A failed upload can resume its draft;
published release assets are never overwritten by this workflow. Kernel releases
are prereleases and do not replace the project's latest application release.

`cargo build --release` does not build or download a kernel. The first `run`,
`explore` or `prepare` announces the latest published kernel download on stderr
and streams it from GitHub. Because the repository is private, provide `GH_TOKEN` (or
`GITHUB_TOKEN`) with repository read access. Tokens are not stored in the cache
or sent to redirected asset hosts. An explicit `--kernel PATH` or
`SEEDBOX_KERNEL` skips download and cache lookup.

The cache is `~/Library/Caches/seedbox/kernels` on Mac or
`~/.cache/seedbox/kernels` on Linux; `XDG_CACHE_HOME` overrides the parent.
Each architecture has its own entry. Downloads are size-bounded, checked
against GitHub's SHA-256 asset digest and installed atomically. Concurrent callers
share a lock. Later calls verify cached bytes without contacting GitHub; removing
the cache causes the next run to download again. Interrupted downloads never
publish a complete entry. A corrupt entry reports its exact path and remedy.

`doctor` only looks up a cached or explicitly selected kernel; it never downloads
one or creates state. Replay uses the kernel retained with the run and never
contacts GitHub, including after kernel releases change. There is no compiled-in
kernel version or admission pin. To use a newer release, remove the architecture's
cache entry or select a local file with `--kernel`. SHA-256 checks detect damaged
downloads and cache entries.

## Build without private release access

From an existing checkout on native Linux, use the same Nix recipe as CI:

```sh
nix build .#guestKernel --out-link kernel-output

# Linux x86-64:
./target/release/seedbox run examples/etcd --timebase pv --kernel ./kernel-output/bzImage
```

On ARM64 Linux the output is `kernel-output/Image`. To use it on an Apple
Silicon Mac, copy that file to the Mac and run
`./target/release/seedbox run examples/etcd --kernel ./Image`.
The kernel build targets require a Linux builder; they do not run natively on
macOS. No Seedbox GitHub credentials or release access are needed for this path.
The first source build still fetches public Linux source and Nix dependencies;
fully offline builds need those inputs already present.

The guest helper archive is released separately. Without private release access,
also build `nix build .#runtimeArchive` and pass its output through
`--runtime-assets`; see [the complete manual workflow](../tools/runtime/README.md#build-without-private-release-access).
