# Linux guest assets in the CLI

On an x86-64 Linux build host, from the checkout:

```sh
nix develop
bash tools/linux-runtime/build-cli.sh
sudo -E env PATH="$PATH" target/release/seedbox doctor
sudo -E env PATH="$PATH" target/release/seedbox run PROJECT
sudo -E env PATH="$PATH" target/release/seedbox replay .seedbox/runs/ATTEMPT
```

The maintainer script invokes the existing pinned kernel and static supervisor
builders, copies the development shell's static BusyBox/mke2fs, and embeds the
resulting `artifacts/linux-runtime/runtime.tar.gz` in the release executable.
The platform VMM is already linked into that executable. Runtime commands do
not build, download or locate another VMM or guest helper. Skopeo, umoci and a
Docker-compatible image builder remain installed host dependencies, provided by
the development shell. Root/KVM access and the qualified host policy still apply.
This is a maintainer build workflow; installer/distribution work is separate.

The optional build input `SEEDBOX_LINUX_RUNTIME_BUNDLE` selects the archive for
Cargo. Set/change/unset and same-path archive replacement invalidate embedding.
Ordinary Cargo builds without it keep the development input search and optional
musl-compiled supervisor. The standalone VMM aliases and their lockfile,
compiler wrapper and reproducibility contract are unchanged.

The package has exactly seven regular USTAR members: `assets.json`, `bzImage`,
`compose-init`, `busybox`, `mke2fs`, `NOTICE.txt`, and `provenance.json`. Its versioned
manifest binds roles, lengths, SHA-256 digests, the host CLI platform and guest
architecture. All three executable helpers run in the guest; mke2fs is embedded
in the initramfs for named volumes. Actual upstream license texts come from the
pinned source archives, and provenance binds those archives, source revision,
recipe inputs and resulting asset hashes. Missing upstream notices fail the
maintainer build. This provenance records inputs; it does not itself prove
independent build reproducibility or host qualification.

The reader checks bounded compressed/decompressed sizes, exact names, duplicate
entries/metadata keys, archive termination, roles, digests, executable headers
and static linkage before returning an asset handle. Every installation owns a
private directory created with mode 0700 beneath the caller's scratch directory;
there is no persistent guest-asset cache. Helpers get mode 0755 and the kernel
0600. The owner remains alive until preparation has retained consumed bytes,
then removes the directory on success, error or cooperative cancellation.

Explicit runtime flags and their environment path overrides win. For the kernel,
existing configured `SEEDBOX_ARTIFACTS`/`SEEDBOX_WORK` locations precede packaged
bytes; the package precedes incidental checkout/home searches. A missing configured
directory preserves the old search behavior, while a missing explicit path fails.
For static helpers, the package precedes the existing embedded-supervisor/PATH
fallbacks. Fully explicit inputs need no package. Doctor shares kernel selection,
validates a selected package and extracts only its kernel without creating state,
preparing images or running packaged helpers. Replay uses retained inputs only;
an installed package never repairs missing or corrupt retained bytes.

Portable Mac tests cover archive/binary parsing with synthetic bytes, Python
package output consumed by the Rust reader, override precedence, ownership and
Cargo embedding invalidation. Synthetic executables are never run. Actual Linux
package build, relocation outside the checkout, doctor/run/replay without guest
asset overrides, named-volume recovery, retained replay after an upgrade and
VMM/CLI reproducibility remain final merge-train checks. This package supports
`x86_64-linux` only; it does not add an ARM Linux packaging path.
