# Seedbox simulation example

This project is self-contained. Use the same commands on Linux x86-64 KVM or
Apple Silicon HVF after building the packaged CLI for that platform. Linux
execution requires root and KVM access; Mac execution uses the current user.
The etcd and Redis templates passed real workloads,
intentional failure controls, and exact replays on the supported Linux amd64 AMD Zen
workstation with the controlled Seedbox kernel and 1024 MiB RAM per service.
See the repository's `docs/acceptance.md` for the tested image/kernel hashes.
This establishes those image versions, not compatibility with arbitrary OCI images.

Production preparation uses one service per VM, with private RAM-backed storage
retained across process restart only. The repository's `docs/compatibility.md`
describes current draft formats and version-bound runtime evidence; V0 release
compatibility and host-proof admission remain qualification gates.

The etcd template packages Seedbox's existing Go acknowledgement-ledger workload
in a Dockerfile and describes three registry-image members in Compose. The Redis
template builds a dependency-free Rust workload that checks PING, SET acknowledgements,
and exact GET values. Both use the Rust Seedbox configuration/orchestration APIs.

From this directory, with the packaged `seedbox` executable on `PATH`:

```sh
seedbox validate
seedbox doctor
seedbox run
seedbox inspect .seedbox/runs/ATTEMPT --json
seedbox logs .seedbox/runs/ATTEMPT --machine client --stream console
seedbox replay .seedbox/runs/ATTEMPT
seedbox diff .seedbox/runs/ATTEMPT .seedbox/runs/REPLAY
seedbox explore --count 2
```

The Linux maintainer build `bash tools/linux-runtime/build-cli.sh` from the
Seedbox checkout includes the guest kernel, supervisor, BusyBox and mke2fs in
one executable. Explicit runtime-input flags and their environment path overrides
still win. Configured artifact directories precede the packaged kernel;
packaged assets precede incidental checkout/home/PATH defaults. Skopeo, umoci
and the image builder remain installed preparation tools. Unbundled developer
builds keep the existing input search. Package execution on Linux remains a
final merge-train check. Replay uses only retained inputs after an upgrade.

On a VMM failure, text output from `run`, `replay`, `explore` and `inspect`
prints up to five non-empty lines from the final 8 KiB of the attempt's
`output/vmm.stderr` to stderr, prefixed with `vmm stderr:`. Missing or unreadable
logs do not change the outcome; retained records and `--json` output are unchanged.
`inspect` keeps the recorded `error:` line on stdout alongside the run status.

On Linux, preparation unpacks images with root ownership and runs before the execution CPU
lease. Admission briefly checks an eligible core first and releases all temporary
resources before preparation.
`run` picks a free CPU, finds the guest kernel and uses its linked VMM; see
`seedbox help run`. Run/replay must be invoked with the required KVM/MSR access;
see the repository's `docs/runtime.md` for setup and limits. `--margin` and
`--undo` control PMU approach and stepping performance without changing results.
`seedbox --version` reports the simulator version. New runs retain the whole
executable for replay, so an installed CLI upgrade does not replace the recorded
engine. No separate VMM build or runtime compiler invocation is needed. Every
built engine and guest kernel is admitted; `--allow-dev-engine` remains a
compatibility no-op. Native replay evidence remains separate.
Apple Silicon builds also support native ARM Compose execution through the shared
world coordinator, including connection selectors and committed console triggers.
Arbitrary ARM snapshot/fork reuse remains unsupported. Build once with
`nix develop` and `bash tools/mac-runtime/build-cli.sh` from the Seedbox checkout.
The signed CLI packages the ARM kernel and static helpers and automatically owns
a Linux VM for preparation with Skopeo/umoci/Podman. That VM stops before native
HVF execution; replay uses only retained bytes and never starts it. No runtime
compiler or extra VMM path is needed.
Images must support `linux/arm64`, RAM is limited to 3072 MiB per guest, and
exploration is serial. CPU/PMU flags apply to Linux. Mac profiles have distinct
PV time, boot-layout, device and HVF host identities and remain unqualified.
The supervisor defaults Go workers to the guest's CPU count (one in the etcd
template), while preserving an explicit `GOMAXPROCS` value.
The native PV backend requires cooperative guest boundaries; four Go workers
stalled in the Mac example and are not validated.
`--torn-writes` opts into seeded partial survival of unflushed 512-byte disk
sectors on machine crash (off by default). It is part of run identity and replay
restores it automatically. It applies only to retained disks; this template
currently uses RAM-backed storage. See `docs/runtime.md` for the disk contract.

