# CLI reference

[Back to the quick start](../README.md)

`seedbox run` with no arguments runs the Compose project in the current
directory. Every runtime input has a default, so no flags are required:

The primary client commands use positional resource names:

```sh
seedbox run [COMPOSE_OR_SIMULATION_YAML] [--seed N] [--detach] [--json]
seedbox explore [COMPOSE_OR_SIMULATION_YAML] [--seed N] [--count N] [--detach] [--json]
seedbox get simulations [--json]
seedbox get simulation ID [--watch] [--json]
seedbox describe simulation ID [--json]
seedbox delete simulation ID
seedbox delete checkpoint ID NAME
seedbox rerun ID [--detach] [--json]
seedbox replay ID [--json]
seedbox compare BASELINE_ID SIMULATION_ID [--json]
seedbox stop ID
seedbox events ID
seedbox playback ID [REQUEST_ID] [--json]
seedbox pause ID [--request-id REQUEST_ID] [--json]
seedbox resume ID [--request-id REQUEST_ID] [--json]
seedbox step ID [--request-id REQUEST_ID] [--json]
seedbox rate ID RATE [--request-id REQUEST_ID] [--json]
seedbox inject ID ACTION_FILE [--request-id REQUEST_ID] [--json]
seedbox heal ID HANDLE [--request-id REQUEST_ID] [--json]
seedbox logs ID [MACHINE] [--stream stdout|stderr] [--json]
seedbox serve [--state DIRECTORY] [RUNTIME_INPUT_OPTIONS]
```

`run` starts or connects to the local server and follows status over its API.
`get simulation ID --watch` attaches a status observer until completion or Ctrl-C.
It emits a YAML document stream, or newline-delimited JSON with `--json`.
Detaching never cancels the simulation.
Submit saved simulation documents positionally: `seedbox run simulation.yaml`.

`seedbox replay ID` submits a server-owned rerun of a completed simulation and
compares its retained results. It prints `MATCH`, `MISMATCH` or `NOT_COMPARABLE`
and exits 0, 1 or 2 respectively. Use `--json` for the comparison document.
`seedbox rerun ID` instead reports the workload oracle verdict.
`seedbox compare BASELINE_ID SIMULATION_ID` compares two completed simulations
without executing either. Comparisons never change either simulation's verdict.
Replay accepts simulation IDs; importing old offline run bundles into the server
catalogue is still pending.


Oracle exit zero passes; any nonzero exit fails. `x-seedbox.check.exit_code`
is not configurable and must be omitted from Compose files.

Simulation preparation has a 15-minute server deadline, separate from execution
limits. On expiry the server cancels preparation and waits for cleanup. Without
an oracle exit, a preparation or execution timeout reports `TIMEOUT` (exit 124)
and the simulation's test verdict remains incomplete.

`events` streams typed observations as newline-delimited JSON until completion
or Ctrl-C. Disconnecting this observer does not stop the simulation. Delivery
gaps are explicit records; reconnecting starts from the current position with
no history. Use `describe` to recover current status after a gap. A completed
simulation's event stream immediately returns a closure record.

Each event includes its `simulation` ID and revision. `simulatedTimeNs` is
optional and omitted before execution; `position` carries an exact committed
position when known, including control acknowledgements. `preparationProgress`
events identify a `machine` and `stage`: `image` covers image build, pull and
export; `guest` covers guest image assembly; `ready` marks readiness.
`executionTerminated` carries typed `termination` information, including errors,
separately from the workload oracle verdict.

Pending deletion emits `deletionRequested` and sets
`metadata.deletionRequestedAtUnixMs` once. Describe and watch expose that pending
state while the server stops the simulation and its descendants.

`logs` streams application bytes without decoration. Omission diagnostics go to
stderr; `--json` instead emits newline-delimited records with stream, incarnation,
simulated time and lossless byte arrays. There is no backlog on reconnect.
The former run-directory console reader is now `seedbox retained-logs`.
Logs and events follow live output by default; neither needs a `--follow` flag.
`playback` returns the server's latest observation: `state` contains committed
position, requested rate, measured `achievedRate`, pending step and terminal state;
`ageNs` says how long ago the server received it. The server samples independently
of clients, normally every 250 ms, and never invents progress during an unavailable
worker interval. `achievedRate` carries exact `simulatedTimeNs` and `wallTimeNs`
since the rate was selected (or the first boundary); dividing them gives achieved
simulated seconds per wall second. It is absent before a measurable interval.
The command returns YAML by default. A simulation without a
running worker reports an unavailable endpoint; inspection never starts a worker
or advances simulated time. `pause`, `resume`, `step` and `rate` submit durable
requests. Rate is `0` (pause), a positive integer or fraction such as `1/100`, or
`unlimited`. Step requires a paused simulation and admits exactly one committed
round, then remains paused.

