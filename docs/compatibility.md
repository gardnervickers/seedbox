# Draft formats and tested runtime scope

These are the current readers and writers, not a frozen V0 release contract.
Before tagging V0, qualify the final release artifacts/profile and declare the
supported format versions. No migration, future patch-series compatibility or
cross-version execution guarantee is promised here.

With the same Seedbox/application software versions, seed, exact retained inputs
and qualified compatible execution profile, semantic execution evidence and the
application outcome must reproduce. Performance settings must not change results.
A different software version or input may execute differently. Format readability
and possession of engine bytes do not qualify another host or backend.

## Current reader and writer behavior

| Surface | Current behavior |
| --- | --- |
| Compose | `x-seedbox.version: 1`; the [supported subset](../crates/seedbox/README.md#supported-authoring-subset) rejects unknown versions and fields. |
| Run manifest | Draft `identity.version: 2`; checksummed metadata and identity. Version 1 is rejected, not migrated. Fixed metadata structures reject unknown fields; defined dictionaries and opaque bytes are not fixed-field schemas. |
| `inspect --json` | One pretty JSON `RunRecord`, without the manifest checksum envelope. Its version is nested at `identity.version`; there is no separate inspect JSON version. Field/status spelling comes from the library's serde types. Whitespace and key order are not a script contract. |
| Bundle | Container version 1 carries the existing v2 manifest and complete declared object closure. Unknown versions/metadata fields and corrupt, missing, unsafe or extra entries are rejected. Import preserves original manifest and retained opaque bytes, without migration, installation or execution. See [bounds and omissions](runtime.md#bundles). |
| Exploration checkpoint | Draft version 2 records bounded worker capacity, stable seed/index dispatch order and completed/unfinished attribution. Unsupported versions are rejected without migration. Version 1 batch bytes require the prior reader; ordinary run manifests/identity and retained replay are unchanged. |
| Guest RPC | [Public version 1](../python/PROTOCOL.md); operation arguments are strict. The Python binding checks required response fields, version, correlation and timing, and tolerates unrelated response detail keys. New operations are not automatically supported by an older peer. Private relay framing is not a public binding contract. |

An added structural field is not automatically compatible with a strict older
reader, even if optional. Checksums also depend on typed serialization. A new
writer/reader or field meaning needs an explicit version decision and retained
fixture checks; no reader silently normalizes or repairs an old record. SHA-256
checks detect corruption, not authenticity against someone rewriting the store.

Inspection and transfer of an accepted format need no guest execution. Execution
additionally requires verified input bytes and a matching qualified profile.
[Issue #5](https://github.com/gardnervickers/seedbox/issues/5) and
[PR #55](https://github.com/gardnervickers/seedbox/pull/55) implement versioned
host-proof admission in this source. Historical v2 records without that proof
remain readable and transferable, but execution through this CLI retains
ERROR/NOT_COMPARABLE (exit 2) before host probing or guest execution. An engine-change
override cannot bypass missing proof. Use the retained previous CLI only for
historical execution evidence; records are never rewritten to obtain admission.
This implementation has source-only checks: Linux collection and the new profile
are not yet qualified. A readable historical v2 record is distinct from a
rejected v1 record. Draft formats and release compatibility remain undecided.

`diff --json` emits a library `DiffReport` for two retained directories. It does
not change manifest or bundle formats. See [retained comparison](runtime.md#compare-retained-runs)
for evidence scope and missing-data semantics.

## Exit meanings and user data

`run` exits: 0 PASS, 1 FAIL, 2 ERROR/usage/setup, 3 INCOMPLETE and 124 simulated
TIMEOUT. `replay` exits: 0 MATCH (including reproduced FAIL), 1 MISMATCH and
2 NOT_COMPARABLE. Offline `diff` exits 0 MATCH, 1 DIFFERENT, and 2 for
usage/setup or invalid evidence. Application outcome and replay equality are separate. Wall-time
expiry/cancellation are INCOMPLETE, not simulated TIMEOUT. Successful inspect,
export and import return 0 even for a recorded FAIL; success means the command
completed, not that execution is admitted. Their validation/setup errors return 2.

Application workload, fault-injector, verifier and optional file bytes remain
user-owned and opaque. The configured check service's expected exit status carries
the verdict when completion and required evidence exist. No JSON/log/signature
schema or failure grouping is required. Attempts are not unique bug groups.
Full-state comparisons are identified separately; `null` supplies no full-state
proof. Machine digests alone are not complete network/world state.

## Placement and evidence limits

Production OCI preparation uses one service per VM/chroot. The library's placement
map can represent custom co-location, but its fixture coverage and historical
shared-kernel experiments do not expose another supported Compose placement mode.
Writable application storage is private RAM-backed state per fresh run/boot;
process restart retains it. This is not machine crash or durable block storage.

| Case | Evidence and boundary |
| --- | --- |
| AMD Ryzen 5950X Linux/KVM | [PR40 Stage A](https://github.com/gardnervickers/seedbox/pull/40#issuecomment-6030143197) and [#41 application audit](https://github.com/gardnervickers/seedbox/issues/41#issuecomment-6030374337) qualify their exact retained cases/profile, not the outstanding #33/#36 campaign or new host admission. |
| Redis 7.2/Rust checker and etcd 3.5.2/Go checker | [Initial acceptance](acceptance.md) is version-bound to the recorded images/engine. It does not qualify arbitrary Rust, Go or OCI applications. |
| Python faults and native/race fixtures | #41 and [PR39](https://github.com/gardnervickers/seedbox/pull/39#issuecomment-6029205818) retain actual replay evidence; PR39 uses an earlier engine. [PR53](https://github.com/gardnervickers/seedbox/pull/53) changed fault-workload bytes and needs fresh guest qualification. |
| Bundle inspection/transfer and offline replay | [#42](https://github.com/gardnervickers/seedbox/issues/42) includes portable byte-preservation controls and actual same-host offline replay. Transfer portability does not establish cross-host execution equality. |
| Other runtimes/platforms | MinIO requires [#48](https://github.com/gardnervickers/seedbox/issues/48) acceptance. Arbitrary C/Python/Go programs, JVM/JIT and io_uring remain unqualified. Intel, second-Zen replay, Mac TCG production and cloud qualification remain deferred/unproven; [TCG evidence](backends.md#qualification-and-capability-limits) is not KVM evidence. |

Keep image/kernel/engine hashes and profile details with each acceptance record.
The remaining V0 release gate includes reader/writer decisions, reproducible
packaging, final profile/runtime acceptance and independently reviewed fixtures.