`run`, `prepare` and `explore` accept `--timebase counter|pv` to choose
semantic guest progress (default `counter` on Linux; Mac uses only PV), with no environment override. Counter counts retired guest branches; PV compute
between exits costs no virtual time, and time advances at guest exits, syscalls
and clock reads. The choice changes run identity and results. Counter is omitted
from semantic parameters to preserve existing identities; PV is retained as
`timebase: pv`. Replay uses the recorded choice and accepts no override; unknown
or mismatched timebases are refused, even when an engine change is allowed.
Run/replay and inspect print `timebase: VALUE`; `inspect --json` includes the
semantic profile and the VMM's reported `report.timebase` when available.
Bundles preserve the timebase. PV uses no PMU counter, but the CLI retains the
same host admission, CPU/SMT lease and Zen MSR policy for both choices.

`run`, `prepare` and `explore` accept `--page-timer` (off by default,
no environment override). An ABI-4 kernel arms one-shot clockevents by writing
an absolute deadline into the shared time page, avoiding port `0x530` exits.
The VMM advertises the capability only after the guest announces ABI 4;
with the flag enabled, older kernels fail at boot with a rebuild/drop-flag error.
The flag is aimed at PV: an earlier arm lowers the clock bound and uses the same
syscall budget as the VMM planner, without a prompt syscall exit. Watchdog splits
reproduce that bound and budget without adopting the page word, expiring timers
or drawing jitter. Adoption happens only at legal exits (including HLT and PV
yields), canonical spin-idle landings and world slice boundaries, after the
exit clock is accounted. A halted machine wakes from the adopted deadline
(including its seeded jitter), never from a raw page store. In the counter
timebase, an earlier page arm waits for the next exit and may fire late by up
to a slice. One-shot stop/restart
uses page disarm and avoids PIT mode port writes.
The requested semantic parameter is retained
as `page_timer: true` only when enabled. Replay restores it without an override.
Timer jitter uses the existing seeded stream. This changes enabled-run timing
and witnesses; omission preserves the existing identity encoding for the same
engine and kernel inputs.
Checkpoint parent identities also bind the page timer only when enabled,
preventing checkpoint reuse across timer modes while preserving feature-off hashes.

Charged timer I/O retains its full cost across scheduler boundaries; pending
operations are included in terminal witnesses.

For an experimental runtime fork, `seedbox run --checkpoint-at-ns 1000000000
--branch-seed 42` captures the world at one simulated second
and continues with branch-identity-derived simulator randomness and guest CRNG
entropy. Use the newly built VMM and current K2/ABI-3 guest kernel.
These are semantic options with no environment backing; replay reads them from
the retained run. A branch seed equal to the parent seed continues without any
reseeding. Application-owned PRNG state stays in captured RAM. Each branch's
retained attempt uses `output/machines/` for prefix and continuation logs,
`.chunks` indexes and `--full-state` page hashes; use `logs` and `diff` on that attempt.
The public CLI currently supports one checkpoint branch, without saved snapshot
files or event-triggered capture. See the
repository's `docs/runtime.md` for coverage and pending KVM qualification.

From another terminal, `seedbox pause RUN` pauses the host's entire world at a
slice boundary and `seedbox resume RUN` continues it. Use the executing attempt
directory, identifiable by `RUN/output/host-control`, with the same privileges as
execution. Replay has a new attempt directory. Both commands wait for an engine
acknowledgement; guest time, state and witnesses remain fixed while paused.
Wall-time limits and cancellation remain active, and pause time counts against
the wall limit. CPU leases remain held until execution ends. This requires a
host-control-capable engine; see the repository's `docs/runtime.md` for details.

