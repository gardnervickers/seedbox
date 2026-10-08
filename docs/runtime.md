# OCI preparation and KVM execution

The etcd and Redis Compose examples have passed real guest runs, failure controls
and replay on the qualified Linux workstation. See [the acceptance record](acceptance.md)
for the tested engine, kernel, image digests and lifecycle checks.

These are version-bound results, not qualification of arbitrary OCI runtimes or
the latest changed example inputs. See [draft formats and runtime scope](compatibility.md)
for current readers/writers, exit meanings and remaining release gates.

## Disk crash policy

`run` and `prepare` accept `--torn-writes` (off by default). Off retains the
`flush-stable/drop-all-unflushed/v1` model: crashes drop all unflushed sectors.
The omitted setting adds no profile or world JSON field and leaves the existing
disk digest and witness inputs unchanged. Building the modified VMM still changes
the engine binary hash, as every VMM source change does.

When enabled, the profile's semantic parameter `disk_loss` and each disk report's
`model` are
`flush-stable/seeded-latest-sector-512-half/disk-loss-machine-incarnation/v1`.
Replay restores this policy from the retained profile; there is no replay override
or environment variable. The low-level world JSON uses the same `disk_loss` key
and model string. Unknown model strings are rejected.

At each crash, the latest unflushed version of each sector independently has a
50% chance of surviving. The atomic unit is 512 bytes: multi-sector writes can
survive as any subset, but a single sector's bytes are never torn. A later sector
write can survive while an earlier one is lost. Overwritten historical versions
are not candidates; there is no write log. Lost sectors fall back to their last
flushed contents or the base image. Every completed FLUSH stabilizes all preceding
completed writes, so it never loses coverage under this policy.

Selection visits volatile sectors in ascending sector order, using `below(2) == 1`
from the versioned SHA-256 RNG domain `disk-loss/<machine>/<incarnation>` and the
run seed. Incarnation identifies the machine that crashed (initially 0). No
other RNG stream is consumed. Survivors become stable in the next incarnation;
snapshot/fork restore carries the policy and both disk layers. Reports count
only actually lost volatile sectors in `disk_lost_sectors`.

