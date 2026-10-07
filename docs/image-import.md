# Native completed-image import

The native importer is the image-preparation foundation for the
[one-binary workflow](identity.md). It is available through the library, with
portable tests on Mac. The CLI still uses the existing legacy preparer pending
integration with the active CLI merge stacks. No run format, release pin,
supported execution host or default CLI behavior changes in this step.

`oci::native::NativeOci` accepts an explicit `ImageInput` and `GuestPlatform`:

| Input | Behavior |
| --- | --- |
| Registry reference | Pull a tag or SHA-256 digest over verified HTTPS; select the requested Linux platform from an OCI/Docker index. |
| OCI layout | Read its index and verified blob closure; optionally select `org.opencontainers.image.ref.name`. |
| OCI archive | Read a plain, gzip or zstd tar containing a layout; apply the same index and descriptor checks. |
| Docker-save archive | Select a single matching image or supplied exact tag; construct a deterministic OCI envelope for legacy saved config/layer bytes. New exports containing an OCI layout preserve its original descriptors. |
| Local Docker/Podman image | Export the supplied existing reference with `image save`, then import its captured archive. No build, pull, retag or removal command is issued. |

Registry references support ordinary Docker Hub shorthand, explicit registries,
tags and `@sha256:` pins. Anonymous bearer authentication is supported. Private
images can be supplied through an authenticated Docker/Podman export or an
archive; native credential discovery is not yet implemented. Registry bearer
tokens are kept in memory and are never forwarded to a different redirect origin.
Ordinary references use TLS; an explicitly supplied loopback HTTP endpoint is
available to library callers and offline protocol tests.

Platform selection is explicit: `LinuxAmd64` or `LinuxArm64`. Index descriptors
and actual image configurations are checked, including indexes whose descriptor
platform is absent. Baseline AMD64 `v1` and ARM64 `v8` variants are accepted.
Different matching manifests require explicit selection. Selection does not
qualify a CPU, guest kernel or execution backend.

The content store retains exact platform manifest, config and compressed layer
bytes. Each descriptor's size and SHA-256 must match; cached bytes are verified,
and an existing cache entry does not replace checking a new source. Every layer
also matches the configuration's uncompressed `rootfs.diff_ids` before its
filesystem changes are applied. A legacy Docker-save envelope is derived from
captured bytes, rather than represented as an original registry manifest.

Layers become a virtual Linux tree. Linux UID/GID, modes, whole-second mtime,
regular hardlinks and symlinks go directly into the guest archive. File payloads
use private numbered spools, so case sensitivity and Linux ownership do not
depend on the host filesystem. Whiteouts affect lower layers only, including
opaque whiteouts encountered after new entries. Symlink parent resolution occurs
inside the guest namespace. The final tree requires directory parents before
CPIO emission; image paths are never unpacked onto the host.

The current guest archive cannot encode extended attributes, ACLs, capabilities,
special files or sparse files. These fail explicitly. Fractional mtimes are
represented as whole seconds, matching the existing newc path. Global PAX
metadata is unsupported. Metadata extensions are capped at 1 MiB before tar
parsing can allocate their payloads; JSON metadata is capped at 8 MiB before
parsing, and metadata descriptors are checked before downloading. An individual
blob/uncompressed layer is
capped at 16 GiB, a source archive at 64 GiB, and entries at one million. Linux
pathname, component and symlink-length limits are checked. Transfers, verification,
materialization and packaging check cancellation while streaming. Network calls
also have bounded connect/read timeouts.

`config::parse_native` / `load_native` load Compose for the caller's explicit
`GuestPlatform`, which is also the default for omitted service `platform:`.
An explicit incompatible platform fails authoring validation. When a service
declares both `image:` and `build:`, only the image affects Seedbox; external
build paths, options and variables are left uninterpreted. A build-only service
fails with an explanation to build externally and supply `image:`. The existing
`config::parse` / `load` APIs and CLI retain their legacy authoring behavior.

`preparation::native::NativePreparer` implements `PreparationBackend`. Its runtime
inputs use the existing admitted profile and captured helper/kernel/executable
bytes; service overrides use owned `NativeSource` values. It checks runtime
admission and architecture before image transport, preserves config/reserved
path rules and memory floors, and refuses `ImageSource::Build` even with an
override. Guest-helper checks establish ELF architecture, not static linking or
runtime compatibility. CLI integration and an application-capable ARM64 kernel
and world executor remain separate work.

Portable acceptance covers compression and both archive families, exact retained
bytes, platform selection and authoring, layering and Linux metadata, hostile archives,
cancellation, authenticated loopback registry routing, and captured inputs that
remain stable while an exporter replaces their original files. Local-client tests
use simulated exporters; these checks do not establish guest boot, live daemon
support or general external registry interoperability.

An opt-in public-registry smoke passed on Apple Silicon macOS on 2026-10-07:
BusyBox index `sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e`
selected ARM64 manifest `sha256:d82c2ab94640ded77cf76514ce6a84870761105058a4a9e51b05a8a79be97a6c`.
The test imported its verified config and compressed layer, then packaged 442
image entries. This exercised anonymous HTTPS, bearer authentication and the
public blob transport; it did not boot the image. To repeat that import:

```sh
SEEDBOX_PUBLIC_IMAGE=docker.io/library/busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e \
  nix develop --command cargo test -p seedbox --locked --test native_oci \
  public_registry_pinned_image_smoke -- --ignored --exact --nocapture
```

This test defaults to `linux/arm64`; `SEEDBOX_PUBLIC_IMAGE_PLATFORM=linux/amd64`
selects the other supported image platform. These variables configure only the
opt-in test, not the library or CLI.