For optional build timestamp control, `prepare`, `run` and `explore` accept
`--build-epoch SECONDS` (0..=4294967295; default absent, no environment backing).
This explicitly changes new build-layer file and directory timestamps and
requires the admitted project-Nix native Podman, including for exploration.
Registry and base image metadata is preserved. Pin/control other build inputs;
an epoch alone cannot make arbitrary builds reproducible. Services sharing one
frozen native build source reuse preparation work in that call, while retaining
independent launch/config overrides. Replay needs no epoch option and uses the
complete retained application release layers. Deterministic execution uses those
captured bytes and does not require independently rebuilt OCI images to match.
See `docs/runtime.md` in the Seedbox repository.

All application storage, including Redis AOF and etcd data directories, is private
RAM-backed storage recreated on each fresh run/replay. No data persists to the host.
Service names resolve through guest hosts files. The workload owns readiness
retries; Compose dependency ordering is not implemented.

For a negative control, set `SEEDBOX_PLANT_FAILURE: "1"` in the Redis workload,
or append `--plant, missing` to the etcd client's command list. The expected result
is FAIL; a fresh replay must reproduce that FAIL with MATCH. Nonempty fault
schedules remain unsupported until their guest effect timing is verified.

`explore` validates runtime inputs and host admission before its one preparation,
then executes a finite consecutive seed sequence with one worker by default.
`--workers N` (1..32) bounds parallel attempts and requires automatic CPU selection
(omit `--cpu` and `SEEDBOX_CPU`); all N eligible physical cores and siblings must be
available and admitted before preparation. Admission refusal
retains batch ERROR without preparation or seed dispatch; each seed rechecks its host.
Each run retains its ordinary manifest and replay inputs under the printed batch
path. FAIL and witnessed simulated TIMEOUT continue; infrastructure errors and
cancellation stop. Count defaults to 1 and seed to the Compose seed. `--json` prints
one report with separate outcomes and undispatched seeds. Version 2 batch
checkpoints preserve seed/index dispatch ordering independently of completion order,
with unfinished/running attribution and a pending suffix. Earlier batch bytes need
their prior reader; ordinary run inspection/replay is unchanged. Infrastructure
uncertainty stops new dispatch, cancels only owned work and waits for cleanup.
Reports separate measured process-family CPU, elapsed wall, peak active attempts
and summed completed-attempt wall (a conservative guest-core charge once all
owned attempts return, not exact lease time). Partial sums omit running intervals.
Real serial/parallel and core invariance still require runtime qualification.
Build exploration requires
the inspected local project-Nix Podman closure; cancelled builds remain
cleanup-unverified ERROR with their private tag retained until cessation is verified.
See the repository's `docs/runtime.md` for bounded preparation evidence and limits.

To share a retained run, use `seedbox export .seedbox/runs/ATTEMPT -o run.seedbox.tar`.
On Linux or macOS, `seedbox import run.seedbox.tar --state imported` prints a new run
directory that `seedbox inspect` can read. Replay that directory with
`seedbox replay IMPORTED_RUN --state imported` on a compatible host for the
recorded backend, using a compatible installed Seedbox release. No original checkout, registry or
image rebuild is needed. Bundles preserve retained logs and visible omissions;
output omitted by the original capture policy is not transferred. Export/import
never replace existing data; the default transfer cap is 32 GiB for unique objects
and separately for materialized log bytes (`--max-bytes` overrides it).

Directed network baselines belong in `x-seedbox.network.links`, for example
`[{from: server, to: client, latency: 20ms, jitter: 2ms, loss_ppm: 50000}]`
using this project's service names. Each pair is unique; latency is required,
jitter and loss default to zero. Latency cannot be below global network latency,
jitter must be less than latency, their sum is at most one hour, and loss is an
integer from 0 to 1000000. `seedbox validate` checks these bounds.

Guest controllers can call `world.shape(["server"], ["client"], latency=20_000_000,
jitter=2_000_000, loss_ppm=50000)` and `world.heal(handle)`. Shapes apply only in
the requested direction. The latest live layer wins; healing reveals the prior
policy. Partitions dominate shapes, and already routed frames retain their
send-time decisions. Inspection includes shape handles and `loss_dropped` counters.
Network state contributes a `network_witness` to the final world witness.

