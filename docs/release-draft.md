# Draft pinned runtime artifacts (#65)

`packages.x86_64-linux.releaseDraft` describes a candidate CLI, standalone VMM,
patched guest kernel, embedded/bootstrap supervisor and static BusyBox. It is a
source-reviewed draft, not a qualified engine or published release. It targets
only the current NixOS x86-64 AMD Zen host family; it makes no generic Linux,
installation, reproducibility or runtime acceptance claim.

## Recipe and identity

The flake lock pins Nixpkgs. The recipe uses two independent Cargo vendor closures
and both checked-in lockfiles. The CLI runs `cargo build --release --locked
--offline`; the VMM still runs the existing `cargo build-vmm` alias from the
repository root with its existing release profile and standalone target directory.
It does not unify features, add a static Rust target, strip, audit-inject or patch
ELF outputs. The static musl supervisor compiler comes from the project Nix pin.

A Linux Nix sandbox must unpack into `/build/seedbox`; the recipe refuses a
different source root. HOME, Cargo home and temporary paths are fixed. Rust paths
remap `/build` to `/seedbox-build`; `env!(CARGO_MANIFEST_DIR)` still embeds the
canonical build path. This is why arbitrary checkout directories plus Rust path
remapping alone are insufficient. The supervisor build script invokes the compiler
directly, so generic `CFLAGS` are not claimed to remap it: its input/output paths
are canonical and its existing flags stay unchanged.

The kernel uses the existing pinned tarball/hash, patch/config scripts and build
script. Each derivation gets a fresh source/output tree. Build version is 1,
LOCALVERSION is empty, architecture is x86, epoch is 0, user/host are seedbox and
the timestamp is the existing epoch timestamp. Kernel C, assembly and host-tool
path maps are explicit. Compiler/binutils dependencies come from the flake pin.
Both Rust builds and the kernel use two build jobs.

Packaging retains raw binaries and a runtime Nix closure (including GNU loader
and shared libraries). It does not substitute musl for the native GNU VMM/CLI.
`payload/provenance.json` records source commit, source/store/toolchain paths,
lockfile hashes, native/bootstrap targets and recipe parameters.
`SHA256SUMS.json` records every regular payload/closure file; the archive digest
also covers modes and symlinks. Members are sorted, mtime/uid/gid are zero,
user/group names are empty and gzip has no filename or wall timestamp. Raw build
logs and elapsed/PID/host observations belong in separate receipts.

A changed VMM binary from canonical paths/remapping/toolchain is a **new engine**
even when its source, lockfile and profile are unchanged. A changed supervisor or
kernel changes retained inputs. None replace the accepted `168ad5d…` engine or
`e8ece7c…` kernel. Existing admission policy does not qualify this candidate and
must not be silently repinned. Repacking unchanged payload bytes changes only the
archive identity. Old witnesses/seeds cannot qualify a new engine.

## Source-only controls

Run inside the project development shell:

```sh
python3 tools/release/test_package.py
nix-instantiate --parse nix/release/default.nix >/dev/null
```

The portable controls use tiny inert files. They vary clean paths, creation order
and mtimes; require equal archives; exercise retained executable permissions,
symlinks/hardlinks and checksum readability; and reject missing bootstrap,
non-immutable provenance, invalid closure roots, special files and unsafe output
paths. They build or execute no VMM, kernel, OCI image or application. Required
repository source checks remain separate.

## Future bounded proof (requires its own build gate)

Pin a clean Git commit and retain its flake metadata and evaluated derivation
paths. On the qualified workstation, build only the CLI and VMM derivations
first, `--max-jobs 1 --cores 2`, with substitutions/cache reuse explicitly
accounted for. Two independently fresh sandbox executions of the same recipe
must actually compile; two result links or a shared Cargo target do not count.
Use a forced rebuild with retained logs if appropriate. Keep the original first
outputs/receipts before the second execution, and compare raw CLI, VMM and
supervisor with SHA-256 and `cmp`. Inspect ELF interpreter, RUNPATH, dependencies
and residual build paths. Proposed first budget is 20 elapsed minutes; stop on
any mismatch without retrying or changing pins.

Only after that gate, authorize two fresh kernel derivations separately
(proposed 40 elapsed minutes, two jobs, sequential builds). Compare `bzImage` and
resolved `kernel.config` byte-for-byte. Then independently reconstruct the archive
from each raw output/closure, compare it, and retain archive and file checksums.
Evaluation or portable packaging controls are not a reproducibility proof.

The artifact contains `payload/`, `nix/store/`, Nix registration and closure paths.
No installer is supplied. A later delivery decision must choose a supported Nix
closure import/copy procedure or preinstalled exact Nix dependencies; ordinary
archive extraction is not claimed to register or safely install a Nix closure.
Installing into `/nix/store`, downloading/executing binaries, publishing assets
and qualifying a fresh engine each require their own reviewed plan. The later
runtime matrix should exercise public prepare/run/explore/export/import/replay
with the new closure and guest-owned checks, including planted FAIL then replay
MATCH, before calling downloadable delivery complete.
