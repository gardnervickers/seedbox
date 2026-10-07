# OCI preparation and KVM execution

The etcd and Redis Compose examples have passed real guest runs, failure controls
and replay on the qualified Linux workstation. See [the acceptance record](acceptance.md)
for the tested engine, kernel, image digests and lifecycle checks.

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
the builder applies `.dockerignore`. The unique build tag is removed afterward.
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

## Preparation, execution, and replay

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
under sudo). The VMM is found next to an installed `seedbox`, then in this
checkout's `cargo build-vmm` output, then on `PATH`. A `seedbox` inside cargo's
`target/` directories, like the one above, uses the `cargo build-vmm` output.
The guest supervisor is built into `seedbox`, and BusyBox comes from
`SEEDBOX_BUSYBOX`. A missing input fails before preparation starts and names
every location searched.

Without `--cpu`, the executor leases the first free core, in ascending order,
among the CPUs the process may run on, skipping CPU 0's core. With `--cpu N` it
leases exactly that core and fails if another run holds it. The executor reserves
the chosen core and all SMT siblings with the same `/tmp/seedbox-msr-cpu-N.lock` flock
protocol as `harness/batch.py`, before reading or changing MSRs. It snapshots exact
values, sets bit 54 with readback, pins the VMM, and restores with readback only
after reaping it. An unsuccessful restore retains `output/msr-original.json`.
Library cancellation and CLI SIGINT/SIGTERM/SIGHUP cause cleanup through the same
path. SIGKILL and host loss cannot guarantee cleanup.

`--state` chooses the content store and attempts root (default `.seedbox`).
`--memory-mb` defaults to 1024 MiB per service and is a semantic parameter; `--full-state` requests RAM/register
digests. Preparation checks a conservative archive boot-memory floor before acquiring
a CPU lease. Application memory can require a larger value; the etcd image failed
to unpack at 512 MiB and passed at 1024 MiB. `--margin` and `--undo` are recorded per attempt and do not change identity.
`--undo` defaults to 256 branches; zero disables undo and 16 exercises a smaller
margin. The accepted range is 0..262144. Both settings are passed explicitly to
the retained engine, without inheriting VMM environment variables. `--margin`
controls PMU approach and stepping distance; syscall exits remain available
throughout a soft stop interval independently of that setting. Earlier yields
do not publish a stop, inject an interrupt, or invalidate a checkpoint.

Charged timer-port accesses conserve their full 1000 ns cost across world and
timer boundaries. Their device effect and read response occur once; the guest
cannot resume while a charged operation remains pending. Due timers latch before
the next portion of the cost is paid. A duration cutoff can retain the pending
operation, including its remaining cost and transferred bytes, in the exit
witness and the full-state `pending_io` part. Other clock advances retain the
strict hard-stop check.

Replay defaults to the retained engine. An explicit `--vmm NEW_BINARY
--allow-engine-change` permits a fresh identity and witness comparison with it.
Replay does not require registry access, a builder, or the original Compose files.

Run exits are 0 for PASS, 1 for FAIL, 2 for ERROR, 3 for INCOMPLETE, and 124 for
simulated TIMEOUT. Replay exits are 0 for MATCH (including a reproduced FAIL),
1 for MISMATCH, and 2 for NOT_COMPARABLE. Results print the attempt path.

## Guest semantics and limits

Each service currently gets one machine and one OCI-root chroot. Its complete
filesystem is writable RAM-backed initramfs storage, freshly instantiated for
each run/replay; image volume paths have the same per-run lifetime. Process
stop/start retains that filesystem. No state persists between fresh machine boots.
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

Inspection and partition/heal counter snapshots count nominal send, drop and
arrival times strictly before admission. A packet physically delivered in an
early scheduler window is excluded until its nominal arrival precedes that
boundary. Final report counters still record physical NIC acceptance totals.


## Exploration preparation

`seedbox explore [COMPOSE] --seed FIRST --count COUNT` has one sequential worker.
Count defaults to 1; seed defaults to the resolved Compose seed. Bounds are finite,
positive and checked for inclusive u64 overflow before slow work. The complete
configuration, PreparedInputs and performance parameters are frozen once. Every
attempt verifies the full retained closure through the ordinary run lifecycle;
missing/corrupt bytes cause ERROR without re-preparation or repair.

The owned batch directory contains atomic `exploration.json` checkpoints,
`preparation/` and `attempts/SEED/ORDINARY-RUN/`. Attempted means entering orchestration,
not proof that a guest started. Counts distinguish PASS/FAIL, TIMEOUT, ERROR,
INCOMPLETE and unfinished attempts; remaining seeds are undispatched. A killed process
leaves an unfinished INCOMPLETE checkpoint and independently inspectable ordinary
manifests. There is no resume operation. Checkpoint/evidence failures stop dispatch.
The final aggregate uses ERROR, INCOMPLETE, TIMEOUT, FAIL, PASS precedence and the
existing exit codes 2, 3, 124, 1, 0. Application FAIL and witnessed simulated TIMEOUT may
continue; infrastructure errors, wall expiry and cancellation stop.

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
Unavailable measurements are null and do not affect run identity.

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
engine bytes does not establish cross-host or cross-backend compatibility; full
host drift admission remains issue #5. Portable fixture replay verifies the bundle
mechanism; offline application replay requires separate hardware acceptance.