Mutation commands print their request ID to stderr and one receipt as YAML or
JSON to stdout. They briefly wait for acknowledgement; an `accepted` receipt
means the server is still driving the request. `acknowledged` identifies the
boundary and any worker rejection. For step, `stepping: true` means its admitted
round has not committed yet. Read `playback ID` to observe completion.
Reusing `--request-id` with the same command returns the original receipt; a
different command conflicts. `playback ID REQUEST_ID` reads that receipt after
completion or restart. IDs are scoped to a simulation and retained for its
lifetime, with a limit of 4096 control requests per simulation. A lost client
connection never cancels an accepted action. `interrupted` means execution ended
before the acknowledgement was durably recorded; the action may have occurred,
and is never resent to a replacement worker.

The API uses `POST /v1/simulations/ID/controls` for mutations and
`GET /v1/simulations/ID/controls/REQUEST_ID` for their durable receipts.
`GET /v1/simulations/ID/playback` remains the cached playback observation.
Playback and fault controls share request identity and one pending mutation per
simulation. Fault admission uses a prepared worker proposal, durable intervention
history, then worker commit; an acknowledgement means admitted, while fault events
report its eventual effect. `inject ID ACTION_FILE` reads one typed YAML or JSON
action (use `-` for standard input); `heal ID HANDLE` removes a healable fault.
Input files are bounded to 64 KiB and the serialized control request to 4096 bytes.
Machine names, handles, parameters and backend capabilities are checked at the
committed boundary. A rejected action is reported in the receipt and exits 2.

For example, save this as `partition.yaml`:

```yaml
action: partition
handle: isolate-db
left: [api]
right: [db]
```

Then use `seedbox inject ID partition.yaml`, followed by
`seedbox heal ID isolate-db`. Process actions use `action: stop`, `restart`,
`pause_process` or `resume_process` with `machine: NAME`. Whole-machine actions
use `pause`, `resume`, `crash` or `reboot`; the file schema also supports shape,
connection drop, clock skew/drift and throttle. Application process pause is
distinct from `seedbox pause ID`, which pauses the whole simulation.

`rerun ID` requires a completed simulation and reproduces retained fault inputs
at their original boundaries. Playback remains available during rerun, but new
fault injection is rejected to preserve the recorded history.

The run-directory controls are now `retained-pause RUN` and `retained-resume RUN`.
Describe defaults to YAML; JSON mode produces one document. IDs replace retained
directory paths in run output. Kernel/helper/CPU/watchdog/full-state/checkpoint
flags are no longer run options. Server asset discovery and the remaining
advanced/offline command migration are tracked in the
[configuration audit](simulation-configuration-audit.md).

The table distinguishes project settings, server startup options and offline
artifact commands. Runtime input options belong to `serve` and `prepare`;
`doctor` accepts only kernel, VMM and CPU selection. They are not submission
options for `run`, `explore`, `rerun` or `replay`.