Controllers can also discover directional TCP incarnations with
`world.connections("server", "client")`, then pass an exact returned record to
`world.drop_connection(record)`. The returned handle arms loss for that direction
only; `world.heal(handle)` returns matched/dropped counts. Require `dropped > 0`
when checking that a fault occurred. Reused tuples do not inherit a stale selector.
See the repository's `docs/runtime.md` connection selector section for pagination,
SYN/FIN/RST semantics, bounded tracking and unsupported/fragmented traffic.

Run/replay check that the host has the KVM and perf capabilities the engine
needs; CPU model, microcode, CPUID, engine and kernel bytes are not pinned.
Passing on a second machine is not cross-host qualification.

Check the host and installed engine/kernel with `seedbox doctor` (Linux KVM as
root, or Apple Silicon Mac as the current user) before running. It uses the normal CPU/runtime defaults without building,
preparing images or starting guests. `VALID` means execution-profile compatibility,
not cross-host replay qualification; `--json` shows the admitted observations.

Known non-Zen counters require explicit `--allow-unqualified-cpu` for run,
explore and replay and remain unverified. They retain exact release/counter/input
checks and SMT ownership but carry no qualified AMD proof or AMD MSR writes.
`doctor` refuses those counters. Disabled selected-event readiness precedes preparation and
is rechecked under leases; it never enables a guest counter. Intel is not qualified.

Engines and guest kernels are not pinned: any built engine and kernel are
admitted, and each run records their hashes. `--allow-dev-engine` is still accepted and has no effect.
Every host/CPU/PMU/MSR/lease/ABI check still applies.

Compare two retained run directories on Linux or macOS with `seedbox diff RUN_A
RUN_B [--json]`. Import bundles first. Diff reports world and machine witnesses,
changed identity/semantic fields, full-state parts such as RAM and disk, and the
first differing retained trace or terminal record (including state parts).
`seedbox run --full-state` compares whole-machine state; Linux additionally retains
per-page RAM evidence for diff and replay diagnostics. See [docs/runtime.md](https://github.com/gardnervickers/seedbox/blob/main/docs/runtime.md#compare-retained-runs).
Ordinary runs do not retain exit traces; terminal times cannot locate an
earlier divergence. Missing evidence and omissions stay explicit. Exit 0 MATCH
means equality of compared evidence, not replay qualification; DIFFERENT exits 1,
and usage/setup or invalid evidence exits 2. No guests execute and no state is created.

Read committed-time chunks with `seedbox logs RUN --machine NAME --since-ns N
--until-ns N`. Consecutive bytes from the same machine/incarnation/stream within
one world slice coalesce until another stream writes. Times are the first byte's
simulated commit time, inclusive at the start and exclusive at the end. Filters
select whole chunks, so a selected chunk can contain later bytes beyond the
upper bound. Omit filters to read all indexed output. Stdout contains raw bytes
in time, machine name and per-machine output order, without prefixes or added
newlines; status and omissions go to stderr. Add `--json` for metadata and
lossless hexadecimal (`bytes_hex`). No application log format is required.
Check stderr or JSON `omissions` for old engines without indexes, capture bounds
or retention limits. Indexes travel with retained logs in export/import bundles. Reading logs needs no KVM or root.

For a scenario using the guest control API, the development engine supports
`world.arm_console_stop(source, b"MARKER", target, match_timeout=1)` and
`world.arm_console_partition(source, b"MARKER", left, right, match_timeout=1)`.
Arm confirmation means armed; inspect the trigger and target state for the
actual effect. `world.cancel_trigger(trigger)` cancels an unmatched trigger.
Markers use only future committed console bytes, and effects occur at a safe
future world barrier. See [the trigger contract](https://github.com/gardnervickers/seedbox/blob/main/docs/triggers.md)
for bounds, deadlines and the outstanding development-engine AMD checks.

`prepare`, `run` and `explore` also accept `--deferred-tx` (off by default).
It suppresses virtio-net TX kicks while deterministic polls service the queue,
with slices capped at 50 microseconds of simulated time. RX is unchanged.
This changes packet timing and identity: the profile records `deferred_tx=true`
only when enabled. Replay uses the recorded mode without an override.
Raw `output/vmm.json` reports doorbells, TX packets polled at boundaries and
mean/max upper bounds on publish-to-pickup delay. These bounds start at the
previous empty-ring observation; ordinary guest RAM stores are not timestamped.