This policy applies to retained virtio-blk disks. Current Compose application
storage remains RAM-backed; attaching named volumes to retained disks is separate
work. Low-level disk fixtures, including the ext4 fsync ledger, are documented in
[`harness/worlds/README.md`](../harness/worlds/README.md#retained-disk).

## Build on the qualified Linux host

From the repository root:

```sh
nix develop
bash kernel/build.sh
cargo build --release --locked
```

The platform VMM is linked into `seedbox`; ordinary runtime commands do not
compile or find a separate executable. `cargo build-vmm` and `cargo test-vmm`
remain isolated standalone aliases for low-level fixtures. Their compiler
wrapper remaps build paths and removes the shell's synthetic checkout RPATH.
The linked executable uses the workspace lockfile and has its own engine identity.

New profiles record simulator version, host and guest architectures, backend,
timebase and an explicit internal-worker contract. The content store retains
the complete executable, including its signature on macOS. Replay launches that
retained executable with the saved contract and metadata, without replacing its
version with the installed CLI's version. No manifest or bundle format changes
are needed. Historical profiles with no descriptor still invoke their retained
standalone VMM directly. Partial, unknown or incompatible descriptors fail
before host execution. The combined executable is a development engine under
the existing explicit `--allow-dev-engine` gate; it has not received new release
qualification. `prepare` records the development marker without authorizing
execution. Apple Silicon also supports the native development ARM Compose
backend described below.

Development versions carry the Git source commit (`0.1.0-dev+COMMIT`) and a
`.dirty` suffix for local changes. Source exports without provenance report
`-dev+unknown`; a matching clean release tag is required for a release label.
This readable label does not replace exact retained bytes or qualify a new
runtime. The Mac maintainer build propagates source provenance into its exported
Linux helper without copying Git history.

The `seedbox` build embeds the static guest supervisor from `guest/compose-init.c`,
compiled with the shell's musl compiler (`SEEDBOX_STATIC_CC`, or `musl-gcc`).
Without one it still builds, and `run`/`prepare` require `--guest-init`, for
example the output of `bash guest/build-compose.sh FILE`.

Preparation requires `skopeo`, `umoci`, and a Docker-compatible builder (the shell
supplies Podman). Rootful unpacking preserves image ownership and modes. Run on
the supported Linux amd64 host, or use the packaged private preparation VM on Mac.

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

Apple Silicon uses the native development HVF application backend. Enter
`nix develop` and run `bash tools/mac-runtime/build-cli.sh` to produce the signed
CLI with its ARM kernel, static helpers and Linux preparation tools embedded.
Use `target/release/seedbox run PROJECT --allow-dev-engine` and
`target/release/seedbox replay RUN --allow-dev-engine`, without host root or
separate binary paths. The owned preparation VM stops before HVF execution;
replay never starts it. Runtime commands never invoke Rust. See
[the full Mac workflow](../tools/mac-runtime/README.md).

Mac profiles retain their own executable descriptor, ARM boot placement,
PV entropy/timer policy, guest-kernel hash and HVF CPU/OS observations.
Full-state capture includes native CPU/SIMD/GIC state and application devices.
Old fixture reports cannot satisfy this application contract. A missing or
incompatible retained proof fails replay before host execution.
This remains unqualified development evidence. ARM images, 64..3072 MiB per
guest and one exploration worker are required; PMU/core options apply to Linux.
The generated etcd example sets `GOMAXPROCS=1` for its single-vCPU guests. The
native PV backend advances through cooperative guest boundaries; the example
with four Go workers stalled in native guest execution and is not validated.

After building the binaries above, as root with the Nix tools on PATH
(`sudo -E env PATH="$PATH" ...` from the Nix shell):

```sh
seedbox=./target/release/seedbox

"$seedbox" prepare examples/redis --output prepared.json
"$seedbox" run examples/redis
"$seedbox" inspect .seedbox/runs/ATTEMPT --json
"$seedbox" replay .seedbox/runs/ATTEMPT
```

Every input has a default; `seedbox help run` lists them with their overrides.
The kernel is found in `$SEEDBOX_ARTIFACTS`, `$SEEDBOX_WORK/artifacts`,
`./artifacts` or `~/seedbox-sim/artifacts` (also in the invoking user's home
under sudo). The VMM defaults to the engine linked into `seedbox`; `--vmm`
explicitly selects the legacy standalone contract.
The guest supervisor is built into `seedbox`, and BusyBox comes from
`SEEDBOX_BUSYBOX`. A missing input fails before preparation starts and names
every location searched.

Qualified Zen execution requires the versioned `amd-zen3-engine52eb-v2` host policy in
[host requirements](../README.md#host-requirements). Run validates immutable
engine/guest-kernel inputs and performs host admission before OCI work; host-kernel
bytes are not checked. Temporary core reservations, affinity and disabled
exact-event perf descriptors are released
before preparation on success/error; cleanup failure refuses admission. The actual
leased core/siblings are rechecked before any workaround MSR write or VMM spawn.
Complete raw static/leased CPUID observations and workaround/restore diagnostics
are retained as Seedbox-owned output. The expected filtered table is source-derived;
it is not an actual guest-visible CPUID observation. The complete table retains
KVM's entry order, as the unchanged engine does; live observations are never
sorted or repaired. Correcting the earlier sorted expected table changes the full
policy fingerprint and run identity. A prior full proof of that table remains
NOT_COMPARABLE, with the original manifest preserved. Zero padding remains a
strict expectation; decoded syscall traces do not measure those padding bytes.
A static field refusal is reported before unattempted perf readiness.

Inspect, export and import keep old manifests unchanged. Missing or drifted proof
in an old replay produces a retained ERROR/NOT_COMPARABLE rejection with a specific
host-admission reason and exit 2 before host execution. `--allow-engine-change`
cannot bypass the policy. Historical use of a previous retained CLI remains
possible and has its own evidence boundary. A second machine matching every
required field receives compatibility admission, not cross-host qualification.
No host-ID enrollment or application-specific output schema is added.

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

The VMM source change produces a new engine identity. The accepted frozen engine,
kernel pins and admission policy are unchanged; build the new engine with
`cargo build-vmm` on Linux and use `--allow-dev-engine` until separately qualified.
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

`--state` chooses the content store and attempts root (default `.seedbox`).
`--memory-mb` defaults to 1024 MiB per service and is a semantic parameter; `--full-state` requests RAM/register
digests. Preparation checks a conservative archive boot-memory floor before acquiring
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

`--allow-dev-engine` on run, explore, replay and doctor explicitly admits an
unpinned VMM or guest kernel. It has no environment or configuration backing.
Such runs bind `engine_qualification = "dev-unpinned"` in their semantic identity,
and their run report and inspect JSON carry the marker through export/import.
Both pinned artifacts remain ordinary runs even when the flag is supplied.
Unknown markers, a dev marker with both artifacts pinned, and unpinned artifacts
without the marker are rejected. All other admission checks remain in force.

Replaying a marked dev run requires `--allow-dev-engine` and the same retained
engine/kernel hashes. MATCH and MISMATCH output is labelled `unqualified dev
engine`; omitting the flag retains NOT_COMPARABLE (exit 2) naming the flag before
live host admission. `--allow-engine-change` cannot change a dev run's engine.
This opt-in does not qualify development artifacts or change release pins.

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

For unpinned artifacts, `seedbox doctor --allow-dev-engine` uses the same host
probe and prints `dev engine: unqualified`; its JSON includes
`engine_qualification: "dev-unpinned"`. This is unqualified development readiness.
Passing the flag with both pinned artifacts adds no marker.

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

At a boundary T every machine reaches T before control admission and faults in
ID order, followed by sends at T or later. Routing fixes each candidate's loss
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
`loss_dropped`; `loss_dropped` is omitted for a link that has lost nothing.
The report's `network_witness` hashes in-flight frames, per-link last
arrivals, stream positions, live layers, cuts and baselines. It is folded into
`world_witness` only when the run has baseline overrides or applied a shape, so
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

Faults and control requests that take effect at the same simulated time T are
ordered against sends as follows. The world applies a boundary at T only once
every machine has reached T, so a machine that is still running before T keeps
sending under the earlier network state, even when another machine is already
waiting at T. At T, control admission and effects come first, then faults in
ID order, and only then does any machine run, so every frame sent at T or
later sees the new state. A frame's partition decision and arrival time are
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
CPU/microcode, CPUID and capability drift is refused by the versioned admission
policy; focused guest/profile acceptance remains a separate gate. Portable fixture
replay verifies the bundle mechanism; offline application replay requires separate hardware acceptance.

### Explicit unqualified counters

`--allow-unqualified-cpu` applies consistently to run, explore and replay. It
permits only an explicitly selected known non-Zen counter, with a warning that
results are unverified. Its `counter` semantic parameter identifies the
experimental profile; qualified AMD `host_policy` fields are absent and a mixed
profile is refused. Replay still requires the recorded counter and exact
supported release/input bytes. The flag cannot bypass release pins, checksums,
retained-profile checks or CPU ownership. This encoding is intentional; old
records are not silently upgraded or relabeled.

The experimental path checks the counter on each selected SMT sibling and
applicable KVM capabilities under a scoped affinity guard, then rechecks while
all canonical leases are held. A counter without a workaround does not open,
write or claim restoration of AMD MSRs. Every child is reaped before applicable
restoration and lease release. `doctor` returns a setup refusal for unqualified
counters, including when the experimental flag is present, so it never emits an
AMD `VALID` success for Intel. Exit meanings remain unchanged.

The current staged source pins frozen engine52eb and accepted kernele8ece.
The artifact has build provenance but is not runtime qualified. Fresh ordinary
full-state run/replay acceptance remains required. Experimental readiness opens
the selected raw perf event disabled before preparation and under each held
sibling lease, then closes it before restoring affinity. Counting attributes
match the engine; admission adds PERF_FLAG_FD_CLOEXEC8 rather than engineopen0
solely to protect descriptor inheritance. It never enables, maps or reads the
guest event. Source tests and warned Intel opt-in are not Intel qualification or
a determinism guarantee.