| Input | Default | Override |
| --- | --- | --- |
| Compose file | `compose.yaml` in the given directory (default `.`) | positional path |
| Directed link baselines | no overrides; global latency/jitter and zero loss | `x-seedbox.network.links` |
| Link bandwidth and queue | infinite bandwidth, unbounded queue | `x-seedbox.network.bandwidth_bps`, `queue_bound`, or the same keys in `links` |
| Runtime network impairments | off | `world.shape`: reorder, duplicate, burst loss, latency spike and TCP reset parameters |
| Connection selectors | disabled; discover and select through the guest control API | `world.connections`, `world.drop_connection` |
| Seed | Project seed, otherwise allocated once by the server | `--seed` on `run` or `explore` |
| Guest wall clock | latest `created` time in the prepared images (at least 86400) | `x-seedbox.wall_clock_epoch` (no CLI flag) |
| Guest RAM per service | 1024 MiB | `x-seedbox.machine.memory_mb` (64..65536; Mac 64..3072) |
| Deferred TX kicks (Linux KVM) | off; service TX as in the current engine | none; internal engine setting |
| Runtime checkpoint | absent; ordinary execution | `checkpoint ID --request-id NAME`, then `branch ID NAME` |
| Page timer (Linux KVM) | off; ABI-4 clockevent stores, aimed at PV | none; internal engine setting |
| Exploration count | `1` | `--count` |
| Exploration concurrency | server-owned execution capacity | No per-submission option |
| Torn disk writes | off (drop all unflushed sectors on crash) | `x-seedbox.disk.torn_writes: true` |
| CPU | Server-owned allocation; Linux leases a free core; Mac uses HVF scheduling | `doctor --cpu N` or `SEEDBOX_CPU` for doctor (Linux only) |
| Guest kernel | `kernel` beside the executable | `--kernel`, `SEEDBOX_KERNEL` |
| VMM | the platform engine linked into `seedbox`; runtime commands never invoke Cargo | `--vmm`, `SEEDBOX_VMM` (Linux legacy standalone engine) |
| Guest helper archive | embedded by the complete build; explicit archive for plain Cargo builds | `--runtime-assets`, `SEEDBOX_RUNTIME_ASSETS` |
| Guest supervisor | from the helper archive | `--guest-init`, `SEEDBOX_GUEST_INIT` |
| mke2fs (only for named volumes) | from the helper archive | `--mke2fs`, `SEEDBOX_MKE2FS` |
| Engine and guest kernel | any built engine and kernel are admitted; the run records their hashes | `--vmm`, `--kernel` |
| Server state | `$XDG_STATE_HOME/seedbox`, otherwise `$HOME/.local/state/seedbox` | `serve --state`, `SEEDBOX_STATE` |
| Offline preparation and artifact state | `.seedbox/` | `--state`, `SEEDBOX_STATE` on `prepare`, `export` and `import` |
| Diff inputs | Two required retained run directories; no runtime inputs or state lookup | positional `RUN_A RUN_B` |
| Witness log (Linux) | always; retain `output/machines/NAME.witness-log` for diff and replay to locate the first diverging 1 ms interval | none |
| Per-page RAM evidence (Linux) | off; full-state capture retains `output/machines/NAME.pages` beside machine traces | `serve --full-state` or `prepare --full-state` |
| Log selection | All indexed machines and both streams, with no time bounds; raw bytes on stdout (`--json` for metadata) | `retained-logs RUN --machine NAME --stream console --since-ns N --until-ns N` |
| Bundle transfer cap | 32 GiB each for unique objects and materialized logs | `--max-bytes` |

The server socket must fit the host's Unix socket path limit. If a custom state
path is too long, startup fails before creating state and asks you to choose a
shorter `SEEDBOX_STATE` directory.

Settings that describe the simulated world live in the Compose file's
`x-seedbox` section; flags choose only how a run executes. Every key below is
optional and shows its non-default form:

```yaml
x-seedbox:
  machine: {memory_mb: 2048}
  disk: {torn_writes: true}
```

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
The host checks are the KVM capabilities the engine needs; CPU model,
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
and mke2fs, with `linux/arm64` as the image platform. `doctor --cpu` and the standalone `--vmm` override
remain Linux-only. Exploration concurrency belongs to the server.

