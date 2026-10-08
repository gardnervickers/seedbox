# Native Mac runtime build

From an Apple Silicon checkout:

```sh
nix develop
bash tools/mac-runtime/build-cli.sh
target/release/seedbox doctor
target/release/seedbox run PATH_TO_PROJECT
target/release/seedbox replay .seedbox/runs/ATTEMPT
```

The maintainer build owns a temporary ARM Linux Lima VM. It builds the audited
application kernel, static guest helpers and Linux preparation worker, exports
the Nix runtime closure for the existing Skopeo, umoci and Podman tools, embeds
that payload into the CLI, and signs the CLI with the HVF entitlement. Its exit
trap stops and deletes the build VM. The preparation closure contains no Rust
compiler. The current unoptimized packaging size is roughly 250 MiB; release
distribution and payload-size optimization are separate work.
The exported build source carries the host checkout's commit and dirty marker,
so the Linux preparation helper keeps its provenance without copying `.git`.

At runtime, the CLI verifies and caches its embedded payload. Preparation uses
a private, marked Lima directory and an exclusive lock. It copies only the
requested inputs into that owned Linux filesystem, verifies returned content
hashes, and stops the preparation VM before executing the world. Application
machines run directly under macOS HVF, one worker process and one vCPU per
service. Runtime commands never invoke Cargo and need no separate VMM path.
The host Lima executable and its resources remain an installed dependency,
provided by the project Nix development shell. The CLI embeds the guest inputs
and Linux preparation closure; this maintainer workflow does not distribute
Lima itself. The first preparation VM start downloads the pinned Ubuntu image
and installs the pinned Nix bootstrap described in `lima.yaml`; initial setup
requires network access. Replay needs neither Lima nor those downloads.

Podman uses a separate VFS storage directory for each preparation job. This
avoids reusing overlay mount state across VM stops; cleanup removes that job's
build storage. The private VM's standard image policy accepts unsigned application
and base images. Neither setting changes the host's container configuration.

Replay requires the retained signed executable, guest archives and kernel,
plus the matching HVF host proof. It does not start the preparation VM, pull
images or rebuild anything. Updating the installed CLI does not replace the
retained executable. Performance options stay outside semantic identity.

The native application profile uses ARM PV time and a versioned world report.
It records boot layout, seeded entropy/timer policy, devices, kernel and exact
executable bytes. Full-state evidence covers RAM, CPU/SIMD/GIC, PV clock,
UART and application devices. This profile cannot be substituted for the older
ARM fixture or the qualified x86 KVM profile. Mac execution remains a development
engine; matching evidence does not establish
cross-host qualification.

Images must provide Linux ARM64 releases. Mac parsing defaults to
`linux/arm64` and rejects `linux/amd64`; no guest instruction emulation is used.
Supported RAM is 64..3072 MiB per service. CPU pinning and unqualified PMU flags
are Linux-only, and Mac exploration currently supports one worker. Kernel,
supervisor, BusyBox and mke2fs overrides remain available for development.

Mac application worlds use the shared coordinator for connection discovery,
selectors and committed console triggers. Exercise the guest control API with:

```sh
target/release/seedbox run harness/acceptance/workloads/mac-capabilities.yaml
target/release/seedbox run harness/acceptance/workloads/triggers.yaml
target/release/seedbox replay .seedbox/runs/ATTEMPT
```

The capabilities scenario checks connection discovery, invalid-selector
rejection, trigger arm/cancel and ordinary partition/heal operations. The trigger
scenario checks committed matches and their scheduled effects. Arbitrary ARM
snapshot/fork reuse remains unsupported. Timed log chunks and per-page RAM
diagnostics are Linux features; Mac retains raw streams and whole-state evidence.
