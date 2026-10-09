# CLI reference

[Back to the quick start](../README.md)

`seedbox run` with no arguments runs the Compose project in the current
directory. Every runtime input has a default, so no flags are required:

| Input | Default | Override |
| --- | --- | --- |
| Compose file | `compose.yaml` in the given directory (default `.`) | positional path |
| Directed link baselines | no overrides; global latency/jitter and zero loss | `x-seedbox.network.links` |
| Link bandwidth and queue | infinite bandwidth, unbounded queue | `x-seedbox.network.bandwidth_bps`, `queue_bound`, or the same keys in `links` |
| Runtime network impairments | off | `world.shape`: reorder, duplicate, burst loss, latency spike and TCP reset parameters |
| Connection selectors | disabled; discover and select through the guest control API | `world.connections`, `world.drop_connection` |
| Seed | `x-seedbox.seed` from the Compose file | `--seed` |
| Guest wall clock | latest `created` time in the prepared images (at least 86400) | `x-seedbox.wall_clock_epoch` (no CLI flag) |
| Deferred TX kicks (Linux KVM) | off; service TX as in the current engine | `--deferred-tx` (prepare/run/explore) |
| Runtime checkpoint (Linux KVM) | absent; ordinary execution | `--checkpoint-at-ns T --branch-seed B` (run only; B=P continues without reseeding; no environment backing) |
| Timebase | Linux: `counter` (retired guest branches); Mac: `pv` | `--timebase` (prepare/run/explore/doctor) |
| Page timer (Linux KVM) | off; ABI-4 clockevent stores, aimed at PV | `--page-timer` (prepare/run/explore) |
| Exploration count | `1` | `--count` |
| Exploration workers | `1`, sequential execution; Mac currently requires `1` | `--workers` (1..32; Linux automatic CPU selection for parallel execution) |
| Torn disk writes | off (drop all unflushed sectors on crash) | `--torn-writes` |
| CPU | Linux: first free core other than CPU 0's, under the shared lease; Mac: automatic HVF scheduling | `--cpu`, `SEEDBOX_CPU` (Linux only) |
| Guest kernel | built locally in the OS temporary directory | `--kernel`, `SEEDBOX_KERNEL` |
| VMM | the platform engine linked into `seedbox`; runtime commands never invoke Cargo | `--vmm`, `SEEDBOX_VMM` (Linux legacy standalone engine) |
| Guest helper archive | embedded in release builds; local temporary archive for debug builds | `--runtime-assets`, `SEEDBOX_RUNTIME_ASSETS` |
| Guest supervisor | from the helper archive | `--guest-init`, `SEEDBOX_GUEST_INIT` |
| mke2fs (only for named volumes) | from the helper archive | `--mke2fs`, `SEEDBOX_MKE2FS` |
| Engine and guest kernel | any built engine and kernel are admitted; the run records their hashes | `--vmm`, `--kernel` |
| State | `.seedbox/` | `--state`, `SEEDBOX_STATE` |
| Diff inputs | Two required retained run directories; no runtime inputs or state lookup | positional `RUN_A RUN_B` |
| Per-page RAM evidence (Linux) | off; under `--full-state`, retain `output/machines/NAME.pages` beside machine traces | existing `--full-state` |
| Log selection | All indexed machines and both streams, with no time bounds; raw bytes on stdout (`--json` for metadata) | `logs RUN --machine NAME --stream console --since-ns N --until-ns N` |
| Bundle transfer cap | 32 GiB each for unique objects and materialized logs | `--max-bytes` |

Directed baselines are optional Compose settings. For example, with `server` and
`client` services:

```yaml
x-seedbox:
  network:
    latency: 1ms
    links:
      - {from: server, to: client, latency: 20ms, jitter: 2ms, loss_ppm: 50000, bandwidth_bps: 1000000, queue_bound: 50ms}
```