`x-seedbox.disk.torn_writes: true` enables seeded partial survival of unflushed
512-byte disk sectors on crash. It is recorded in run identity and replayed
automatically. It applies to retained disks, including Compose named volumes. See [disk semantics](runtime.md#disk-crash-policy).

Virtual time and automatic application progress follow the
[determinism catalogue](determinism-catalogue.md#progress-from-translated-blocks).

`seedbox help run` lists every option. The guest Python API supports
[bounded committed console triggers](triggers.md) for future stop/partition
faults in the development engine; see that contract and host validation handoff. A run prints `OUTCOME: simulation-id`;
exit status 0 is PASS (or replay MATCH), 1 FAIL (MISMATCH), 2 ERROR, 3
INCOMPLETE and 124 TIMEOUT. A cancelled or incomplete replay exits 2
(NOT_COMPARABLE). Offline `diff` exits 0 for MATCH, 1 for DIFFERENT,
and 2 for usage/setup or invalid retained evidence.

Starting the server with `seedbox serve --full-state` also retains per-page RAM hashes with each machine's
final state. Replay inherits that mode. On RAM-only state-digest differences,
offline `diff` prints the differing PFN count and first eight PFNs on stderr.
Server comparison documents report aggregate witness and full-state equality. The files travel in bundles under the ordinary retention policy
and are excluded from run identity, witnesses and replay verdicts. See the
[binary format and diagnostic fields](runtime.md#compare-retained-runs).

For legacy retained attempts, pause a running world from another terminal with
`seedbox retained-pause RUN`, then
continue it with `seedbox retained-resume RUN`. Use the executing attempt directory
(for replay, its new attempt), identifiable by `RUN/output/host-control`. Both
commands wait for a boundary acknowledgement. All guests and virtual time freeze;
wall-time limits and cancellation remain active. See [host pause](runtime.md#retained-host-pause)
for the mechanism and development-engine verification requirements.

For legacy retained attempts, text output from `inspect`
prints up to five non-empty lines from the final 8 KiB of that attempt's
`output/vmm.stderr` to stderr, prefixed with `vmm stderr:`. Missing or unreadable
logs do not change the outcome. Retained records and `--json` output are unchanged.
`inspect` keeps the recorded `error:` line on stdout alongside the run status.

[Draft format and runtime scope](compatibility.md) distinguishes inspection
and transfer from execution compatibility. Retained inputs replay with their
retained engine; changing engines may change execution. Development execution
has no version or qualification allowlist.

Capture a paused simulation with `seedbox checkpoint ID --request-id NAME`,
then continue from that capture with `seedbox branch ID NAME`. See
[checkpoint and branch operations](#checkpoint-branches-in-development) below.
The older seed-reseeding checkpoint design is documented as historical engine
behavior in [runtime checkpoint design](runtime.md#runtime-checkpoint-design-experimental).

## Delete simulations

Delete a simulation with `seedbox delete simulation ID`
(`DELETE /v1/simulations/ID`). Deletion cascades to its descendants, including
reruns, branches and seed-range members. The server stops the source and all
descendants, waits for termination, then removes their catalogue resources,
checkpoints, retained input descriptors and private execution state. New
descendants are blocked while deletion is in progress. Active simulations can
be deleted; no separate stop or dependent deletion is required. Shared
content-addressed objects remain available to other simulations; object garbage
collection is deferred. Repeating deletion succeeds.

Completed seed-range members may be deleted. `seedbox get simulations SOURCE_ID`
omits deleted members and reports their count as `deletedMembers`.

## Finite exploration

`seedbox explore [COMPOSE] --seed FIRST --count COUNT` submits consecutive seeds
to the local server. Count defaults to one and is bounded at 4096; the first seed
comes from the explicit option, the project, or one server allocation. Overflow
is rejected before execution. The server runs the first simulation, freezes its
preparation inputs, and queues the remaining seeds against that same closure.
Each seed has its own simulation ID, logs, events, status and oracle verdict.
Interactive changes to one simulation are not inherited by other seeds.

`--detach` returns the source simulation ID after acceptance. Use
`seedbox get simulations SOURCE_ID` to inspect the range and its current members,
and `seedbox stop SOURCE_ID --all-seeds` to stop it. Closing the client leaves
accepted work owned by the server; foreground Ctrl-C requests a range stop.
The server owns concurrency and CPU allocation; there is no `--workers`,
`--cpu`, asset override, or watchdog option on `explore`.

Accepted range intent and member identities survive restart. Interrupted active
simulations remain incomplete; undispatched seeds continue against retained
inputs. Failed initial preparation ends the range without repeating preparation.
`--json` prints one status document containing member summaries and any range
error. No CLI process-resource measurements or separate exploration archive are
produced. Inspect each simulation with the ordinary describe/log/event commands.

## How execution is controlled

- **Progress and time.** Guest progress is charged at guest exits, syscalls
  and clock reads; idle machines jump to their next event. A host watchdog
  interrupts a guest that makes no exits to report where it
  is stuck, then resumes it without advancing its time.
- **Stops.** Events (timer deadlines, frames, scenario commands) are delivered
  at deterministic points only: natural VM exits. A timer interrupt fires at the
  first exit at or after its deadline, at most `stop_tolerance_ns` late.
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
  Each machine hashes every deterministic exit. Full-state capture adds
  a hash of registers and all guest RAM; the standalone engine also supports
  `--digest` and `--trace` diagnostics.

## Host requirements

- Any Linux x86-64 host with KVM, including cloud VMs with nested KVM, or an
  Apple Silicon Mac. Root is not needed unless the host's `/dev/kvm`
  permissions require it. On Linux the executor acquires the shared CPU/SMT
  leases for the selected core and holds them until its child is reaped; the
  legacy `batch.sh` runner uses the same lease protocol.

Host admission checks the KVM capabilities the engine needs. CPU model,
microcode, CPUID and host-kernel bytes are not checked. Before image
preparation, it temporarily reserves an eligible core and checks each
online/allowed SMT sibling; affinity and reservations restore before
preparation. This is readiness admission; it does not prove cross-host
determinism. Run identities carry no host-policy proof; host-policy fields in
older records are kept as data and do not block replay.

## Check execution compatibility

`seedbox doctor` checks the installed engine, guest kernel and selected host core
with the same readiness checks used by execution. It needs a Linux
KVM host with `/dev/kvm` access, uses the normal
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
separate results; a reproduced FAIL is a successful MATCH. Server startup with
`--full-state` adds per-machine RAM/register comparisons. The server owns
performance settings, which do not change semantic identity. Each run has private application storage: image filesystems are RAM-backed,
and named volumes use retained ext4 disks that survive guest crash/reboot.
The default RAM allocation is 1024 MiB per service. See [runtime instructions](runtime.md)
for compatibility, memory sizing, root access, cancellation and the supported subset.

The [acceptance record](acceptance.md) includes real etcd/Redis passes,
intentional failures, exact replays, full-state comparisons, timeout/cancellation
and lease contention on a Linux KVM host.

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
qualify replay or prove equality of unrecorded state. Performance settings,
attempt metadata and arbitrary console output are excluded.
See [comparison details](runtime.md#compare-retained-runs).

## Read committed-time logs

`seedbox retained-logs RUN [--machine NAME] [--stream console|mgmt] [--since-ns N]
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
```

Use the run directory printed by `import` as `IMPORTED_RUN`. Import executes
nothing. These are offline artifact operations. Importing a bundle into the
simulation server catalogue is not implemented yet, so the imported directory
cannot currently be passed to `replay ID`. Server replay uses retained engine
inputs without image preparation or registry access. A reproduced application
FAIL can still be a replay MATCH.

Bundles include only retained logs. Any capture omissions remain visible and
are printed as `omitted files: N`; omitted local output is not transferred.
Existing files/runs are never replaced. `--max-bytes` changes the local transfer
cap without changing run identity. See [runtime instructions](runtime.md#bundles)
for bounds and supported formats.

The page timer is an internal KVM mode, disabled for new simulations.
Retained runs preserve their recorded mode. An ABI-4 kernel arms one-shot clockevents by writing
an absolute deadline into the shared time page, avoiding port `0x530` exits.
The VMM advertises the capability only after the guest announces ABI 4;
with the option enabled, older kernels fail at boot with a rebuild/drop-option error.
The option is aimed at PV: an earlier arm lowers the clock bound and uses the same
syscall budget as the VMM planner, without a prompt syscall exit. Watchdog interruptions leave the bound, budget and plan origin untouched,
including between the guest's arm loads and stores. A later re-arm retains an
earlier lowered bound until a legal exit; an overdue arm retains its original
`syscalls0 + 1` target. These splits never adopt the page word, expire timers
or draw jitter. Adoption happens only at legal exits (including HLT and PV
yields) and world slice boundaries, after the
exit clock is accounted. A halted machine wakes from the adopted deadline
(including its seeded jitter), never from a raw page store. One-shot stop/restart
uses page disarm and avoids PIT mode port writes.
The profile records `page_timer: true|false`. Replay restores it without an override.
Timer jitter uses the existing seeded stream. Enabling it changes run timing
and witnesses.
Checkpoint parent identities also bind the page timer only when enabled,
preventing checkpoint reuse across timer modes while preserving feature-off hashes.
This composes with deferred TX's `+deferred-tx` identity suffix: all four option
combinations have distinct parent and branch identities, and both off retains
the original encoding.

### Checkpoint branches (in development)

`seedbox checkpoint ID --request-id NAME` captures a paused simulation at a
committed boundary. List captures with `seedbox get checkpoints ID` and inspect
one with `seedbox describe checkpoint ID NAME` (both accept `--json`). Wait
for `checkpoint.state: captured` before running
`seedbox branch ID NAME --detach`. The branch gets a new simulation ID, preserving
its source seed, configuration and fault history up to the checkpoint. Its
metadata names the parent simulation and checkpoint. Follow its application
output with `seedbox logs BRANCH_ID [MACHINE]` and its internal events with
`seedbox events BRANCH_ID`. Historical application logs are not replayed.

Both Linux KVM and Apple Silicon HVF have native validation for capture, branch
logs, matching source results, fault isolation, and branch rerun after a server
restart.
`seedbox resume ID` continues a paused simulation; it does not create a branch.
After admitting a fault, step through one committed round before capturing a
checkpoint so the snapshot includes its effects.

Delete an unused capture with `seedbox delete checkpoint ID NAME`
(`DELETE /v1/simulations/ID/checkpoints/NAME`). The source simulation must be
completed, and deletion is rejected if any branch references the checkpoint.
Deletion hides the capture from discovery and removes its captured metadata,
machine files and frozen intervention copy. The control receipt remains for
history and idempotent retries. Deletion during active execution is deferred in
[local hardening](../LOCAL-HARDENING.md).
