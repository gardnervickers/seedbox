# OCI preparation and execution

See the [quick start](../README.md#quick-start) for asset builds and the etcd example.
Seedbox prepares images through a running Docker or Podman engine, including local
Dockerfile builds. It never starts the engine daemon or builds guest assets at
runtime. Replay uses retained inputs and does not contact the engine.



`seedbox replay ID` submits a server-owned rerun of a completed simulation and
compares its retained results. It prints `MATCH`, `MISMATCH` or `NOT_COMPARABLE`
and exits 0, 1 or 2 respectively. Use `--json` for the comparison document.
`seedbox rerun ID` instead reports the workload oracle verdict.
`seedbox compare BASELINE_ID SIMULATION_ID` compares two completed simulations
without executing either. Comparisons never change either simulation's verdict.
Replay accepts simulation IDs; importing old offline run bundles into the server
catalogue is still pending.

## Simulation server

`logs ID [MACHINE]` follows separate application stdout/stderr streams. Omitting
the machine combines output in committed publication order; `--json` includes
machine attribution and lossless bytes. Meanwhile,
`events ID` follows structured internal observations. Both use bounded live
delivery; reconnecting does not replay earlier output. Raw logs go to stdout,
with omissions on stderr. JSON log mode and events produce newline-delimited
records. The older console/archive interface is `retained-logs RUN`.

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

`run` connects to a user-local server, starting it when absent. It submits a
resolved project and follows structured status; `--detach` returns after
acceptance. `get simulations`, `describe simulation ID`, `stop ID` and `rerun ID`
use the same API. Foreground Ctrl-C requests a stop; disconnecting does not.
`get simulation ID --watch` observes status until completion or Ctrl-C, which
only detaches the observer. It emits a YAML document stream, or newline-delimited
JSON with `--json`. Submit either Compose or a saved simulation document as a
positional path: `seedbox run simulation.yaml`.
`playback ID` reads the active worker's committed position, rate and step state
through the same server. The server serializes worker-control requests, retains
an unacknowledged request across timeouts, and never advances a guest for an
inspection. Preparing or completed simulations without a worker endpoint return
an explicit unavailable response. `pause ID`, `resume ID`, `step ID`, and
`rate ID RATE` submit durable control requests; `--request-id` makes a retry
idempotent and `playback ID REQUEST_ID` inspects its receipt.

After preparation, a simulation enters `Booting`. A committed worker status
confirming initial readiness from every machine advances it to `Executing`.
Later crashes and reboots do not restart this initial stage. These are
server-observed transitions, so control sampling latency affects their timing.
Very short runs can finish before readiness is sampled; exact short-run stage
accounting is still being implemented.

Stage durations use the server's monotonic clock. Describe and list include the
current elapsed duration; status watches refresh it once per second even when
the lifecycle is unchanged. These display updates do not advance the resource
revision or write catalogue records. A stage transition persists its duration,
including preparation that ends in an error. If the server crashes, the active
stage retains its last durable lower bound with `complete: false`; recovery
does not invent a duration for time the server could not observe.

A simulation test passes only on oracle exit zero and fails on any nonzero
oracle exit. Compose rejects `x-seedbox.check.exit_code`; expected success codes
are not configurable. Execution termination is separate: without an oracle exit the test
is incomplete. The CLI reports setup/execution errors as 2, preparation/simulated/wall timeout as
124 and other incomplete execution as 3; an observed oracle exit takes precedence
and maps to 0/1, with execution diagnostics reported separately.
The server records an execution report's oracle exit before finalizing retained
artifacts. A subsequent manifest-write failure therefore retains the observed
test verdict and reports the storage error as its execution termination.
An unsuccessful worker exit or cleanup failure likewise preserves a validated
oracle report while marking execution as an error. A racing cancellation does
not hide an explicit backend or cleanup error.

The following checkpoint section documents existing engine mechanisms. Its old
paired run flags have been removed from the client. Current resource operations
are documented under [checkpoint and branch operations](#checkpoint-branches-in-development).

## Runtime checkpoint design (experimental)

The historical `seedbox run --checkpoint-at-ns T --branch-seed B` interface
executed the parent seed P
up to simulated time T, captures the world, and continues under B. B=P is a
true continuation: every captured stream and guest entropy stays in place.
Each public branch is its own retained run/attempt. Its prefix and continuation
use the ordinary `output/machines/NAME.console` and `.mgmt` files with `.chunks`
indexes, and `--full-state` writes `NAME.pages` alongside them. `retained-logs` and `diff`
read that attempt directly, as they do for ordinary runs and replays.
Both options are semantic parameters with no environment backing. Replay
reconstructs the parent prefix and checkpoint from retained inputs; bundles
need no RAM snapshot. Without the options, the ordinary scheduler, RNG draw
order, dedicated six-field time-page digest and witness inputs remain main's.

Capture occurs at a committed scheduler boundary before control, triggers,
faults and guest work at T. Queued committed console bytes, fault effects and
control requests from completed slices drain into world state before the stop
is tested. Pending world RPCs remain captured. Running machines have reached T;
sleeping, paused, down and exited machines retain their clocks, with no possible
next action before T. Charged I/O may still owe time: its response and device
effect are already committed, and restoring keeps the debt without repeating
the effect. Boot forks still require every machine parked.

The CLI and engine share `crates/seedbox-engine-core/src/checkpoint.rs`. Parent identity
uses the versioned `seedbox-world-parent-v2` domain, config with checkpoint
metadata removed, kernel/initrd/disk paths replaced by content hashes, engine
hash and parent seed. An enabled page timer adds an explicit semantic
tag to the parent hash; omission adds no bytes, preserving feature-off identities.
Checkpoints cannot be reused across timer modes. Branch identity binds this parent, T and B.
The retained RunIdentity also binds T and B; it is not a second parent-identity
scheme. Relocating inputs does not change the branch. With deferred TX enabled,
both sides hash a `+deferred-tx` suffix, separating parent and branch identities
and future branch RNG streams from the off mode. Off keeps the original hash
input. The engine recomputes the supplied CLI identity.

Build the VMM and current K2/ABI-3 kernel from this checkout. #93 was closed;
its reseed hook landed through #126 with PV fields at u64 indices 6–8,
`entropy_gen` at 9, entropy at 10–13, and `SEEDBOX_ABI = 3`. No engine/kernel
pins or host-policy proof are added. A changed-seed branch keys future directed
link loss/jitter, timer jitter, reboot entropy and disk survival on its full
branch identity. Each up machine, including paused guests and controllers,
receives 32 bytes through the time page. The kernel mixes them into its CRNG
before its next kernel random output; the timer hook refreshes the vDSO
generation at its next tick. Pre-boot machines keep the publication until kernel
initialization. Announced ABI <3 is unsupported for changed-seed runtime
reseeding and produces an explicit error. `checkpoint_reseed` reports
`time_page`, `not_up` or `continuation` per machine. Publication does not prove
guest consumption. Application-owned PRNGs and already consumed bytes stay in RAM.

Capture covers the following state from current main:

| State | Capture and continuation |
| --- | --- |
| Machines/devices | RAM, vCPU registers/MSRs/events, full shared page, PIC/PIT, UARTs, virtio network/control/block queues, entropy words/cursor, pending charged I/O, timer RNG, clocks and witness accumulators. |
| PV clock (#85, #89, #113) | Full page captures PV enable/ack, bound and syscall target, as well as the original fields and fork entropy. `MachineSnapshot.syscall_ns` retains the sampled per-machine speed across fresh-VM restore; forks do not resample it. Watchdog interruptions only report diagnostics and resume the existing plan; they never establish a legal clock boundary. KVM-run scratch affects host performance only and resets at restore; `prepare`/`start` overwrite remaining progress and syscall baseline before guest entry. |
| Triggers (#109) | The full `Control` clone carries arm order, KMP failure tables, partial prefix/seen/reset counts, match/effect deadlines, cancellations, statuses and evidence. The committed byte slot is drained before capture. Machine `observe_console` is restored and scheduled trigger boundaries constrain the checkpoint stop. |
| Directed links (#97) | The full `Faults.links` clone carries baselines, active layers, FIFO arrival and serialization departure history, optional queue evidence, use markers and both RNG positions. Changed-seed forks replace future loss/jitter streams only; in-flight packets retain send-time decisions. |
| Connection selectors (#106) | `Faults.connections` carries TCP tuples/incarnations, SYN/FIN/closed state, next-ID/eviction counters, live selections, matched/dropped counts and witness-use marker. Selections are not rediscovered at restore. |
| Timed logs (#105) | UART snapshots retain raw prefixes and index records, seen-byte cursor, pending/coalescing state, caps and failure state. Machine-wide log order and incarnation are retained. Each runtime branch opens its own console/management index sinks and writes the captured prefix before further output. Host descriptors are recreated. |
| Lifecycle (#91) | Complete `Control`/`Faults` include pending calls/plans/replies, relay/session counters, readiness, process/lifecycle state, active handles, boundaries, queued lifecycle effects and logs. Machines retain paused/down flags, incarnation and boot time; the world retains incarnation history, management deliveries and paused ingress counts. Supervisor/relay state lives in RAM. |
| Disk/torn writes (#100) | Complete virtio-blk state includes transport counters/queues, fault model, immutable base and stable/volatile overlays. Main's parameterized survival threshold and ascending sector draw order are preserved. Selection is atomic at crash, so no partially consumed survival cursor needs capture. A down snapshot already holds selected survivors; forks cannot resample its earlier crash. |
| Network inspection | Physical link counts and `NetworkCounters` totals, pending nominal deltas and frontier are cloned, alongside packets, partitions and event ordering counters. |
| Named volumes (#96) | Filesystem/page-cache state is in RAM; retained block state is in virtio-blk. Mount declarations/placement are immutable inputs. Formatting/mounting is not rerun. |

Fault plans, sampled deadlines, arrivals and control requests remain captured;
`arm_seed` is never rerun for a runtime branch. New incarnations created after
branching use branch-derived entropy/timer streams. A down incarnation already
constructed before T retains its sampled boot entropy; its disk and future
reboot action are captured. Host leases, pause/resume socket and watchdog
handles are not world semantics. They remain owned by the
executing process or are recreated at restore. Page-hash files (#121) are
observation artifacts generated from restored RAM at report time. Offline
readers, parallel exploration workers and host diagnostics add no state to a
single captured world; public checkpoint flags stay `run`-only.

The low-level engine supports multiple held checkpoints and N forks:

```sh
seedbox-vmm --config WORLD.json --seed 7 --checkpoint-at-ns 1000000000,2000000000 \
  --fork-seeds 11,12 --out OUTPUT --digest
```

This runs branches 7, 11 and 12 at each T, producing six reports/directories
under `OUTPUT/checkpoint-T/seed-B/`. A single (T, B) writes directly under
`OUTPUT`, matching the public retained-run layout. Each prefix starts from the
same origin, so an earlier hard stop cannot perturb a later checkpoint. Restores
use fresh VMs/full RAM copies. RAM cost is approximately whole-world RAM per held checkpoint, plus
an origin when several times are requested. Times must increase and precede
duration. A world ending before T fails checkpoint creation explicitly.
Runtime checkpoints cannot mix with `boot` forks or trace output. Console/index
streams are reopened with captured prefixes. Output directories contain logs and
reports, not resumable RAM snapshots.

Event-triggered capture, serialized snapshots, cross-process/host resume,
public multi-branch manifests and application PRNG reseeding remain future work.
Ignored KVM tests cover nonmonotonic restore, repeated branches, B=P, parked
boot guard, message/committed-byte draining, matcher/selector state, scheduled
trigger effects, PV page/speed, timed-log cursor/order, lifecycle calls, disk
survivors, branch streams and getrandom/urandom divergence. Portable tests
check shared identity, ABI layout, replay settings and feature-off link/disk/RNG
results and witness traces generated from main.

## Disk crash policy

`x-seedbox.disk.torn_writes: true` enables torn writes (off by default). Off retains the
`flush-stable/drop-all-unflushed/v1` model: crashes drop all unflushed sectors.
Off adds no world JSON field and leaves the existing disk digest and witness
inputs unchanged.

The profile records the choice as `torn_writes: true|false`; each disk report
carries the numeric policy (512-byte sectors, survival probability 0.5) under
`disk.fault_model`. Replay restores this policy from the retained profile; there
is no replay override or environment variable. The low-level world JSON uses a top-level
`disk_loss` object, for example
`{"sector_bytes": 512, "survival_probability": 0.5}`. Unknown or non-object
parameters are rejected.

At each crash, the latest unflushed version of each sector independently has a
50% chance of surviving. The atomic unit is 512 bytes: multi-sector writes can
survive as any subset, but a single sector's bytes are never torn. A later sector
write can survive while an earlier one is lost. Overwritten historical versions
are not candidates; there is no write log. Lost sectors fall back to their last
flushed contents or the base image. Every completed FLUSH stabilizes all preceding
completed writes, so it never loses coverage under this policy.

Selection visits volatile sectors in ascending sector order, testing the unit
interval draw against the recorded survival probability (main's implementation),
from the versioned SHA-256 RNG domain `disk-loss/<machine>/<incarnation>` and the
run seed. Changed-seed runtime checkpoints prefix that domain with
`checkpoint/<branch-identity>/`; plain continuations keep the original domain.
Incarnation identifies the machine that crashed (initially 0). No other RNG
stream is consumed. Survivors become stable in the next incarnation;
snapshot/fork restore carries the policy and both disk layers. Reports count
only actually lost volatile sectors in `disk_lost_sectors`.

This policy applies to retained virtio-blk disks, including the disks backing
Compose named volumes. Unmounted application paths remain RAM-backed. Low-level
disk fixtures, including the ext4 fsync ledger, are documented in
[`harness/worlds/README.md`](../harness/worlds/README.md#retained-disk).

## Build Seedbox and guest assets

```sh
nix develop
python3 tools/build.py
```

The complete build resolves current guest inputs through Nix, then embeds the
exact helper archive in the native CLI. On Mac the guest build uses a Linux
container with a persistent Nix store. Each checkout publishes its own complete
`build/current` directory atomically; failed builds leave it unchanged.
Run `build/current/seedbox` directly. The kernel lives beside that executable.
Nix and Cargo are never invoked by `run` or `replay`.
See [build outputs and explicit overrides](../tools/runtime/README.md).

## Preparation, execution, and replay

Preparation uses Bollard and the Docker-compatible engine API for registry/local
images and Compose `build:` contexts. The engine owns build caching.
Seedbox exports the image layers and merges them locally with
`ocirender`, retaining the resolved filesystem and launch configuration. No
container is created, so engine-injected files never enter the guest filesystem.
The retained image
manifest describes this flattened filesystem snapshot, not the original registry
layers. Replay uses the snapshot and never contacts the engine.
The content store records which snapshot each engine produced for an
immutable image ID, so a later preparation by the same Seedbox engine reuses
that snapshot instead of exporting and merging the image again. Builds and
pulls still go through the engine. A record whose objects are missing is
ignored; the image is exported again.

Exported Linux ownership, modes, links and case-sensitive paths are processed as
archive metadata without extracting guest paths on the host. Unsupported extended
attributes, special files and metadata outside the boot format fail explicitly.

Apple Silicon uses the native HVF application backend. Linux x86-64 uses KVM.
Both consume embedded guest helpers, a supplied kernel and engine-prepared images. Replay uses
retained bytes without an image registry, build tools or container engine.

Mac profiles retain their own executable descriptor, ARM boot placement,
PV entropy/timer policy, guest-kernel hash and HVF CPU/OS observations.
Full-state capture includes native CPU/SIMD/GIC state and application devices.
Reports must match the actual application inputs and supported guest shape.
HVF CPU/OS observations are diagnostic data; missing or changed observations
and old qualification markers do not block replay. Live execution still probes
the HVF capabilities it needs.
Connection discovery/selectors and committed console triggers use the shared
coordinator on Mac. Console matching observes future bytes at their first
committed stop. Both backends retain committed-time console and management
indexes for `seedbox retained-logs`, including time filters and bundle transfer. Per-page
RAM evidence remains Linux-only; Mac retains whole-state hashes. Full arbitrary ARM
snapshot/restore and release from a deterministic time park remain unsupported.

This remains unqualified development evidence. ARM images, 64..3072 MiB per
guest and one exploration worker are required; core options apply to Linux.
The supervisor preserves the image and Compose environment without adding
application-specific defaults such as `GOMAXPROCS`. New ARM
profiles park initial guests at the shared launch barrier, then release them
at seeded per-name offsets capped by `min(1 ms, network lookahead)`. The cap
keeps causal packets and RPC triggers from preceding the latest release, and
the phase stays within the existing absolute simulated-time limit. The numeric
`arm_launch_max_skew_ns` cap is retained as data and defaults to 1 ms for new
runs; replay clips it to network lookahead. `initial_launch` records the release
evidence. Profiles without a numeric cap keep their earlier release behavior.

After the release build, run as your current user with KVM device access on
Linux:

```sh
seedbox=./build/current/seedbox

"$seedbox" prepare examples/redis --output prepared.json
"$seedbox" run examples/redis
"$seedbox" describe simulation SIMULATION_ID --json
"$seedbox" replay SIMULATION_ID
```

`seedbox help run` lists submission options. Runtime assets are resolved by
the server; configure them at startup with `seedbox serve`. `prepare` also
accepts explicit runtime inputs for offline preparation. On Linux, explicit
runtime input flags and their `SEEDBOX_*` path overrides take precedence.
The default kernel is `kernel` beside the executable; complete builds embed
helpers. Explicit `--kernel` / `SEEDBOX_KERNEL` and `--runtime-assets` /
`SEEDBOX_RUNTIME_ASSETS` select other inputs. `SEEDBOX_ASSET_DIR` explicitly
selects the legacy `ARCH/kernel` and `ARCH/runtime.tar.gz` layout. Missing assets
report the build command. Runtime commands never build or download them.

`doctor` uses only the selected kernel, without extracting helpers or creating
run state. Replay uses the retained executable, kernel and boot archives; it
never substitutes files from the build directory.

The VMM defaults to the engine linked into `seedbox`; `serve --vmm` explicitly selects
the legacy standalone contract. A missing input fails before preparation starts
and names every location searched.

Linux execution performs host admission before OCI work: an x86-64 CPU with
the KVM capabilities the engine needs (see
[host requirements](cli-reference.md#host-requirements)). CPU model, microcode, CPUID,
engine, guest-kernel and host-kernel bytes are not pinned. Temporary core
reservations and affinity are released before preparation on success/error;
cleanup failure refuses admission. The actual leased core/siblings are rechecked
before the VMM spawn. Raw static/leased observations are retained as
Seedbox-owned output.

Inspect, export and import keep old manifests unchanged. Run identities carry no
host-policy proof; `host_policy*` fields in older records are kept as data and do
not block replay.

The server executor leases the first free core, in ascending order,
among the CPUs the process may run on, skipping CPU 0's core. CPU allocation is
server-owned; submissions have no `--cpu` option. `doctor --cpu N` probes an
explicit core and fails if another run holds it. The executor reserves
the chosen core and all SMT siblings with the same `/tmp/seedbox-msr-cpu-N.lock` flock
protocol as `harness/batch.py`, pins the VMM, and releases the lease only after
reaping it.
Library cancellation and CLI SIGINT/SIGTERM/SIGHUP cause cleanup through the same
path. SIGKILL and host loss cannot guarantee cleanup.

## Playback control

Use `seedbox pause ID`, `seedbox resume ID`, `seedbox step ID`, or
`seedbox rate ID 1/10` to control a server-owned simulation. `rate ID 0` pauses;
`rate ID unlimited` resumes without pacing. Step admits one committed round and
requires pause. Requests return durable receipts and continue if the client
closes. Use the printed request ID with `--request-id` to retry safely, or read
it with `seedbox playback ID REQUEST_ID`. An acknowledgement identifies where a
command was processed; a step can still be in progress afterward. `playback ID`
and the `playback` field of `describe` expose the latest sampled worker state and
its age. Requested rate and the measured simulated/wall interval are separate.
Reads never send worker commands or change the sampling cadence. Resource watch
refreshes live timings and sampled progress without catalogue writes.

## Retained host pause

The following interface applies to legacy retained attempts. Use server playback
commands above for current simulations. From a second terminal, pause or resume
the host's entire executing world:

```sh
seedbox retained-pause .seedbox/runs/ATTEMPT
seedbox retained-resume .seedbox/runs/ATTEMPT
```

The attempt directory exists before execution finishes. Find live attempts with
`find .seedbox/runs -path '*/output/host-control'`; remove the `/output/host-control`
suffix to get RUN. A replay creates a new attempt: control that directory, not
the completed source run. Use the same user that started the execution.
`PAUSED: RUN` means the VMM acknowledged a slice boundary; `RESUMED: RUN` means
it released the host gate. Requests are idempotent and concurrent requests take
effect in receive order. A request waits at most five seconds for acknowledgement;
a timeout does not retract a queued request, so send resume if its state is uncertain.
During initial machine construction, acknowledgement waits until the first boundary.

The executor creates a private short-path Unix datagram endpoint under `/tmp` and
writes its locator to `RUN/output/host-control`. It passes the socket path explicitly
as `--host-control` to the retained VMM. The VMM receives and acknowledges requests
only between world slices, before any scheduling, device processing or guest entry.
While paused it polls for resume every 10 ms. It never changes guest state, clocks,
RNG, pending I/O, event order, slice count, control transcripts or witness inputs.
Elapsed host time and performance measurements can change. This is separate from
Compose's in-guest fault/control API and never pauses just one simulated machine.

The executor continues checking cancellation while paused. Intentional host
pause and pacing waits do not consume `wall_time`; guest execution and other
worker work do. A private process-shared monotonic clock accounts for an ongoing
wait without advancing the guest or polling the control socket. Resuming restores
the remaining budget. Retained older workers that do not publish wait accounting
keep their original absolute wall-time behavior. CPU/SMT leases remain held
throughout a pause; cleanup reaps the worker before releasing them.

Portable tests exercise the socket gate and control acknowledgements. Native
pause/resume acceptance uses the current server API: it creates a full-state
simulation, pauses and resumes a rerun using its ID, then compares the outcome,
witness and full-state digests:

```sh
nix develop --command bash harness/check.sh linux-pause
```

The harness builds current guest inputs, supplies a private server state directory
and handles root execution. It requires Linux KVM and a Docker-compatible engine.
The server allocates CPUs automatically. Source-test success alone does not
establish hardware witness invariance.

## Execution settings

Seedbox uses PV time with automatic application instrumentation, including
shared libraries and JIT code. See the [determinism catalogue](determinism-catalogue.md)
for the mechanisms and limits, and [progress integration and evidence](progress.md)
for packaging and regression coverage.

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
The VMM report includes `machines[].pio_total`: all KVM PIO exits across incarnations, including
`0x52c` yields, outside witnesses. The existing `pio` count
excludes yields.
The profile records `page_timer: true|false`. Replay restores it without an override.
Timer jitter uses the existing seeded stream. Enabling it changes run timing
and witnesses.
Checkpoint parent identities also bind the page timer only when enabled,
preventing checkpoint reuse across timer modes while preserving feature-off hashes.
This composes with deferred TX's `+deferred-tx` identity suffix: all four option
combinations have distinct parent and branch identities, and both off retains
the original encoding.

`serve --state` chooses server storage, defaulting to `$XDG_STATE_HOME/seedbox`
or `$HOME/.local/state/seedbox`. `SEEDBOX_STATE` overrides that default. Offline
`prepare`, `export` and `import` use `--state` with default `.seedbox`.
`x-seedbox.machine.memory_mb` defaults to 1024 MiB per service and is a semantic parameter; `serve --full-state` requests RAM/register
digests and retains per-page RAM hashes (format below). Preparation checks a conservative archive boot-memory floor before acquiring
a CPU lease. Application memory can require a larger value; the etcd image failed
to unpack at 512 MiB and passed at 1024 MiB.
The server owns the Linux watchdog interval, recorded per attempt outside
semantic identity. There is no watchdog submission option. The watchdog only
interrupts a run that makes no exit, so the engine can report on stderr where
the guest is stuck; it never advances guest time. Stops land only at natural VM
exits; the engine requests no syscall yield exits.

Charged timer-port accesses conserve their full 1000 ns cost across world and
timer boundaries. Their device effect and read response occur once; the guest
cannot resume while a charged operation remains pending. Due timers latch before
the next portion of the cost is paid. A duration cutoff can retain the pending
operation, including its remaining cost and transferred bytes, in the exit
witness and the full-state `pending_io` part. Other clock advances retain the
strict hard-stop check.

Engines and guest kernels are not pinned: run, explore, replay and doctor admit
any built VMM and guest kernel, and each run records their hashes in its
identity.

Replay uses the retained engine, kernel and boot archives. It accepts a
simulation ID and has no engine override. A comparison across engine changes
requires a fresh simulation followed by `seedbox compare BASELINE_ID SIMULATION_ID`.
Combined executables and legacy standalone workers both read the JSON launch
request, `output/worker-request.json`; retained inputs preserve that contract.
Replay does not require registry access, a builder, or the original Compose files.

Run exits are 0 for PASS, 1 for FAIL, 2 for ERROR, 3 for INCOMPLETE, and 124 for
simulated TIMEOUT. Replay exits are 0 for MATCH (including a reproduced FAIL),
1 for MISMATCH, and 2 for NOT_COMPARABLE, including a cancelled or incomplete
replay. Results print the simulation ID.
Offline diff exits 0 for MATCH, 1 for DIFFERENT, and 2 for usage/setup or invalid evidence.

## Compare retained runs

```sh
seedbox diff .seedbox/runs/ATTEMPT .seedbox/runs/REPLAY
seedbox diff .seedbox/runs/ATTEMPT .seedbox/runs/REPLAY --json
```

Both positional inputs are retained run directories, including directories printed
by `seedbox import`. Import exported bundles first. Diff runs on macOS or Linux,
requires no root, runtime inputs, content store, host admission or registry access,
and creates no state. Manifests are verified with the existing v2 reader. Selected
materialized evidence under `output/` must match the manifest's recorded digest;
missing declared files, corrupt bytes and unsupported reports produce setup exit 2.
Unregistered and policy-omitted files are never interpreted, even if left on disk.

Plain output starts with exactly one `MATCH: RUN_A <> RUN_B` or `DIFFERENT: ...`
line, followed by lowercase fields. `--json` prints exactly one pretty library
`DiffReport`. MATCH exits 0; DIFFERENT exits 1. This is evidence comparison, not
replay admission: equal incomplete records can MATCH while witness equality is
unknown (`matches: null`). Differing evidence availability is DIFFERENT. Omissions
and unavailable detail are listed in `limitations`.

The report presents the world witness first, then machines by name with exit
witnesses, optional full-state digests/parts and selected terminal fields. The
manifest's full-state digests remain usable when the raw engine report was omitted.
State parts such as `ram`, `disk` and `pending_io` are compared separately;
`events` is the engine's descriptive event state, while the other parts are
validated lowercase SHA-256 digests.

With server startup `--full-state`, the VMM also writes `output/machines/NAME.pages` for each machine.
Retention records the files as `logs["machines/NAME.pages"]` for diff and replay.
The binary format has no header: entry PFN `p` occupies bytes `8*p..8*p+8` and is
a little-endian `u64`, formed by interpreting the first eight SHA-256 bytes of
guest RAM bytes `4096*p..4096*p+4096` as little-endian. Entries cover all guest
RAM in ascending PFN order, including zero pages; 512 MiB RAM produces exactly
1 MiB of hashes. These are truncated hashes, not RAM snapshots, and collisions
can hide a page difference. Capture reads RAM without changing it or consuming
random draws. It adds no witness or state-digest input. The former env-only
page-capture switch has been removed; no new flag or environment setting is needed.

Page files follow the ordinary log retention policy and travel in bundles when
retained. They are excluded from run identity and never decide the replay verdict
or offline diff status. When a machine's full-state digest differs and `ram` is
the only differing state part, diff adds `machines[].ram_pages` with
`differing_pfns` and `first_pfns` (up to eight, ascending). Plain diff prints the
summary on stderr. `diff --json` emits one document and no page-summary stderr.
Server `compare` and `replay` return aggregate witness and full-state equality;
they do not expose page diagnostics.

Both files must be manifest-declared, checksum-valid and exactly eight bytes per
recorded 4 KiB page. Missing or omitted evidence cannot locate PFNs and is reported
as a limitation. Invalid selected page evidence makes offline diff exit 2.
Old engines' runs remain readable without these files. PFNs locate final RAM differences, not the first divergent instruction.

All changed identity fields are listed as JSON pointers with both values, including
configuration, seed, engine, kernel, images/layers, guest archives, provenance and
semantic parameters. Missing values are null. Arrays are compared as whole values.
Execution outcome, report, errors and omission metadata are also compared.
Attempt names, replay ancestry, performance parameters, engine performance statistics,
host timings and arbitrary console/log contents do not determine MATCH.

Every x86 run writes `output/machines/NAME.witness-log`: periodic samples of the
machine's running exit witness. The first witness record at or after each 1 ms
boundary of the machine's simulated time is sampled, plus a final sample at the
end, so idle intervals add nothing. Each entry is 24 bytes of little-endian `u64`:
`time_ns`, `records` (witness records so far, one per trace line) and the first
eight bytes of the running witness. Sampling reads the witness without
feeding it, so witnesses, state digests and replay verdicts are unchanged.
Snapshots, forks, checkpoints and crash restarts carry the log with the witness.
The log follows the ordinary retention policy and is excluded from run identity.

When both runs retain witness logs, diff compares them entry by entry and reports
the earliest differing interval across machines as `divergence`: the machine,
the one-based `sample`, the `last_match` sample (null when the first differs) and
each side's sample (null when that log ended first). The first differing exit
lies after `last_match` and at or before the earlier differing sample, by records
and simulated time. A differing log makes the comparison DIFFERENT. A log on only
one side is an evidence difference; none on either side (older engines, ARM) is a
limitation. Plain diff prints `divergence: <json>` and a readable summary on
stderr. Server `compare` and `replay` return aggregate comparison fields,
without this offline divergence report.

When both runs retain `machines/NAME.trace`, diff compares the existing engine
records (`kind time_ns extra(hex) rip(hex)`) in order for each machine.
An inserted/deleted record is reported at the first positional mismatch, including
EOF on either side; there is no guessed realignment. Of those first mismatches,
the lowest available simulated time on either side wins, with machine name as a
stable tie-breaker. Each side retains its own time, kind and values; `record` is
one-based. Traces are streamed and fully checked, including after the first mismatch.
Trace lines are limited to 64 KiB and engine report metadata to 256 MiB.

The production adapter does not enable tracing. Without a differing trace record,
`first_record` reports the earliest differing terminal observation available in
`vmm.json`, with source/kind `terminal`, machine and each side's time. This includes
state-part differences and their before/after values at the terminal observation.
Aggregate witnesses alone give no exit/time localization, so `first_record` can be null even when
witnesses differ. Terminal time is only when the differing final state was observed;
it is not a claim about the first divergent instruction or exit. Full-state digests
are hashes, not snapshots. No new execution evidence is collected by diff.

## Check the execution profile

On the supported Linux KVM host, as root:

```sh
seedbox doctor
seedbox doctor --cpu 15 --json
```

Doctor resolves the ordinary engine/kernel defaults without building missing
inputs. It hashes those inputs and invokes the same bounded host probe used by
execution admission. It opens KVM, reserves the selected core and its siblings,
and verifies restoration of temporary affinity and release of all reservations. Any uncertainty prevents `VALID` or success
JSON. The report names the actual selected CPU and siblings and preserves the
exact observations admitted by that probe; there is no second read or cached
host inventory. No content store, run directory, image preparation or guest is
created. Missing inputs, unsupported capabilities, busy leases and profile drift
retain the same actionable errors as execution; no repair or fallback occurs.

Exit 0 means the reported policy is compatible. It does not prove repeatability
on an unqualified machine. Plain output has one `VALID` status line and lowercase
fields; `--json` emits one pretty JSON document on success. Errors go to stderr
with exit 2 in either mode. Actual Linux doctor acceptance remains dependent on
issue #5's hardware qualification; Darwin source tests do not establish it.

## Guest semantics and limits

Each service currently gets one machine and one OCI-root chroot. Its complete
filesystem is writable RAM-backed initramfs storage, freshly instantiated for
each run/replay; image volume paths have the same per-run lifetime. Process
stop/start retains that filesystem. A machine crash loses it.

Named Compose volumes are the exception. A machine whose service mounts one gets
a 1 GiB virtio-blk disk, zeroed at the start of each run/replay. The supervisor
formats it as ext4 on first boot (with a static `mke2fs` packaged into the guest),
mounts it, and bind mounts each volume's directory onto its target. The disk has a
write cache: data the application flushed (fsync, fdatasync, `O_DSYNC`) survives a
machine crash and reboot, and every write since the last flush is lost. A new volume
takes the mode and owner of the image's target directory and starts empty.
The generic static supervisor is a guest bootstrap alongside the existing C guest
helpers; configuration, preparation, orchestration, and execution are Rust.

Names map to stable `10.0.0.N` addresses in each guest's `/etc/hosts`. The image
supplies application dependencies, argv, environment, working directory and user.
The supervisor sets credentials and executes the argv directly. Config files are
root-owned mode 0444 bind mounts remounted read-only. `/dev`, `/proc`, `/sys` and
`/etc/hosts` are reserved runtime paths. The backend supplies no host mounts,
external network, container daemon, or host filesystem persistence to the guest.

The supervisor defaults `GOMAXPROCS` to the guest's online CPU count (currently
one vCPU per machine), unless the merged image/Compose environment already
contains it, including an empty value. Process stop/start reuses that environment.
Bundled Go workloads use this default instead of fixed CPU counts. This changes
their execution and witnesses; the supervisor bytes are part of run identity.

Image modes, UID/GID, symlinks and hard links are retained in newc archives.
Device/FIFO entries, Linux extended attributes (including file capabilities/ACLs),
files exceeding newc's 4 GiB limit, and unsupported image lifecycle metadata fail
preparation explicitly. Unknown users, missing executables or shared libraries,
and unsupported guest syscalls are compatibility failures, never workload PASS.

`output/world.json` is generated execution input, not user-authored configuration.
`output/worker-request.json` is the x86 engine's launch request: the world path,
seed, output limits, the recorded page-timer and deferred-TX features, and the
server-owned watchdog performance setting.
`output/vmm.json` contains the engine report; `output/vmm.stderr` and streamed
`output/machines/*.console` / `*.mgmt` files preserve diagnostics on cancellation
and timeouts. Serial output is capped at 1 GiB total divided equally between the
two ports of all machines; exceeding that cap fails execution. Trace recording
is not enabled by this adapter.

Each machine's `net` report includes negotiated `event_idx`, `doorbell_exits`
(virtio-net MMIO queue-notify writes), and `suppressed_interrupts` (completion
batches that did not request an interrupt). These diagnostics are outside the
witness. With EVENT_IDX negotiated, the device publishes `avail_event` after
consuming buffers and samples `used_event` for completion interrupts. It polls
available buffers at deterministic guest stops, including slice entry after RX
delivery, slice boundaries and halt, so suppressed kicks do not stall queues.
Without negotiation, the existing NO_INTERRUPT/NO_NOTIFY path is retained.

Deferred TX is an internal KVM mode, disabled for new simulations. The raw
VMM accepts `--deferred-tx` for engine fixtures. This is a semantic option: the profile records
`deferred_tx: true|false`, and enabling it changes packet timing and run identity.
Off retains the behavior at `13b7f85`, including legacy
kick-driven TX. Replay uses the retained setting with no override, including
when changing engines. The engine report declares the setting; an older report
without it means off. This change builds a new engine hash in either mode.

While deferred, queue 1 publishes `avail_event=last_avail+queue_size` with
EVENT_IDX (wrapping u16 arithmetic), or retains `VRING_USED_F_NO_NOTIFY` without
it, including when the queue is empty. A doorbell that still arrives is serviced.
RX retains its previous negotiation, polling and notification behavior. TX
completes at deterministic vCPU entry/natural exits, slice boundaries, charged
I/O boundaries and halt. Deferred slices are capped at 50 microseconds of
simulated time, within the existing world/timer bounds. No RNG draws are added.

Linux 6.12 virtnet's `free_old_xmit` stops when `virtqueue_get_buf` returns no
completed buffer; TX callback recheck loops drain already completed work. Ring
exhaustion and BQL stop the queue, and completion/NAPI wakes it rather than
spinning for a new used entry. Clock/timer progress therefore reaches the bounded
poll, and halt is polled before parking. Host watchdog interrupts never
service TX. A completion spin that makes no exits at all is not bounded by
simulated time; the kernel's own polling loops exit through its clock-read
hooks, and this option does not promise to terminate a guest program that
never exits. No control virtqueue is advertised.

The per-machine `net` diagnostics additionally report `tx_polled` (packets
picked up by polls), `tx_polled_at_boundary` (slice/charged-I/O/halt/proven-idle
polls), and `tx_pickup_samples`, `tx_pickup_delay_upper_ns_mean` and
`tx_pickup_delay_upper_ns_max`. The delay is an **upper bound**, measured from
the last deterministic observation of an empty TX ring to pickup, weighted per
packet. The untrapped guest avail-index store lies inside that interval; its
exact simulated timestamp is unavailable. Samples without an earlier empty
observation are omitted. These values measure polling latency conservatively
and must not be presented as exact publish timestamps. Counters and timing
observations are outside the witness; raw `output/vmm.json` retains them for the
final machine and each crashed incarnation.

Focused host tests (after `cargo build-vmm` and `cargo test-vmm`) are
`cargo test-vmm --features kvm-tests event_idx_polls_on_entry_slice_boundary_and_halt_without_kicks`
and `cargo test-vmm --features kvm-tests deferred_tx_completes_a_pv_spin_without_halt_kick_or_interrupts`.
The latter compares witnesses/state across two watchdog periods. The 60 s kill
world, seed 4000 needs separate option-off and option-on runs and
same-mode replays on a Linux KVM host. Compare doorbells, host time and the pickup
delay bounds in the engine reports. Option-on host results are pending.


For legacy retained attempts, when `inspect` reports
`execution: VMM failed; see vmm.stderr`, it also prints up to five non-empty lines from the final 8 KiB
of the attempt's `output/vmm.stderr` to stderr. Each line starts with `vmm stderr:`;
terminal control characters are removed and invalid UTF-8 is replaced. A line
cut by the byte limit starts with `...`. Missing or unreadable logs are skipped
without changing the outcome. This excerpt is local diagnostic output only:
retained records, serde field names, status lines and `--json` output are unchanged.
`inspect` keeps the recorded `error:` line on stdout alongside the run status.

## Reproduce acceptance

From the repository root in `nix develop`, name the suites to run:

```sh
bash harness/check.sh linux-apps linux-oci   # Linux x86-64 KVM host
bash harness/check.sh mac-apps               # Apple Silicon
```

See [the test matrix](testing.md) for suites, Cargo features and inputs.
The older focused runtime diagnostics remain available:

```sh
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU FRESH_OUTPUT_DIRECTORY
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU ANOTHER_FRESH_DIRECTORY lifecycle
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU FAULT_OUTPUT_DIRECTORY faults
```

The OCI suite builds/tests as the current user and invokes only its final import
executable with `sudo -n`. It verifies the prebuilt image metadata,
whiteouts, symlinks and build-context filtering. It does not start guests.
The default runtime suite runs both application examples and their intentional
failure controls, then replays each. The lifecycle suite checks full-state and
changed-watchdog replay, simulated timeout, wall expiry and SIGTERM cancellation.
The `faults` mode uses a Python controller and synthetic key/value services to verify acknowledged-write
retention across process restarts, overlapping partition healing, and retained
replay. Its deliberately forgetful variant must fail after restart and pass
without it. Missing and rejected effect acknowledgments must produce ERROR.
All runtime modes start KVM guests. Choose available CPU/SMT lanes; contention fails without taking over another run's lease.

The guest fault RPC uses the same preparation, execution and retained replay
path; replay executes the retained Python controller again. See [fault operations](../crates/seedbox/README.md#structured-faults) and the
[recovery harness](../examples/faults/README.md) for the effect and packet contracts.
The legacy fixtures remain available through [the migration map](../harness/worlds/README.md).

### Guest coordination channel

Prepared guests expose the `seedbox` virtio-console port to their supervisor.
This uses shared-memory virtqueues and a dedicated device interrupt, independent
of Ethernet partitions. Python and other bindings use the local Unix socket
specified in [the RPC protocol](../python/PROTOCOL.md); they need no device access.
The simulator records messages at committed guest-device boundaries. Replies
become visible at their recorded simulated time, and device DMA is journaled.
The kernel must include `CONFIG_VIRTIO_CONSOLE`; rebuild it
with `bash kernel/build.sh` when upgrading from the UART-based implementation.

Directed network counters now distinguish NIC acceptance from drops caused by
an unready queue or full receive backlog. This corrects the previous delivered
counter even in runs without faults; compare counters only with that change in
mind when allowing a different engine for replay.

The fault socket exists only on the configured controller. Connections may be
closed and reopened between completed calls; the transcript preserves both
run-wide relay IDs and connection-local request IDs. Redundant lifecycle commands
and commands targeting exited VMs return recoverable RPC errors. The controller
can inspect the application exit and choose PASS or FAIL.

Inspection and partition/shape/heal counter snapshots count nominal send, drop and
arrival times strictly before admission. A packet physically delivered in an
early scheduler window is excluded until its nominal arrival precedes that
boundary. Final report counters still record physical NIC acceptance totals.

### Directed-link policy

`x-seedbox.network.links` accepts a list of directed service pairs:

```yaml
network:
  latency: 1ms
  jitter: 100us
  links:
    - {from: server, to: client, latency: 20ms, jitter: 2ms, loss_ppm: 50000, bandwidth_bps: 1000000, queue_bound: 50ms}
```

Within `x-seedbox`, each pair must be unique and name existing services. An entry
requires `latency`; `jitter` and `loss_ppm` default to zero. Policies are absolute:
a missing jitter or loss does not inherit the global value. Every baseline and
runtime shape requires latency at least the global latency, jitter less than its
latency, latency plus jitter at most one hour, and integer loss in `[0, 1000000]`.
The global latency remains the whole-run lookahead; it cannot be lowered by a shape.

Global policy, directed baselines and shapes also accept optional bandwidth and
queue limits. `bandwidth_bps` is a positive u64 integer in bits per second;
Compose `queue_bound` is a duration and the RPC uses `queue_bound_ns`. The bound
requires finite bandwidth; zero drops every nonempty frame. Absent bandwidth
means infinite bandwidth; an absent bound means no tail-drop limit. These are
absolute settings: a baseline or shape that omits them uses infinite bandwidth
and an unbounded queue, even if the global policy is finite. Resolved settings
are retained in the run's semantic configuration and identity; replay restores
them and offers no override.

`world.shape(from_, to, latency=..., jitter=0, loss_ppm=0,
bandwidth_bps=None, queue_bound=None)` uses nanosecond integers for durations
and returns a healable handle. Its nonempty sides must name distinct machines.
It shapes only `from_ × to`; partitions still cut both directions. Among live
shape layers the highest fault ID wins. Healing removes exactly one handle and
reveals the previous layer, then the baseline, then global latency/jitter with zero
loss where no baseline exists. Any active partition covering a pair dominates
all shapes. Inspect includes both kinds in `active_handles`.

At a boundary T every machine reaches T before control admission, trigger effects
in arm order, and faults in ID order, followed by sends at T or later. Routing fixes each candidate's loss
and arrival at send time; later shaping or healing retains in-flight decisions.
Broadcast expands to other machines in ascending index order. Cuts consume no
randomness; every other candidate consumes one independent per-pair loss draw
then one jitter draw, including zero and total loss. Exact scaled multiplication
maps jitter inclusively into `[0, jitter_ns]` and loss onto its ppm threshold.
A lost frame increments `loss_dropped` and does not move the pair's last arrival.
A delivered frame arrives at `max(send + latency + jitter, last_arrival + 1)`;
a delay decrease preserves FIFO and head-of-line waiting. The scheduler's global
early-delivery window remains unchanged.

For a finite rate, serialization costs
`ceil(frame_bytes * 8 * 1_000_000_000 / bandwidth_bps)` nanoseconds, using the
actual transmitted Ethernet bytes without preamble, FCS or inter-frame gap.
`depart = max(send, last_depart) + serialization`, and arrival uses
`depart + latency + jitter` before the same FIFO clamp. Each directed pair has
its own departure clock; NIC-wide egress sharing is out of scope. A candidate
with `depart - send > queue_bound_ns` is tail-dropped and reserves neither
departure nor arrival. Equality is accepted. Cuts, selected drops and ordinary
loss reserve no bandwidth. A receiver rejection still consumed serialization,
although it retains the existing arrival-clock rollback. Shaping, healing,
snapshot restore and changed-seed forks preserve accepted departures and queued
frames; a faster rate cannot erase an earlier reservation. Timestamp overflow
on the finite-rate path is counted as a queue drop instead of wrapping.

Bandwidth consumes no new RNG draws. The existing loss-then-jitter order remains
exact, including on tail drops. Bandwidth shapes retain `queue_counts` on both
their fault row and heal result: `matched` counts non-cut, non-lost candidates
that reached the winning queue policy, `queue_dropped` counts tail drops, and
`delayed_max_ns` is the maximum proposed send-to-departure delay, including
dropped candidates. Losing layers receive no counts. An unused bandwidth shape
returns explicit zeros. `world.heal(bandwidth_shape)` returns this evidence
object; unlimited shapes retain their existing effect-time return value.

Counters distinguish `delivered`, `partition_dropped`, `receiver_dropped`, and
`loss_dropped`, and `queue_dropped`; loss includes selector drops. Loss and queue
fields are omitted while zero. Queue drops count at send time, under the same
strict-before-admission rule used by inspection and fault snapshots.
The report's `network_witness` hashes in-flight frames, per-link last
arrivals, stream positions, live layers, cuts and baselines. It is folded into
`world_witness` only when the run has baseline overrides, configured finite global bandwidth, applied a shape or used a connection selector, so
a run without link policy keeps its previous world witness. Shape/heal evidence follows the same strict-before-admission
counter boundary as partitions. Fork snapshots retain baselines, last arrivals,
and both RNG streams; arming a run seed reseeds those streams as before.
Finite bandwidth adds a tagged witness extension for its optional parameters,
departure clocks, per-pair and live-layer queue evidence and retained use marker.
With bandwidth absent, no extension bytes, counters or events are added; the
existing feature-off link/network/world witnesses remain unchanged.

Dynamic shapes additionally accept opt-in packet reorder, duplication, burst
loss, latency spikes and TCP reset injection through Python's `world.shape`.
`world.heal` releases each layer and returns `network_fault_counts`; the complete
wire schema, draw order and socket-reset limits are documented in
[the protocol](../python/PROTOCOL.md#dynamic-network-impairments).
Each fault has its own seed-derived stream, sampled only while active. Bursts
retain per-layer/per-direction remaining length. Reorder and spike delays sit
outside the base FIFO clock; duplication adds one receive-side copy a nanosecond
later. Reset replaces an eligible IPv4 TCP frame with a checksummed RST at its
simulated deadline. No host timing enters a decision. Snapshots retain streams,
burst state and already queued deliveries; changed-seed branches reseed only
future samples. A tagged network-fault witness extension is retained after
healing and is absent when no new fault has been used. The existing loss/jitter
streams, ordering and feature-off witnesses are unchanged.

`harness/acceptance/workloads/network-faults.yaml` exercises runtime activation
and healing, checks actual packet receipts and socket effects, and reconnects
after healing. Run and replay it through the public CLI on a Linux KVM host.

Runtime control can also skew a machine's wall clock: `world.clock_skew(target,
offset=..., max_offset=...)` steps that guest's `CLOCK_REALTIME` by a fixed
offset, or by one drawn from `[-max_offset, max_offset]` on the seeded
`clock/<machine>` stream, and returns a healable handle. The guest supervisor
applies the step with `adjtimex(ADJ_SETOFFSET)` and acknowledges it like a
process fault; monotonic time and the virtual TSC never move. The fault row
records the applied `offset_ns`, so offsets are part of the world witness.
Runs without clock skew draw nothing new and keep their witnesses. See
[the protocol](../python/PROTOCOL.md#clock-skew).

`world.clock_drift(target, ppb=..., max_ppb=...)` makes a machine's clock run
fast or slow by a fixed rate error, or one drawn from `[-max_ppb, max_ppb]` on
the same `clock/<machine>` stream, up to 10%. The supervisor sets the kernel's
NTP tick and frequency with `adjtimex`, so realtime, monotonic and boot time
(and the timers on them) drift together while `CLOCK_MONOTONIC_RAW`, the TSC
and simulated time do not. One drift per machine at a time; heal restores the
nominal rate and the fault row records `rate_ppb`. Runs without drift draw
nothing new. See [the protocol](../python/PROTOCOL.md#clock-drift).

The network seed-diversity audit still finds zero default jitter and loss.
`link/<from>/<to>` and `link-loss/<from>/<to>` each advance once per non-cut
candidate, but zero jitter/loss makes their decisions seed-independent for an
identical send trace. An explicit `network.jitter: 100us` at `latency: 1ms`
enables seed variation through the existing jitter stream. Bandwidth and tail
drop depend on bytes and simulated send times, with no extra streams. This PR
keeps the jitter default at zero to preserve feature-off witnesses; any default
change needs a separate semantic-default decision. Reorder, duplicate and burst
profiles will need independent `link-reorder/`, `link-dup/` and `link-burst/`
streams, drawn only when configured after the existing loss/jitter draws.

Every final report includes `network_witness`; runs using link policy fold it into
`world_witness` as described above. Its
versioned, little-endian, length-prefixed encoding hashes pending frame arrival,
sequence, directed indices and SHA-256 of frame bytes; last arrivals; loss/jitter
stream counters; layers ordered by fault ID with handle, pairs and policy; active
cuts; and baselines. Counts delimit collections and strings. It retains bounded
infrastructure state without packet contents or application schemas. This VMM
change creates a new engine identity; hardware/replay qualification is separate
from portable policy tests.

Faults, triggers and control requests that take effect at the same simulated
time T are ordered against sends as follows. The world applies a boundary at T
only once every machine has reached T, so a machine that is still running before
T keeps sending under the earlier network state, even when another machine is already
waiting at T. At T, control admission and effects come first, then trigger
effects in arm ID order (taking the next fault IDs), then faults in ID order,
and only then does any machine run, so every frame sent at T or later sees the
new state. A frame's partition decision and arrival time are
fixed when it is sent; a later partition or heal never changes a frame already
in flight. A slice that runs up to a boundary always ends before the guest can
send at the boundary itself, so no frame is ever stamped at or after a boundary
that hasn't been applied yet.

### Machine pause, crash and reboot

Compose guest drivers can call `World.pause(target)`, `resume(target)`,
`crash(target)` and `reboot(target)` through the [runtime control API](../python/README.md).
Each acts on the target's whole machine at admission and returns an `Effect`;
the transcript and service-attributed fault events are validated during run and
replay. Illegal transitions, and stop or restart of a paused machine, return
recoverable `invalid_state`; pause/resume
of an exited VM and process faults whose machine exits or crashes return
`target_unavailable`. Inspection shows `Paused` and `Down`. After reboot, the
scenario checks application readiness itself. The check service is the
controller, which can't be a target, so a lifecycle operation never ends the run
by itself: a crashed service is not an exited one. See
[Machine pause, crash and reboot](../harness/worlds/README.md#machine-pause-crash-and-reboot)
for CPU, timer, network, incarnation and static-plan semantics.

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

## Exploration preparation

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

## Connection selectors

Guest controllers can discover an IPv4 TCP direction and arm a scoped loss:

```python
page = world.connections("server", "client")
connection = next(c for c in page["connections"] if c["dst_port"] == 4000)
handle = world.drop_connection(connection)
# Run the workload phase that must encounter loss.
evidence = world.heal(handle)
assert evidence["dropped"] > 0
```

Discovery returns at most 32 records per live page in incarnation order. Use
`after=page["next_after"]` for the next page. Selection binds the complete
machine/address/port/direction/incarnation record at the globally committed
admission boundary. A connection that disappears between discovery and admission
is rejected with `invalid_state`. The controller cannot select itself.

The bounded classifier observes candidate Ethernet sends before partition/link
loss decisions. An unfragmented IPv4 TCP SYN without ACK admits both directions
of a tuple and assigns a monotonically increasing run-local incarnation. A SYN
retransmission with the same initial sequence number retains it. A different
initial sequence number replaces it, even without a close. Simultaneous open
joins the same incarnation; SYN-ACK must acknowledge the opposite initial sequence
number plus one (wrapping u32). A SYN-ACK alone or midstream traffic without an
observed opening SYN does not create a connection. A same-ISN opening SYN after
observed closure creates a new incarnation. There is no idle timeout.

FIN half-closes one direction; both observed FINs or either RST close admission.
FIN/RST and subsequent retransmissions still classify under that incarnation until
a replacement SYN. Tracking describes observed sends, including dropped packets,
not a kernel's confirmed TCP state. Process stop/restart does not reset the NIC or
classifier. Restart/reuse without a close and with an identical opening ISN is
indistinguishable from a SYN retransmission and deliberately retains identity.
After replacement, newly sent non-SYN packets on the tuple classify under the
latest incarnation; TCP sequence-window validation is not implemented. Already
routed frames retain their old drop/arrival decisions regardless of later reuse.

Only untagged unicast Ethernet carrying structurally valid IPv4/TCP is classified.
IPv4/TCP options are bounded by their header lengths; checksum validation is left
to the guest (including checksum offload). All IPv4 fragments, including first
fragments with MF, malformed/truncated headers, VLAN, IPv6, ARP, UDP, multicast
and broadcast bypass connection selection and retain ordinary link policies.
There is no reassembly or application-protocol decoding.

At most 1024 bidirectional tuples and 1024 active selector handles are retained.
A new tuple at capacity evicts the oldest unselected closed incarnation first,
then the oldest unselected open incarnation. An incarnation named by a live
selector is never evicted, even after closure; if all retained incarnations are
selected, the new tuple is not tracked. Stale handles keep their counts but cannot
attach to a replacement. Evicted or untracked midstream traffic is unclassified
until a fresh opening SYN can be admitted.
Selector capacity exhaustion is an explicit admission error. Counters and IDs do
not reset on discovery, process restart or healing. Fork snapshots preserve the
classifier and selector state, and reseeding does not reset their identities.

Partitions dominate selected loss, which dominates directed link shapes.
Selected loss drops every matching candidate after consuming exactly one loss
draw and one jitter draw for its pair, without updating the pair's last arrival;
unselected traffic uses the existing link policy and global causal floor. No
additional selector RNG is used, and cuts remain draw-free. Broadcast candidate
ordering remains ascending machine index.
Overlapping handles each retain their matched/dropped counts, and healing only
releases its own handle. Arming is not evidence of injection: require a positive
`dropped` count from heal or retained `selection_counts`. Pending frames are never
reclassified by a fault, healing or tuple reuse.

Once a selector is applied, the network witness includes a tagged encoding of
classifier tuples, incarnation counter, eviction count, SYN/FIN/RST state, active
selectors and their counters. The world witness then includes that network
witness even after healing. Without selector use, tracking contributes no witness
bytes or report fields and leaves legacy RNG, frame decisions and results intact.
This changes the engine identity. Portable tests exercise the production modules;
Linux execution, replay and observation/performance qualification remain separate.
The guest-owned fixture is `harness/acceptance/workloads/connections.yaml`.

## Bundles

`seedbox export RUN [-o FILE] [--state STATE]` writes one uncompressed
`.seedbox.tar` file. Without `-o`, the file is named after the source run directory
in the current directory. `seedbox import FILE [--state STATE]` creates a fresh
run directory and reports its path. Both work without OCI tools or guest execution
on Linux and macOS. Export reads retained objects from the selected state; import
verifies the entire declared closure before publishing a run.

The original manifest bytes, configuration, seed, identity, results and retained
opaque log bytes survive unchanged. The imported directory has a new local name;
its manifest keeps the original attempt metadata. Parent replay attempts are not
recursively included. Witness hashes are retained comparison evidence, not RAM
snapshot files. Capture omissions remain recorded, and their reasons describe the
original attempt: an oversized file left only in that attempt's output directory
is not bundled. This feature does not collect new guest files.

The v1 container accepts the existing v2 run manifest, its identity object, all
prepared-input objects and recorded log objects. Missing/corrupt objects,
unsupported versions, links/special entries, unsafe names and extra/truncated data
are rejected. Import rejects symlinked state roots and object/run directory
components and never repairs a corrupt existing object. Existing destinations
are never replaced. Import performs no installation or execution.

`--max-bytes` defaults to 34359738368 (32 GiB). It independently caps unique-object
bytes and the total bytes materialized across log paths; multiple names for one
object each count toward the latter. Other limits are 8,192 objects, 8,192 log
paths, 8 MiB each for container metadata and manifest, and 4 KiB per log name.
Exceeding a limit fails instead of truncating bytes. Staging can require up to
twice the unique-object bytes plus the materialized logs on disk. Failed or
cancelled transfers publish no partial run; valid unreferenced objects may remain
if final publication fails. A force-killed importer may leave private `.bundle-*`
staging directories for manual removal after confirming no importer owns them.

Inspect needs no original checkout or registry. Import creates an offline run
directory; importing that directory into the simulation server catalogue is not
implemented. It cannot be passed to `replay ID`. Exporting engine bytes does not
establish cross-host or cross-backend compatibility; missing KVM capabilities
are refused by host admission. Server replay uses its own retained simulation
inputs and preserves PASS/FAIL, ERROR, INCOMPLETE, TIMEOUT and MATCH/MISMATCH meanings.

## Committed-time logs

`seedbox retained-logs RUN --machine NAME --stream console --since-ns 1000000000
--until-ns 2000000000 --json` filters retained chunks in the half-open simulated
nanosecond interval. All filters are optional; the run directory is positional.
Consecutive bytes from the same machine/incarnation/stream coalesce within one
world slice until another stream writes. Each chunk retains its first byte's
commit time; filters select the whole chunk by that time, including later bytes
that may commit beyond the upper bound.
Without `--json`, stdout contains only the selected raw bytes in chunk order,
with no prefixes, encoding changes or added newlines. Lowercase `logs:` status
and `omission:` diagnostics go to stderr. Use `--machine` and `--stream` to
recover one stream's indexed bytes exactly. `--json` includes chunk metadata and
lossless hexadecimal payloads as `bytes_hex` in one report.
Success exits 0, including explicitly incomplete capture; invalid inputs or
corrupt evidence exit 2. No runtime inputs, images, host tools or KVM are needed.

The library exposes `logs::Reader`, `Filter`, chunk metadata, omissions, and
streaming byte access. Only manifest-declared, checksum-verified evidence is
used. An absent index never causes host receipt order or application timestamps
to be substituted. Filtering cannot change execution or witness identity.
Indexes share the existing retention limits and bundle transfer path; missing
raw streams and indexes stay explicit after import.

## Committed console triggers

The development engine implements bounded, exactly-once console marker triggers
through the guest control API. Arm/cancel admission, match deadlines, future
causal effect barriers, snapshot state and witness evidence are specified in
[the trigger contract](triggers.md). No trigger ever armed adds no report fields,
witness bytes or console stopping points. This changes the engine.

## Guest wall clock and user namespaces

Preparation records one wall_clock_epoch after image builds/imports finish.
The SBXLAUNCH4 recipe carries it to the supervisor, which sets CLOCK_REALTIME
before the launch barrier on x86 and ARM64. Replay uses the retained value.
`x-seedbox.wall_clock_epoch` in the Compose file sets it: an integer number of
Unix seconds in [86400, 4294967295], because Linux cannot set a wall clock
earlier than the guest's uptime; any other value is a
`$.x-seedbox.wall_clock_epoch` validation error. Otherwise it is the latest
RFC 3339 `created` time in any prepared image's config or history, and at least
86400. Builders stamp those times after each step, so a certificate minted
during a build is already valid. Preparation never samples the host clock, so
independent fresh runs of the same images start at the same guest Unix time and
can be compared directly; rebuilt images can change the epoch and witnesses.
The epoch is a simulation parameter, so it has no CLI flag or environment
override, and it does not alter image-layer timestamps. Replay always uses the
retained epoch.
Both application kernels support unprivileged user namespaces. The supervisor
recursively mounts the image at the namespace root before entering it, preserving
named volumes and allowing CLONE_NEWUSER.

Live engine events include `machineReady` and `machineExited`, attributed to a
machine, incarnation, and simulated time. They describe committed observations;
the final oracle report still determines the test verdict. `sourceGap` describes
missing worker datagrams, which may have contained logs or events, and
`sourceUnavailable` marks an uncertain tail or malformed observation. Event
stream delivery gaps are separate from these worker transport gaps.

`machineLifecycle` events describe successfully committed pause, resume, crash,
reboot, and throttling transitions. Crash identifies the ended incarnation;
reboot identifies the new one. Throttling includes its `factorPermille` value.
These machine transitions are distinct from host playback pause/resume controls.

`faultChanged` events carry a fault ID, action name, optional target machine, committed status, and an optional
`appliedTimeNs`. Statuses are `unrequested` (scheduled), `requested` (awaiting a
guest effect), `applied`, `failed`, or `unavailable`. Immediate transitions at one
boundary are coalesced. The event timestamp is the observing committed boundary;
`appliedTimeNs` identifies the actual effect time when available. Fault status
never supplies or overrides the workload oracle's verdict.

If a target VM exits while a fault is waiting for its guest effect, the fault
becomes unavailable under the same deadline rules whether it came from a static
plan, a trigger, or a runtime request. An already observed effect is preserved.

Preparation has a server-owned 15-minute deadline, including backend resolution
and image preparation. It uses an owned cancellation token, so expiry is reported
as `preparationTimeLimit` rather than a user stop. The server awaits cleanup and
joins its deadline timer before handing off to execution. Cancellation is
cooperative: container streams check it while waiting; synchronous filesystem
operations must return before their owning task can finish. A completed
preparation cannot leave a timer that later cancels execution.

### Interactive faults

Use `seedbox inject SIM_ID ACTION_FILE` to submit a typed YAML or JSON action.
For example, a file containing `action: stop` and `machine: db` stops the
application on machine `db`. Use `seedbox heal SIM_ID HANDLE` to remove a healable
network, clock or throttle fault. Both commands accept `--request-id` for safe
retries and emit an admission receipt; stream `seedbox events SIM_ID` to observe
the resulting effects. `ACTION_FILE` may be `-` for standard input.

ARM throttle factors of 1000..1000000 permille scale the 1000 ns baseline syscall
charge exactly through PV ABI 3. Each syscall captures its cost once, preserving
any remaining charge across boundaries, throttle changes and healing; ordinary
clock reads remain 1 ns. Snapshots preserve the page and in-flight guest state,
and the coordinator reapplies active throttle after a crash. This adds no ARM
speed reseeding. Fresh simulations need a matching ABI-3 build from
`python3 tools/build.py` inside `nix develop`; old retained simulations continue
with their retained engine and kernel. Native validation is pending. See the
[ARM PV ABI](arm64-backends.md#guest-layout-and-pv-abi-3).

Rerunning a completed simulation reapplies its retained interventions at the
same committed positions. Pausing, stepping and pacing remain available during
rerun; new fault injection is rejected. A server restart can recover an
interrupted simulation's durable intervention journal for rerun.

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
