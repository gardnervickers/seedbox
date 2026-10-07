# One-binary simulator identity

This is the implementation contract for the [one-binary plan](https://claude.ai/artifact/15qtCGTgwQrjY2nu6t1UW9).
It describes the intended replacement for the separate VMM executable. The
current runtime still uses the artifact and admission contracts documented in
[compatibility.md](compatibility.md); this document alone changes no format,
release pin, or supported host.

## Version and retained artifacts

The Seedbox version is the public simulator identity. There is no separately
versioned engine. A published release version names one immutable executable
for each supported host OS and architecture. A rebuild with different bytes
must not replace an existing release artifact.

A development executable reports `VERSION-dev+COMMIT`, with a `.dirty` suffix
for local modifications. These labels are provenance, not unique artifact
keys: two dirty builds may have the same label. Source archives without verified
release provenance are development builds too. A tag or a version string alone
does not qualify a build.

The content store retains the exact executable, identified internally by its
SHA-256 digest. Runs also retain their actual kernel, supervisor, guest images,
and prepared filesystems. Embedded inputs are materialized and verified through
the same store; an explicit development kernel override is retained separately.
Changing the executable or an input creates a distinct captured run even if
its readable development version has not changed.

Release qualification associates a version, host platform, backend, timebase,
and guest ABI with the tested release artifacts and host requirements. Artifact
verification still checks the published bytes; there is no additional public
engine fingerprint. Unknown or development builds require explicit development
admission and are not release qualification evidence. Qualification cannot be
obtained by editing a version string.

## Recorded execution contract

The new record identifies:

- Simulator version and whether the build is a release or development build.
- Host OS and architecture, guest architecture, backend and timebase.
- Executable kind: legacy standalone VMM or Seedbox internal worker.
- Exact executable, kernel, supervisor and other input digests.
- Semantic configuration, seed, guest ABI and admitted host policy.

The executable kind selects the launch protocol explicitly. It is not inferred
from the filename or by executing an unknown artifact to inspect its version.
Legacy records lacking this field retain their legacy interpretation.

The existing format has strict readers and checksums over typed serialization.
Adding an optional field does not make a new record readable by an old reader.
The implementation must explicitly version changed formats, keep an unchanged
legacy decoding/checksum path, and test the frozen fixtures before enabling a
new writer. It must not deserialize an old record into a new shape and then
silently recompute its identity or admission proof.

## Replay

Replay launches the verified retained executable. A Seedbox worker receives the
internal `__engine` command before normal CLI initialization. It never searches
for a VMM, invokes Cargo, prepares images, or inherits ambient `SEEDBOX_*`
configuration. Legacy VMMs continue to receive their original argument protocol.
The owning CLI preserves cancellation, process-group cleanup and the ordering
that stops workers before releasing host state such as MSR leases.

Initially replay requires the recorded host OS, architecture, backend and
timebase, plus a compatible admitted host. Platform mismatch is NOT_COMPARABLE
before execution and names the recorded and current platforms. Equal versions
or traces do not establish cross-platform replay or full-state equality. A new
cross-platform comparison mode needs separate qualification and must not change
the interpretation of exact replay.

Historical records either replay through their verified legacy path or fail
with an explicit NOT_COMPARABLE reason. Missing host proof is not synthesized.
An upgrade retains old executable artifacts while retained runs reference them.
Garbage collection may remove only unreferenced artifacts.

## Platform selection and admission

The executable includes only backends supported by its build target. Expected
initial combinations are x86-64 Linux/KVM, ARM64 Linux/KVM and ARM64 macOS/HVF.
Their availability in source is distinct from supported execution on a host.
Image selection follows the selected guest architecture, never the build host
implicitly. An incompatible image fails before extraction or guest execution.

The counted x86 profile keeps its existing CPU, perf, MSR and host-policy gates.
A future PV profile can omit mechanisms it does not use, but writable `/dev/kvm`
alone is not sufficient admission. Kernel capabilities, CPU feature exposure,
guest ABI and actual repeatability still require validation. Broad Linux support
must not be inferred from removing perf/MSR use.

HVF admission includes the required OS APIs and executable entitlement. A
retained copy must preserve its signed bytes. Multiple ARM64 machines require
an explicit design for HVF's per-process VM constraint: an internal worker
entry point alone does not establish support for a multi-machine world. The
world scheduler must own machine progress and deterministic inter-worker
ordering if each machine needs a separate process.

## Acceptance obligations

Every implementation step preserves the CLI status-line and exit-code contract,
admission before preparation, and `doctor` as a probe that creates no run or
prepared image. A successful probe is not a replay qualification claim.

Moving the VMM into a library changes executable bytes. For a refactor intended
to preserve semantics, compare execution witnesses against the separate VMM on
the affected fixtures with identical inputs. Performance parameters never
change results. Repeatability within each ARM64 backend and equivalence between
backends are separate checks; matching exit traces do not imply matching RAM.

The x86 worker integration and embedded release kernel wait for the plan's K2
merge and re-pin. Architecture-aware OCI work can proceed independently. The
ARM64 library's [configurable boot layout and clock horizon](arm64-backends.md#application-groundwork)
are also independent groundwork; neither supplies a multi-machine executor.
The final user acceptance is a fresh build, ordinary Compose run, replay MATCH,
and replay of that retained run after a newer Seedbox build, without manually
configured binary or kernel paths.

## Image inputs

Seedbox consumes finished images. It does not build Dockerfiles or manage a
builder VM. Registry references, OCI layouts/archives, Docker-save archives and
finished local Docker/Podman images converge through verified native import.
Compose may retain `build:` for external tooling; Seedbox uses the declared
`image:` and never invokes a build. A build-only service receives a setup error
explaining how to build externally and supply an image.

Registry tags resolve to exact retained manifests, configs and layers. Archive
inputs and exported local images receive the same digest and platform checks.
Image selection follows the guest architecture. A Mac ARM64 run requires an
ARM64 or multi-platform image, without silently using x86 emulation. Replay uses
the captured closure and needs no registry, container daemon or original source.

The importer applies layers to a virtual Linux filesystem and writes that
metadata directly into the guest archive. It does not unpack image paths onto
the host, require root, or remap Linux ownership to Mac user IDs. Unsupported
filesystem metadata fails explicitly instead of being discarded.

## Existing dependencies

Checked on 2026-10-07: [ARM64 milestone #120](https://github.com/gardnervickers/seedbox/pull/120)
is merged and supplies the isolated HVF/KVM machine backend. It does not yet
implement Compose networking or multi-machine worlds. The
[Nix artifact recipe #72](https://github.com/gardnervickers/seedbox/pull/72),
[PV engine #85](https://github.com/gardnervickers/seedbox/pull/85),
[PV CLI #89](https://github.com/gardnervickers/seedbox/pull/89),
[K2 kernel fix #114](https://github.com/gardnervickers/seedbox/pull/114), and
[K2 integration #126](https://github.com/gardnervickers/seedbox/pull/126) remain
open drafts. Directed-link policy [#97](https://github.com/gardnervickers/seedbox/pull/97)
is merged and included in this branch. The remaining CLI work in
[#105](https://github.com/gardnervickers/seedbox/pull/105),
[#106](https://github.com/gardnervickers/seedbox/pull/106) →
[#109](https://github.com/gardnervickers/seedbox/pull/109) is also mid-merge.
Refresh the integration base after those changes land. Reuse their reviewed
work rather than implementing competing recipes, timebases or kernel fixes.
Native image import remains independent.