Each entry affects one direction. Latency is required; jitter and loss default to
zero. Optional `bandwidth_bps` is a positive integer in bits per second;
`queue_bound` is the maximum send-to-departure time, including serialization,
and requires a bandwidth. Omitting both preserves infinite bandwidth. Global
latency remains the fixed scheduling lower bound for every link.
`seedbox validate` checks the services, duplicate pairs, and policy bounds before
preparation. Guest controllers can temporarily override links with
`world.shape(["server"], ["client"], latency=20_000_000, jitter=2_000_000, loss_ppm=50000)`
and release the returned handle with `world.heal(handle)`; see
[network policy](runtime.md#directed-link-policy).
Shapes also accept `bandwidth_bps=1_000_000, queue_bound=50_000_000` (nanoseconds).
Healing a bandwidth shape returns `queue_counts` with matched frames, tail drops
and maximum serialization/queue delay. Require positive drop evidence when a
scenario needs to exercise a full queue. The guest-owned
`harness/acceptance/workloads/bandwidth.yaml` checks serialization delay, queue
drops and a successful exchange after healing; run it and replay its retained run
on a Linux KVM host.

Runtime shapes also support packet reorder and duplication, burst loss, latency
spikes and TCP reset injection. Each uses an optional seed-derived stream and
returns `network_fault_counts` when healed. For example:
`world.shape(["client"], ["server"], latency=1_000_000, reorder_ppm=500_000, reorder_delay=100_000_000)`.
See the [dynamic fault parameters](../python/PROTOCOL.md#dynamic-network-impairments)
and `harness/acceptance/workloads/network-faults.yaml` for dynamic on/off checks
and replay.

For one TCP direction, use `world.connections("server", "client")`, select an
exact returned record with `world.drop_connection(record)`, then
`world.heal(handle)` and require its `dropped` count to be positive. Discovery is
paginated and selectors bind an observed incarnation, so tuple reuse does not
inherit the old fault. See [connection selectors](runtime.md#connection-selectors)
for lifecycle and unsupported-traffic behavior.

Engines and guest kernels are not pinned: any built engine and kernel are
admitted, and each run records their hashes.
The host checks are the KVM and perf capabilities the engine needs; CPU model,
microcode and CPUID are not pinned.

`seedbox --version` reports the package version. New runs retain the complete
`seedbox` executable and its worker protocol; replay launches those
retained bytes even after the installed CLI is upgraded. The executable hash is
an internal integrity reference, so equal version labels never substitute for
equal bytes. Existing runs without a worker descriptor retain the standalone
VMM contract. The combined executable has its own engine identity and needs
separate runtime evidence. Every built engine and guest kernel is admitted.

The ARM application engine is linked into the Mac CLI. Both platform adapters
use the shared world coordinator, including connection selectors and committed
console triggers; ARM arbitrary snapshot/fork reuse remains unsupported. Its PV world identity,
boot-layout evidence and HVF host admission are distinct from the existing ARM
fixture and x86 KVM profiles. Mac defaults use locally built Image, supervisor,
and mke2fs, with `linux/arm64` as the image platform. `--cpu` and `--vmm`
remain Linux-only; Mac exploration is serial.

`seedbox run --torn-writes` enables seeded partial survival of unflushed
512-byte disk sectors on crash. It is recorded in run identity and replayed
automatically. It applies to retained disks, including Compose named volumes. See [disk semantics](runtime.md#disk-crash-policy).

Choose `seedbox run examples/etcd --timebase pv` for the PV clock model.
`prepare` and `explore` accept the same option. PV compute between exits costs no
virtual time; time advances at guest exits,
syscalls and clock reads. The default `counter` counts retired guest branches.
The choice changes results and run identity, has no environment override, and is
retained through export/import. Replay uses the recorded timebase and accepts no
`--timebase` override. `inspect` and run/replay output show `timebase: VALUE`;
`inspect --json` includes the VMM's reported timebase in `report.timebase` when a
report is available. Older counter records omit this report field.

`seedbox help run` lists every option. The guest Python API supports
[bounded committed console triggers](triggers.md) for future stop/partition
faults in the development engine; see that contract and AMD validation handoff. A run prints `OUTCOME: run-directory`;
exit status 0 is PASS (or replay MATCH), 1 FAIL (MISMATCH), 2 ERROR, 3
INCOMPLETE and 124 TIMEOUT. A cancelled or incomplete replay exits 2
(NOT_COMPARABLE). Offline `diff` exits 0 for MATCH, 1 for DIFFERENT,
and 2 for usage/setup or invalid retained evidence.

`seedbox run --full-state` also retains per-page RAM hashes with each machine's
final state. Replay inherits that mode. On RAM-only state-digest differences,
plain `diff` and MISMATCH replay print the differing PFN count and first eight
PFNs on stderr. The files travel in bundles under the ordinary retention policy
and are excluded from run identity, witnesses and replay verdicts. See the
[binary format and diagnostic fields](runtime.md#compare-retained-runs).

Pause a running world from another terminal with `seedbox pause RUN`, then
continue it with `seedbox resume RUN`. Use the executing attempt directory
(for replay, its new attempt), identifiable by `RUN/output/host-control`. Both
commands wait for a boundary acknowledgement. All guests and virtual time freeze;
wall-time limits and cancellation remain active. See [host pause](runtime.md#host-pause)
for the mechanism and development-engine verification requirements.

On a VMM failure, text output from `run`, `replay`, `explore` and `inspect`
also prints up to five non-empty lines from the final 8 KiB of that attempt's
`output/vmm.stderr` to stderr, prefixed with `vmm stderr:`. Missing or unreadable
logs do not change the outcome. Retained records and `--json` output are unchanged.
`inspect` keeps the recorded `error:` line on stdout alongside the run status.

[Draft format and runtime scope](compatibility.md) distinguishes inspection
and transfer from qualified execution. Same software versions, seed and exact
retained inputs must replay on a qualified compatible profile; changing versions
may change execution. Release compatibility remains a V0 qualification gate.

Experimental runtime forks use `seedbox run examples/redis --checkpoint-at-ns
1000000000 --branch-seed 42` with the newly built VMM. The
ordinary seed drives the prefix; the full branch identity keys future simulator
randomness and guest CRNG entropy after one simulated second. Build the VMM and
current K2/ABI-3 guest kernel from this checkout. A branch seed
equal to the parent seed continues without any reseeding. Replay reads these
semantic settings from the retained run. Application-owned PRNG state remains
captured. Each public branch has its own retained attempt with the normal
`output/machines/` layout: `logs` includes its prefix and post-checkpoint chunks,
and `diff` reads page hashes retained by `run --full-state`.
See the [checkpoint design](runtime.md#runtime-checkpoint-design-experimental)
for state coverage, engine-only multiple checkpoints/branches and unverified KVM behavior.

## Finite exploration

`seedbox explore [COMPOSE] --seed FIRST --count COUNT` runs consecutive seeds
with one worker by default, preparing the complete input closure once. Compose
defaults to `.`, the first seed to the Compose seed, and count to 1.
`--workers N` (1..32) permits at most N owned attempts at once; parallel exploration
requires automatic CPU selection, so omit `--cpu` and `SEEDBOX_CPU`. Before
preparation, the executor must be able to reserve and admit N distinct eligible
physical cores together, including their SMT siblings; insufficient capacity is
ERROR. Those transient reservations are released before OCI work. Each running
world still owns its ordinary core/sibling lease. Contention afterward is an
infrastructure error, with no retry or waiting-for-capacity policy. Empty/overflowing ranges fail
before state creation or preparation. Immutable runtime inputs and host admission are
checked before the sole preparation; refusal retains batch ERROR without dispatch.
Each seed uses the ordinary retained-input and leased host rechecks. Application FAIL
and witnessed simulated TIMEOUT continue; ERROR, INCOMPLETE and cancellation stop
dispatch. No seed is retried.

Each exploration retains `exploration.json`, preparation diagnostics and ordinary
per-seed run directories under `attempts/SEED/`. Inspect and replay those run paths
with the existing commands. An unfinished checkpoint remains INCOMPLETE, including
when a process dies after committing a run but before updating the batch. The final
exit precedence is ERROR (2), INCOMPLETE (3), TIMEOUT (124), FAIL (1), PASS (0). `--json` emits one
report with per-attempt outcomes, undispatched count, elapsed time and CLI process-family
resource measurements; application logs remain arbitrary bytes. Checkpoint version 2
records seed indexes in ascending dispatch order regardless of completion order,
with `finished: false` for dispatched attempts whose terminal result is not yet
checkpointed. The pending suffix remains undispatched. Infrastructure or retention
uncertainty cancels only the batch's owned attempts and joins them before returning;
a backend blocked in cleanup prevents a prompt completion claim. Version 1 batch
checkpoints need their prior reader; their bytes and ordinary run manifests remain
unchanged.

`peak_running` counts active orchestration attempts. `active_attempt_seconds` sums
their whole wall intervals, including admission, materialization and cleanup; it is
not exact lease time or measured CPU time. Once all owned attempts return, this
one-world-per-worker backend can use the sum as a conservative guest-core charge.
An unfinished checkpoint omits still-running/uncheckpointed intervals and is not
a complete cumulative charge. CLI process-family CPU totals include
preparation and reaped children. Parallel scheduling and chosen cores are performance
choices excluded from run identity; actual serial/parallel and per-core equality
remain runtime qualification gates.

## How execution is controlled

- **Progress and time.** With the default counter timebase, guest progress is
  the host perf count of retired
  conditional branches (guest only, kernel and user). Virtual time advances
  1 ns per branch; idle machines jump to their next event.
- **Stops (counter timebase).** Events (timer deadlines, frames, scenario commands) are delivered
  at deterministic points only: a natural VM exit, or an exact branch count
  reached by overflow interrupt plus hardware-breakpoint stepping (no TF).
  Syscalls are not stopping points. A timer interrupt fires at the first
  natural exit at or after its deadline, at most `stop_tolerance_ns` late, and
  then by an exact count. An overflow interrupt that skids past a counted
  stop is rolled back and retried closer, or fails the run, rather than moving
  the stop.
- **Guest kernel changes** (`kernel/`): kernel TSC reads return VMM-published
  virtual time; user RDTSC/RDTSCP trap via CR4.TSD and are emulated. The
  kernel's syscall yield hook remains, but the engine never requests it.
- **Network and scheduling.** virtio-net over virtio-mmio into an in-memory
  fabric with per-direction latency, jitter, loss and partitions. The world runs the
  machine furthest behind in slices that never pass its causal horizon (the
  other machines' times plus link latency) or its next event. A slice may end
  at the first deterministic point in the last half-latency before that
  boundary (capped by `stop_tolerance_ns`), and events are delivered at the
  first stop up to that long before they are due: early by a bounded amount,
  never late, and never before they were sent.
- **Witness.** A network hash covers pending frames, link streams and active policies.
  Each machine hashes every deterministic exit; `--digest` adds
  a hash of registers and all guest RAM; `--trace` writes every exit.

## Host requirements

- x86-64 AMD Zen host with KVM, run as root (guest-only perf counting).
  Seedbox picks a branch counter profile from CPUID at startup
  (`crates/seedbox/src/counter.rs`, `crates/seedbox-vmm/src/profile.rs`) and
  records it in each run's identity, so a run replays only with the counter
  that made it. Only Zen is qualified. Non-hybrid Intel cores have profiles
  (`intel-0x5101c4` up to Comet Lake, `intel-0x5111c4` from Ice Lake) that
  have never run on hardware; `run`, `explore` and `replay` use them with a
  warning that results are unverified.
- `--timebase pv` reads no host perf counter, so it needs only KVM and a free
  core: it skips the counter profile, the perf event
  checks and the workaround MSR, and its identity records no counter profile.
  It runs on any x86-64 KVM host, including cloud VMs with nested KVM and no
  guest PMU. Hosts other than the workstation are not qualified for PV.
- **The Zen SpeculativeLockMap workaround must be on** for the cores running
  simulations (MSR 0xC0011020 bit 54, rr's workaround). Without it, branch
  counts drift on lock-heavy code (e.g. Go) and runs diverge. The Rust executor
  acquires the shared CPU/SMT leases, sets
  it with readback, then restores the exact originals after reaping its child.
  The legacy `batch.sh` runner uses the same lease protocol.

The non-Zen path is experimental: it retains the counter in identity, validates exact
supported release inputs and applicable KVM capabilities, and holds every SMT
lease through child cleanup. It never opens AMD workaround MSRs. Disabled selected-event readiness is checked
before preparation and again under held leases; no guest counter is enabled by
admission. Intel determinism is not established.

Host admission checks for an AMD x86-64 CPU and the KVM and perf capabilities the
engine needs. CPU model, microcode, CPUID and host-kernel bytes are not checked.
Before image preparation, it temporarily
reserves an eligible core, checks each online/allowed SMT sibling and opens the
exact perf event disabled. Descriptors close and affinity/reservations restore
before preparation. The leased execution core is checked again before MSR writes.
This is readiness admission; it does not prove guest counting, overflow behavior
or cross-host determinism. Run identities carry no host-policy proof; host-policy
fields in older records are kept as data and do not block replay.

## Check execution compatibility

`seedbox doctor` checks the installed engine, guest kernel and selected host core
with the same readiness checks used by execution. It needs a Linux
KVM host (PV needs KVM access; counter also needs PMU/MSR access), uses the normal
`--cpu`, `--kernel` and `--vmm` defaults, and
never builds a missing VMM, prepares images or starts guests. `--json` prints the
exact admitted observations. Success prints `VALID` after probe resources and
thread affinity are verified cleaned up; problems use the ordinary error and
exit status 2. This is compatibility readiness, not cross-host replay qualification.

## Applications

`seedbox init my-app --template etcd` (or `redis`) creates a self-contained
project; the checked-in [`examples/etcd`](../examples/etcd/compose.yaml) and
[`examples/redis`](../examples/redis/compose.yaml) build their workload Dockerfiles through your running Docker-compatible engine. Their
Dockerfiles package the workload; image defaults and Compose overrides launch the
services. No application-specific initramfs, boot arguments, or world JSON needs
to be authored. The existing etcd acknowledgement-ledger checker is unchanged and
now lives with its Compose example. Generated projects include their own source.

Each attempt retains exact image manifests/layers, kernel, guest archives,
configuration, engine, logs and result. Replay uses the retained content store
without registry pulls or builds. PASS, FAIL, ERROR, TIMEOUT and INCOMPLETE are
separate results; a reproduced FAIL is a successful MATCH. `--full-state` adds
per-machine RAM/register comparisons. `--margin` and `--undo` change performance without
changing semantic identity. Each run has private application storage: image filesystems are RAM-backed,
and named volumes use retained ext4 disks that survive guest crash/reboot.
The default RAM allocation is 1024 MiB per service. See [runtime instructions](runtime.md)
for compatibility, memory sizing, root access, cancellation and the supported subset.

The [acceptance record](acceptance.md) includes real etcd/Redis passes,
intentional failures, exact replays, full-state comparisons, timeout/cancellation,
lease contention and exact MSR restoration on the workstation.

## Compare retained runs

`seedbox diff RUN_A RUN_B [--json]` compares two retained directories on Linux or
macOS without executing guests. It reports the world witness, each machine's
exit/full-state witnesses and state parts (such as RAM and disk), the first
differing retained trace record (or terminal record with state parts), and changed
identity fields including seed, engine, kernel, images and
semantic parameters. Import bundles first, then pass their printed run paths.

Only manifest-declared, checksum-verified evidence is read. The adapter does not
record traces by default: terminal times cannot identify an earlier exit where
execution diverged. Missing witnesses, traces and capture omissions are explicit.
MATCH means equality of the compared evidence, even if incomplete; it does not
qualify replay or prove equality of unrecorded state. Performance counters,
margin/undo, attempt metadata and arbitrary console output are excluded.
See [comparison details](runtime.md#compare-retained-runs).

## Read committed-time logs

`seedbox logs RUN [--machine NAME] [--stream console|mgmt] [--since-ns N]
[--until-ns N] [--json]` reads checksum-verified retained output on Linux or macOS.
Each chunk coalesces consecutive bytes from one machine/incarnation/stream within
a world slice, ending when another stream writes or the slice returns. Its time
is the first byte's simulated commit time. Filters select whole chunks by that
time: the lower bound is inclusive and the upper bound exclusive, even if later
bytes in a selected chunk commit beyond the upper bound. Equal-time chunks sort
by machine name, then the machine's output order across both streams.

By default, stdout contains the selected raw bytes in chunk order, without
prefixes, encoding changes or added newlines. Status and omission diagnostics go
to stderr. Select one machine and stream for its exact indexed byte sequence.
`--json` includes times, incarnations, byte ranges and lossless `bytes_hex` in one
report. The library's `logs::Reader::copy_chunk` also returns the exact raw bytes.
No application schema or line framing is imposed.

Capture is bounded and independent of the witness. Missing indexes (including
older engines), unindexed raw suffixes, and retention omissions are explicit.
A successful read does not claim complete logs; check stderr or JSON `omissions`.
Bundles retain the indexes through the ordinary log policy.

## Sharing a retained run

Export a run with its exact retained inputs and logs, then import it into fresh
state on Linux or macOS:

```sh
seedbox export .seedbox/runs/ATTEMPT -o failure.seedbox.tar
seedbox import failure.seedbox.tar --state imported
seedbox inspect IMPORTED_RUN
seedbox replay IMPORTED_RUN --state imported
```

Use the run directory printed by `import` as `IMPORTED_RUN`. Import executes
nothing. Replay uses the bundled engine and inputs without image preparation,
registry access or the original checkout, and requires a compatible installed
Seedbox release and qualified AMD Linux host. Transfer does not qualify a new
host. A reproduced application FAIL is replay MATCH.

Bundles include only retained logs. Any capture omissions remain visible and
are printed as `omitted files: N`; omitted local output is not transferred.
Existing files/runs are never replaced. `--max-bytes` changes the local transfer
cap without changing run identity. See [runtime instructions](runtime.md#bundles)
for bounds and supported formats.

`run`, `prepare` and `explore` accept `--page-timer` (off by default,
no environment override). An ABI-4 kernel arms one-shot clockevents by writing
an absolute deadline into the shared time page, avoiding port `0x530` exits.
The VMM advertises the capability only after the guest announces ABI 4;
with the flag enabled, older kernels fail at boot with a rebuild/drop-flag error.
The flag is aimed at PV: an earlier arm lowers the clock bound and uses the same
syscall budget as the VMM planner, without a prompt syscall exit. Watchdog and probe splits leave the bound, budget and plan origin untouched,
including between the guest's arm loads and stores. A later re-arm retains an
earlier lowered bound until a legal exit; an overdue arm retains its original
`syscalls0 + 1` target. These splits never adopt the page word, expire timers
or draw jitter. Adoption happens only at legal exits (including HLT and PV
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
This composes with deferred TX's `+deferred-tx` timebase suffix: all four flag
combinations have distinct parent and branch identities, and both off retains
the original encoding.
