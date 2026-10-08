# OCI preparation and KVM execution

The etcd and Redis Compose examples have passed real guest runs, failure controls
and replay on the qualified Linux workstation. See [the acceptance record](acceptance.md)
for the tested engine, kernel, image digests and lifecycle checks.

These are version-bound results, not qualification of arbitrary OCI runtimes or
the latest changed example inputs. See [draft formats and runtime scope](compatibility.md)
for current readers/writers, exit meanings and remaining release gates.

## Runtime checkpoint design (experimental)

`seedbox run --checkpoint-at-ns T --branch-seed B` executes the parent seed P
up to simulated time T, captures the world, and continues under B. B=P is a
true continuation: every captured stream and guest entropy stays in place.
Each public branch is its own retained run/attempt. Its prefix and continuation
use the ordinary `output/machines/NAME.console` and `.mgmt` files with `.chunks`
indexes, and `--full-state` writes `NAME.pages` alongside them. `logs` and `diff`
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

The CLI and engine share `crates/seedbox-vmm/src/checkpoint.rs`. Parent identity
uses the versioned `seedbox-world-parent-v2` domain, config with checkpoint
metadata removed, kernel/initrd/disk paths replaced by content hashes, engine
hash, timebase and parent seed. Branch identity binds this parent, T and B.
The retained RunIdentity also binds T and B; it is not a second parent-identity
scheme. Relocating inputs does not change the branch. Counter and PV parents
have different identities. With deferred TX enabled, both sides hash
`<timebase>+deferred-tx` in place of the timebase name, separating parent and
branch identities and future branch RNG streams from the off mode. Off keeps
the original timebase hash input. The engine recomputes the supplied CLI identity.

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
| PV timebase (#85, #89, #113) | Full page captures PV enable/ack, bound and syscall target, as well as the original fields and fork entropy. `MachineSnapshot.syscall_ns` retains the sampled per-machine speed across fresh-VM restore; forks do not resample it. The configured timebase stays in immutable boot inputs and parent identity. Spin probes finish inside `run_until`; capture refuses an active probe. The completed probe's idle state is captured in the machine. Watchdog phase/backoff and KVM-run scratch affect host performance only and reset at restore; `prepare`/`start` overwrite remaining progress and syscall baseline before guest entry. |
| Triggers (#109) | The full `Control` clone carries arm order, KMP failure tables, partial prefix/seen/reset counts, match/effect deadlines, cancellations, statuses and evidence. The committed byte slot is drained before capture. Machine `observe_console` is restored and scheduled trigger boundaries constrain the checkpoint stop. |
| Directed links (#97) | The full `Faults.links` clone carries baselines, active layers, FIFO arrival history, shaped-use marker and both RNG positions. Changed-seed forks replace future loss/jitter streams only; in-flight packets retain send-time decisions. |
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
reboot action are captured. Host leases, pause/resume socket, counter/watchdog
handles and undo caches are not world semantics. They remain owned by the
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
use fresh VMs/full RAM copies; multiple held snapshots cannot share Undo's dirty
baseline. RAM cost is approximately whole-world RAM per held checkpoint, plus
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
results and witness traces generated from main. Actual restored execution and
whole-guest feature-off equality remain unverified on this Mac. See
[the AMD host procedure](checkpoint-amd.md) for the exact remaining commands.

## Disk crash policy

`run` and `prepare` accept `--torn-writes` (off by default). Off retains the
`flush-stable/drop-all-unflushed/v1` model: crashes drop all unflushed sectors.
The omitted setting adds no profile or world JSON field and leaves the existing
disk digest and witness inputs unchanged. Building the modified VMM still changes
the engine binary hash, as every VMM source change does.

When enabled, the profile retains semantic parameters
`disk_loss_sector_bytes = "512"` and
`disk_loss_survival_probability = "0.5"`; each disk report carries the same
numeric policy under `disk.fault_model`. Replay restores this policy from the
retained profile; there is no replay override or environment variable. A retained
legacy profile containing the old `disk_loss` semantic parameter is rejected as
not replay-compatible by this CLI. The low-level world JSON uses a top-level
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

## Build on the qualified Linux host

From the repository root:

```sh
nix develop
bash kernel/build.sh
cargo build --release --locked
cargo build-vmm
```

`cargo build-vmm` builds the VMM in its own workspace with its own lockfile, so
the engine binary, and the engine hash recorded in every run, does not depend on
which cargo command built it or what else was built with it. It still depends on
the toolchain, `RUSTFLAGS` and the checkout's absolute path, which debug
information embeds.

The `seedbox` build embeds the static guest supervisor from `guest/compose-init.c`,
compiled with the shell's musl compiler (`SEEDBOX_STATIC_CC`, or `musl-gcc`).
Without one it still builds, and `run`/`prepare` require `--guest-init`, for
example the output of `bash guest/build-compose.sh FILE`.

Preparation requires `skopeo`, `umoci`, and a Docker-compatible builder (the shell
supplies Podman). Rootful unpacking preserves image ownership and modes. Run on
the supported Linux amd64 host; Darwin is for configuration and portable tests.

`skopeo` selects Linux amd64 and copies the original platform-specific manifest
and blobs with preserved digests. Seedbox verifies descriptor hashes and sizes,
retains their exact bytes, resolves launch defaults, and delegates OCI whiteouts
and extraction to `umoci`. Dockerfile builds use a retained, frozen local context;
the builder applies `.dockerignore`. The unique build tag is removed afterward,
but the builder's layer cache is kept between runs: a step such as
`RUN go mod download` re-runs only when the frozen context or Dockerfile changes.
To force fresh fetches, change the context or run `podman image prune`, which
also reclaims the cache; seedbox never prunes it.
Registry TLS verification remains enabled; signature policy is not enforced by
this importer. Build progress logs are not semantic identity inputs.
Use fully qualified Dockerfile `FROM` references such as
`docker.io/library/busybox:1.37`; the supported Podman host has no default
unqualified registry search list. `--builder docker` selects a Docker client
instead when the host provides one.

`prepare`, `run` and `explore` accept optional `--build-epoch SECONDS`
(0..=4294967295; no environment backing). It requires the inspected project-Nix
local Podman executable; custom wrappers, Docker and remote clients fail before
image preparation when the option affects a build service. Seedbox passes
`--source-date-epoch=SECONDS --rewrite-timestamp` to that native executable.
The selected epoch and policy are retained in built-image provenance. Without
the option, the builder's existing timestamp behavior remains unchanged.
Inherited `SOURCE_DATE_EPOCH` alone controls image creation/history metadata;
it does not make generated layer directories reproducible.

This explicit policy clamps all newly committed file and directory timestamps
later than the epoch, including otherwise stable copied-file timestamps. It
preserves base and registry image metadata. It does not guarantee reproducible
arbitrary builds: pin base resolution and control external build inputs yourself.
Every original manifest, config and layer remains retained; changing a merged
rootfs in a later layer cannot remove earlier varying input bytes.

Within one preparation call, services declaring the same frozen build source
on the admitted native Podman path share build/import/unpack work. Each service
still resolves its own launch overrides and packages its own guest recipe.
Contexts include retained bytes, modes, symlinks, Dockerfile selection and ignore
rules. There is no cross-call cache; custom builders and registry imports keep
their existing preparation behavior. Failed, cancelled or cleanup-unverified
sources are never reused as successful preparation.

Each prepared application release is a captured input closure. Same-version
deterministic execution and replay use its exact retained bytes, seed and
qualified profile; they do not rebuild the application. Independently rebuilding
an OCI image need not produce identical bytes for V0. The optional timestamp
policy is user-selected metadata control, not a prerequisite for this execution
guarantee or an image reproducibility guarantee.

## Preparation, execution, and replay

After building the binaries above, as root with the Nix tools on PATH
(`sudo -E env PATH="$PATH" ...` from the Nix shell):

```sh
seedbox=./target/release/seedbox

"$seedbox" prepare examples/redis --output prepared.json
"$seedbox" run examples/redis
"$seedbox" run examples/redis --timebase pv
"$seedbox" inspect .seedbox/runs/ATTEMPT --json
"$seedbox" replay .seedbox/runs/ATTEMPT
```

Every input has a default; `seedbox help run` lists them with their overrides.
The kernel is found in `$SEEDBOX_ARTIFACTS`, `$SEEDBOX_WORK/artifacts`,
`./artifacts` or `~/seedbox-sim/artifacts` (also in the invoking user's home
under sudo). The VMM is found next to an installed `seedbox`, then in this
checkout's `cargo build-vmm` output, then on `PATH`. A `seedbox` inside cargo's
`target/` directories, like the one above, uses the `cargo build-vmm` output.
The guest supervisor is built into `seedbox`, and BusyBox comes from
`SEEDBOX_BUSYBOX`. A missing input fails before preparation starts and names
every location searched.

Zen execution performs host admission before OCI work: an AMD x86-64 CPU with the
KVM and perf capabilities the engine needs (see
[host requirements](../README.md#host-requirements)). CPU model, microcode, CPUID,
engine, guest-kernel and host-kernel bytes are not pinned. Temporary core
reservations, affinity and disabled exact-event perf descriptors are released
before preparation on success/error; cleanup failure refuses admission. The actual
leased core/siblings are rechecked before any workaround MSR write or VMM spawn.
Raw static/leased CPUID observations and workaround/restore diagnostics are
retained as Seedbox-owned output. A static capability refusal is reported before
unattempted perf readiness.

Inspect, export and import keep old manifests unchanged. Run identities carry no
host-policy proof; `host_policy*` fields in older records are kept as data and do
not block replay.

Without `--cpu`, the executor leases the first free core, in ascending order,
among the CPUs the process may run on, skipping CPU 0's core. With `--cpu N` it
leases exactly that core and fails if another run holds it. The executor reserves
the chosen core and all SMT siblings with the same `/tmp/seedbox-msr-cpu-N.lock` flock
protocol as `harness/batch.py`, before reading or changing MSRs. It snapshots exact
values, sets bit 54 with readback, pins the VMM, and restores with readback only
after reaping it. Only the Zen counter profile needs this workaround; on a host
with another profile the executor leases the cores and leaves MSRs alone. Other
profiles are unqualified and run only with `--allow-unqualified-cpu`. An unsuccessful restore retains `output/msr-original.json`.
Library cancellation and CLI SIGINT/SIGTERM/SIGHUP cause cleanup through the same
path. SIGKILL and host loss cannot guarantee cleanup.

## Host pause

From a second terminal, pause or resume the host's entire executing world:

```sh
sudo seedbox pause .seedbox/runs/ATTEMPT
sudo seedbox resume .seedbox/runs/ATTEMPT
```

The attempt directory exists before execution finishes. Find live attempts with
`find .seedbox/runs -path '*/output/host-control'`; remove the `/output/host-control`
suffix to get RUN. A replay creates a new attempt: control that directory, not
the completed source run. Use the execution user's privileges (normally root).
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

The CLI executor continues checking wall-time limits and SIGINT/SIGTERM/SIGHUP
cancellation while the VMM waits. Pause time counts against the configured wall
limit: a run that expires while paused is INCOMPLETE, just like any other wall
expiry. CPU/SMT leases and the MSR workaround remain held throughout pause; cleanup
reaps the VMM before restoring them. Normal completion/cancellation removes the
locator and private endpoint. Host loss or SIGKILL may leave stale paths, which
cannot acknowledge commands; completed, unavailable and older engines return a
setup error. Pause is not a snapshot and cannot survive process exit or host reboot.

The VMM source change produces a new engine identity. Build the new engine
with `cargo build-vmm` on Linux.
Portable tests exercise the actual socket gate and CLI acknowledgements. The ignored
KVM comparison needs a completed full-state run made with this new engine and enough
wall-time headroom; it exports/imports exact retained inputs into private scratch
state, compares an unpaused replay with two pause/resume cycles, and checks MATCH,
outcome, identity, witness and all full-state digests:

```sh
sudo -E env SEEDBOX_HOST_PAUSE_RUN=/path/to/full-state-run \
  SEEDBOX_HOST_PAUSE_STATE=/path/to/source-state \
  cargo test -p seedbox --locked --test host_pause -- --ignored
```

An optional `SEEDBOX_CPU` selects the usual leased core. This test starts real guests
and requires the ordinary admitted Linux KVM host and root. Source-test success
does not establish hardware witness invariance or qualify the new engine.

## Execution settings

`run`, `prepare` and `explore` accept `--timebase counter|pv`, defaulting to `counter`.
This is a semantic choice with no environment override. Counter time advances
with retired guest branches; PV compute between exits costs no virtual time,
with progress charged at guest exits, syscalls and clock reads. The same seed
under the two models produces different identities and retained manifests.
The semantic profile records `timebase` only for PV, preserving existing counter
identity bytes and hashes. Replay reads the recorded choice, accepts no timebase
override, and refuses unknown recorded values or a backend with a different
timebase, even with `--allow-engine-change`. Bundles preserve the choice.
The VMM report is checked against it; reports from older engines without a
timebase field mean counter. Run/replay and inspect print `timebase: VALUE`.
`inspect --json` exposes the semantic profile and, when present, the VMM's
reported `report.timebase` through the library's serialization.

PV uses no host PMU counter. It keeps the CPU/SMT leases but skips the counter
profile, `--allow-unqualified-cpu`, the perf event checks and the Zen MSR
workaround, so any x86-64 KVM host can run it, and its identity carries no
counter profile. That does not qualify additional PV hosts. The counter stop-placement description
below applies to the default counter model; PV uses its own exit-based progress.

`--state` chooses the content store and attempts root (default `.seedbox`).
`--memory-mb` defaults to 1024 MiB per service and is a semantic parameter; `--full-state` requests RAM/register
digests and retains per-page RAM hashes (format below). Preparation checks a conservative archive boot-memory floor before acquiring
a CPU lease. Application memory can require a larger value; the etcd image failed
to unpack at 512 MiB and passed at 1024 MiB. `--margin` and `--undo` are recorded per attempt and do not change identity.
`--undo` defaults to 256 branches; zero disables undo and 16 exercises a smaller
margin. The accepted range is 0..262144. Both settings are passed explicitly to
the retained engine, without inheriting VMM environment variables. `--margin`
controls PMU approach and stepping distance when undo is off. Neither setting
decides where a stop lands: the first natural VM exit at or after the soft
target and below the hard target, otherwise an exact branch count at the hard
target, before the instruction there runs. Syscalls are not stopping points, and the
engine requests no syscall yield exits.

Charged timer-port accesses conserve their full 1000 ns cost across world and
timer boundaries. Their device effect and read response occur once; the guest
cannot resume while a charged operation remains pending. Due timers latch before
the next portion of the cost is paid. A duration cutoff can retain the pending
operation, including its remaining cost and transferred bytes, in the exit
witness and the full-state `pending_io` part. Other clock advances retain the
strict hard-stop check.

Engines and guest kernels are not pinned: run, explore, replay and doctor admit
any built VMM and guest kernel, and each run records their hashes in its
identity. `--allow-dev-engine` is still accepted and has no effect. An older
record's `engine_qualification` parameter is kept as data and does not block replay.

Replay defaults to the retained engine. An explicit `--vmm NEW_BINARY
--allow-engine-change` permits a fresh identity and witness comparison with it.
Replay does not require registry access, a builder, or the original Compose files.

Run exits are 0 for PASS, 1 for FAIL, 2 for ERROR, 3 for INCOMPLETE, and 124 for
simulated TIMEOUT. Replay exits are 0 for MATCH (including a reproduced FAIL),
1 for MISMATCH, and 2 for NOT_COMPARABLE, including a cancelled or incomplete
replay. Results print the attempt path.
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

With `--full-state`, the VMM also writes `output/machines/NAME.pages` for each machine.
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
summary on stderr. A MISMATCH replay adds the same evidence under
`replay.ram_pages.machines` and prints it on stderr. Existing JSON fields remain;
`diff --json` still emits one document and no page-summary stderr.

Both files must be manifest-declared, checksum-valid and exactly eight bytes per
recorded 4 KiB page. Missing or omitted evidence cannot locate PFNs and is reported
as a limitation. Invalid selected page evidence makes offline diff exit 2;
replay keeps its MISMATCH/exit 1 and retains the diagnostic in
`replay.ram_pages.limitations`. Old engines' runs remain readable without these
files. PFNs locate final RAM differences, not the first divergent instruction.

All changed identity fields are listed as JSON pointers with both values, including
configuration, seed, engine, kernel, images/layers, guest archives, provenance and
semantic parameters. Missing values are null. Arrays are compared as whole values.
Execution outcome, report, errors and omission metadata are also compared.
Attempt names, replay ancestry, performance parameters, engine performance counters,
host timings and arbitrary console/log contents do not determine MATCH.

When both runs retain `machines/NAME.trace`, diff compares the existing engine
records (`kind branches time_ns extra(hex) rip(hex)`) in order for each machine.
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
execution admission. It opens KVM and the disabled exact-event perf descriptor,
reads/permission-checks the selected core and siblings' MSRs without changing
their values, and verifies restoration of temporary affinity and release of all
probe descriptors and reservations. Any uncertainty prevents `VALID` or success
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

`prepare`, `run` and `explore` accept `--deferred-tx` (off by default), also
accepted by the raw VMM. This is a semantic option: enabling it records
`deferred_tx=true` in the profile and changes packet timing and run identity.
Off omits the parameter and retains the behavior at `13b7f85`, including legacy
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
poll, and halt is polled before parking. A PV completion spin that makes no
exits is serviced at the existing proven state cycle's canonical idle point,
even if interrupts are disabled or suppressed; it resumes after completion.
Host watchdog interrupts and intermediate cycle-search steps never service TX.
As in the existing PV model, arbitrary non-repeating, clock-free loops are not
bounded by simulated time; this option does not promise to terminate such guest
programs. No control virtqueue is advertised.

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
`cargo test-vmm event_idx_polls_on_entry_slice_boundary_and_halt_without_kicks -- --ignored`
and `cargo test-vmm deferred_tx_completes_a_pv_spin_without_halt_kick_or_interrupts -- --ignored`.
The latter compares witnesses/state across two watchdog periods. The 60 s kill
world, PV timebase, seed 4000 needs separate option-off and option-on runs and
same-mode replays on the AMD host. Compare doorbells, host time and the pickup
delay bounds in the engine reports. Option-on host results are pending.


When text output reports `execution: VMM failed; see vmm.stderr`, `run`, `replay`,
`explore` and `inspect` also print up to five non-empty lines from the final 8 KiB
of the attempt's `output/vmm.stderr` to stderr. Each line starts with `vmm stderr:`;
terminal control characters are removed and invalid UTF-8 is replaced. A line
cut by the byte limit starts with `...`. Missing or unreadable logs are skipped
without changing the outcome. This excerpt is local diagnostic output only:
retained records, serde field names, status lines and `--json` output are unchanged.
`inspect` keeps the recorded `error:` line on stdout alongside the run status.

## Reproduce acceptance

From the repository root in `nix develop`:

```sh
bash crates/seedbox/tests/accept-oci.sh
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU FRESH_OUTPUT_DIRECTORY
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU ANOTHER_FRESH_DIRECTORY lifecycle
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU FAULT_OUTPUT_DIRECTORY faults
```

The OCI suite builds/tests as the current user and invokes only its final import
executable with `sudo -n`. It verifies the registry images and Dockerfile metadata,
whiteouts, symlinks and build-context filtering. It does not start guests.
The default runtime suite runs both application examples and their intentional
failure controls, then replays each. The lifecycle suite checks full-state and
changed-margin/undo replay, simulated timeout, wall expiry and SIGTERM cancellation.
The `faults` mode uses a Python controller and synthetic key/value services to verify acknowledged-write
retention across process restarts, overlapping partition healing, and retained
replay. Its deliberately forgetful variant must fail after restart and pass
without it. Missing and rejected effect acknowledgments must produce ERROR.
All runtime modes start KVM guests, temporarily apply the scoped MSR workaround,
and compare original MSR readbacks after every execution. Choose available CPU/SMT
lanes; contention fails without taking over another run's lease.

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
become visible at their recorded simulated time, and device DMA participates in
undo bookkeeping. The kernel must include `CONFIG_VIRTIO_CONSOLE`; rebuild it
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
    - {from: server, to: client, latency: 20ms, jitter: 2ms, loss_ppm: 50000}
```

Within `x-seedbox`, each pair must be unique and name existing services. An entry
requires `latency`; `jitter` and `loss_ppm` default to zero. Policies are absolute:
a missing jitter or loss does not inherit the global value. Every baseline and
runtime shape requires latency at least the global latency, jitter less than its
latency, latency plus jitter at most one hour, and integer loss in `[0, 1000000]`.
The global latency remains the whole-run lookahead; it cannot be lowered by a shape.

`world.shape(from_, to, latency=..., jitter=0, loss_ppm=0)` uses nanosecond integers
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

Counters distinguish `delivered`, `partition_dropped`, `receiver_dropped`, and
`loss_dropped`; `loss_dropped` includes selector drops and is omitted for a link
that has lost nothing.
The report's `network_witness` hashes in-flight frames, per-link last
arrivals, stream positions, live layers, cuts and baselines. It is folded into
`world_witness` only when the run has baseline overrides, applied a shape or used a connection selector, so
a run without link policy keeps its previous world witness. Shape/heal evidence follows the same strict-before-admission
counter boundary as partitions. Fork snapshots retain baselines, last arrivals,
and both RNG streams; arming a run seed reseeds those streams as before.

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

## Exploration preparation

`seedbox explore [COMPOSE] --seed FIRST --count COUNT` has one worker by default.
`--workers N` (1..32) permits N simultaneous owned attempts; count defaults to 1
and seed to the resolved Compose seed. Parallel execution rejects an explicit
`--cpu` or `SEEDBOX_CPU` before preparation. Automatic selection must reserve N
eligible physical cores with all SMT siblings together during preflight, verify
each core's admission and release every transient resource before preparation.
Insufficient capacity is ERROR; there is no queue/retry on host contention. A
library execution backend supports only one worker unless it explicitly overrides
`preflight_workers`. Per-attempt admission and leases remain unchanged. Bounds are finite,
positive and checked for inclusive u64 overflow before slow work. Runtime inputs and
backend host admission are checked before the sole preparation; refusal retains
batch ERROR with no prepared inputs or seed attempts. Transient admission resources
are released before preparation, and each seed retains the ordinary leased recheck.
The complete
configuration, PreparedInputs and performance parameters are frozen once. Every
attempt verifies the full retained closure through the ordinary run lifecycle;
missing/corrupt bytes cause ERROR without re-preparation or repair.

The owned batch directory contains atomic `exploration.json` checkpoints,
`preparation/` and `attempts/SEED/ORDINARY-RUN/`. Attempted means entering orchestration,
not proof that a guest started. Version 2 records explicit indexes (`seed = FIRST +
index`) in dispatch order, independent of completion order. Each attempt's
`finished` flag means the coordinator retained its terminal result; false includes
a result that may already exist in an independently committed run manifest.
Counts distinguish PASS/FAIL, TIMEOUT, ERROR, INCOMPLETE and unfinished attempts;
remaining seeds are an undispatched suffix. Unsupported checkpoint versions are
refused clearly without defaults or migration; version 1 bytes require their prior
reader, while ordinary run inspection/replay is unchanged. A killed process
leaves an unfinished INCOMPLETE checkpoint and independently inspectable ordinary
manifests. There is no resume operation. Checkpoint/evidence failures stop dispatch.
The final aggregate uses ERROR, INCOMPLETE, TIMEOUT, FAIL, PASS precedence and the
existing exit codes 2, 3, 124, 1, 0. Application FAIL and witnessed simulated TIMEOUT may
continue; infrastructure errors, wall expiry and cancellation stop new dispatch
and cancel only owned running attempts. The caller's library token is not modified
by an internal failure. Scoped workers are joined even if checkpoint retention
fails. A backend that cannot complete owned cleanup prevents a prompt return; no
unrelated process or service is stopped. Cancellation races may leave an already
finished PASS/FAIL beside cancelled INCOMPLETE attempts and undispatched seeds.

Preparation diagnostics belong to the caller, surviving preparer/scratch destruction.
Raw phase output is capped at 4 MiB per file and 64 MiB total. Bounded prefixes are retained
while the rest is drained, with exact observed omission counts/reasons. An output
stream still open after owned cleanup is incomplete evidence with an explicitly
unknown further byte count and cleanup-unverified ERROR. Compact command/cleanup
receipts have a separate 4 MiB budget; version text is capped at 8 KiB. Required evidence
write failure is ERROR. Receipts record program/hash/arguments/cwd, local mode,
private tag and build-input digests, process status, cancellation/escalation/reap,
elapsed time and cleanup disposition. Application stdout is never parsed for verdicts.

Initial build admission is the inspected project-Nix Linux Podman launcher and
native binary closure, invoked through the launcher with `--remote=false`. Its
local storage/runtime information is retained. Unknown opaque wrappers, remote-only
clients, Docker daemon builders and unverified delegation are unsupported. Registry
projects do not acquire a builder requirement. Existing run/prepare semantics remain.

Cancellation signals only an owned process group, preserving the unreaped leader
identity until final group signalling, then reaps it. Graceful termination escalates
after 250 ms; an unconfirmed exit after 5 s is cleanup-unverified ERROR. This cannot
promise cleanup for a process blocked indefinitely in the kernel or delegated
builder work. Cancelled builds remain cleanup-unverified and keep their unique tag;
no image-rm races a potentially running build. After a completed build, private-tag
cleanup is separately bounded; cleanup failure outranks clean cancellation. No
shared daemon, unrelated image/container, host service or cache is changed.

CLI resource fields report self/reaped-child user/system CPU deltas and separate
self/child RSS high-water readings in bytes, including preparation. They are not
simultaneous aggregate memory, guest peak memory, simulated time or CPU lease duration.
Unavailable measurements are null and do not affect run identity. `peak_running`
is the maximum number of active orchestration attempts. `active_attempt_seconds`
is the sum of whole attempt wall intervals, including materialization, admission
and cleanup, for attempts whose terminal results reached the coordinator. Once
all owned attempts return, it is a conservative guest-core charge for one world
per worker, not exact lease duration or CPU measurement. Unfinished checkpoints
omit still-running/uncheckpointed intervals and cannot supply a complete charge. No exact lease telemetry API
is added. Worker count and core selection are operational choices, absent from
semantic run identity. Synthetic serial/parallel lifecycle controls do not qualify
real per-core/witness invariance; that and staggered hardware cancellation remain
required runtime gates.

Required tool-version provenance uses stdout only, independently of diagnostic
truncation. Its raw `.stdout` evidence is bounded to 8192 bytes per value and
4 MiB per preparation; an oversized or incomplete value fails preparation.
Version stderr remains quota-limited `.log` diagnostics with omission receipts.

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
AMD execution, replay and observation/performance qualification remain separate.
The finite [AMD procedure](connection-selectors-amd.md) uses the guest-owned
`harness/acceptance/workloads/connections.yaml` fixture.

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

Inspect needs no original checkout or registry. Ordinary replay from the imported
state uses the retained engine/guest/kernel inputs and preserves existing
PASS/FAIL, ERROR, INCOMPLETE, TIMEOUT and MATCH/MISMATCH meanings. It requires the
compatible installed Seedbox release and qualified host runtime/profile. Exporting
engine bytes does not establish cross-host or cross-backend compatibility;
missing KVM or perf capabilities are refused by host admission. Portable fixture
replay verifies the bundle mechanism; offline application replay requires separate hardware acceptance.

### Explicit unqualified counters

`--allow-unqualified-cpu` applies consistently to run, explore and replay. It
permits only an explicitly selected known non-Zen counter, with a warning that
results are unverified. Its `counter` semantic parameter identifies the
experimental profile. Replay still requires the recorded counter and exact
supported release/input bytes. The flag cannot bypass checksums,
retained-profile checks or CPU ownership. This encoding is intentional; old
records are not silently upgraded or relabeled.

The experimental path checks the counter on each selected SMT sibling and
applicable KVM capabilities under a scoped affinity guard, then rechecks while
all canonical leases are held. A counter without a workaround does not open,
write or claim restoration of AMD MSRs. Every child is reaped before applicable
restoration and lease release. `doctor` returns a setup refusal for unqualified
counters, including when the experimental flag is present, so it never emits an
AMD `VALID` success for Intel. Exit meanings remain unchanged.

The current staged source pins frozen engine391f and accepted kernel68bf (K2).
The artifact has build provenance but is not runtime qualified. Fresh ordinary
full-state run/replay acceptance remains required. Experimental readiness opens
the selected raw perf event disabled before preparation and under each held
sibling lease, then closes it before restoring affinity. Counting attributes
match the engine; admission adds PERF_FLAG_FD_CLOEXEC8 rather than engineopen0
solely to protect descriptor inheritance. It never enables, maps or reads the
guest event. Source tests and warned Intel opt-in are not Intel qualification or
a determinism guarantee.

## Committed-time logs

`seedbox logs RUN --machine NAME --stream console --since-ns 1000000000
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
raw streams and indexes stay explicit after import. See [timed logs](timed-logs.md)
for format bounds, a portable demo and the pending AMD validation commands.

## Committed console triggers

The development engine implements bounded, exactly-once console marker triggers
through the guest control API. Arm/cancel admission, match deadlines, future
causal effect barriers, snapshot state and witness evidence are specified in
[the trigger contract](triggers.md). No trigger ever armed adds no report fields,
witness bytes or console stopping points. This changes the engine and requires
`--allow-dev-engine`; AMD KVM validation remains outstanding.
