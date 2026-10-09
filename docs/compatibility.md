# Draft formats and tested runtime scope

These are the current readers and writers, not a frozen release contract.
No migration, future patch-series compatibility or cross-version execution
guarantee is promised here.

With the same retained engine, seed, exact retained inputs and compatible
execution profile, semantic execution evidence and the application outcome must
reproduce; a run's witness and `seedbox replay` MATCH are how that is checked.
Performance settings must not change results. A different engine or input may
execute differently. Same-seed equality across hosts is not established.

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
additionally requires verified input bytes and a host with the capabilities the
recorded profile needs (KVM or HVF access). Replay runs the engine retained in the run directory;
records are never rewritten. Host admission checks capabilities only: #142 and
#143 removed the engine, kernel, CPU and host-policy pins, so older records'
extra admission parameters are kept as data and not enforced. A readable v2
record is distinct from a rejected v1 record. Draft formats remain undecided.

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

## Guest runtime facilities

The shared supervisor gives each service VM a private tmpfs `/dev` containing
`null`, `zero`, `full`, `random`, `urandom`, and `tty`. `/dev/shm` is a 64 MiB
tmpfs with mode 1777; `/dev/pts` is a private devpts mount with `/dev/ptmx`
pointing to `pts/ptmx`. `/dev/fd` and `/dev/stdin`, `/dev/stdout`, `/dev/stderr`
point into `/proc/self/fd`. The app inherits console output; this does not
implement Compose `tty:`. Bootstrap disk and management device nodes are absent
from the app's `/dev`; the supervisor opens its management channel and mounts
named volumes before installing this layout.

The hostname and `/etc/hostname` are the Compose service name, and `/etc/hosts`
contains the service's assigned address alongside its peers. Before every exec,
including process restart, the supervisor sets both `RLIMIT_NOFILE` limits to
1048576 before dropping image credentials. `vm.max_map_count` defaults to
262144 and is recorded alongside the hostname in the retained `SBXLAUNCH3`
launch recipe. Compose `hostname:`, `ulimits:`, and `sysctls:` overrides remain
unsupported. `memlock` keeps its kernel default.

Both application kernels enable SysV IPC, POSIX message queues, UTS/IPC/PID/net
namespaces, UNIX98 ptys, seccomp and seccomp filters. **io_uring stays enabled**;
the old x86 `n IO_URING` request never survived Kconfig resolution without
`EXPERT`, so removing it preserves the existing io_uring behavior. The equally
ineffective `n HPET_TIMER` request is removed too. These features do not imply
complete Chromium application support. Network feature alignment and less
common syscalls remain separate work.

Each VM exposes one CPU. Seastar applications such as ScyllaDB and Redpanda
need `--smp 1`; DPDK must select only guest CPU 0. JVM GC, tokio worker counts
and Go parallelism reflect that CPU count. Real SMP remains out of scope.
The shared supervisor now applies a recorded wall-clock epoch, derived from the
prepared images or set by `x-seedbox.wall_clock_epoch`, on both architectures. User namespaces are enabled in both application configs.
CPU feature baselines and cross-host replay remain unproven.

`seedbox run harness/acceptance/workloads/runtime.yaml` exercises the actual
interfaces as a non-root image user: devices, ptys, POSIX and SysV IPC,
hostname resolution, limits, the sysctl, a named volume, an io_uring NOP and a
seccomp allow filter, plus unprivileged UID/GID mapping and nested namespaces.
Replay its retained run to check `MATCH`. This is a
focused interface smoke test; application acceptance remains version-bound.

The `runtime-postgres.yaml`, `runtime-nginx.yaml`, `runtime-python.yaml`,
`runtime-jvm.yaml` and `runtime-node.yaml` projects in the same directory run
initdb plus a SQL query, an HTTP request through nginx's ordinary stdout log,
a Python process pool, JVM local hostname lookup and Node HTTP, respectively.
Those projects set `x-seedbox.machine.memory_mb: 2048` to accommodate their image archives and
application heaps. The io_uring probe checks a real ring completion separately;
a Node HTTP pass alone does not establish which libuv path was used.

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
| Other runtimes/platforms | MinIO requires [#48](https://github.com/gardnervickers/seedbox/issues/48) acceptance. Arbitrary C/Python/Go programs, JVM/JIT and io_uring remain untested. Intel, second-Zen replay, Mac TCG production and cloud hosts remain unproven; [TCG evidence](backends.md#capability-limits) is not KVM evidence. |

Keep image/kernel/engine hashes and profile details with each acceptance record.
The remaining V0 release gate includes reader/writer decisions, reproducible
packaging, final profile/runtime acceptance and independently reviewed fixtures.
