# OCI preparation and KVM execution

The etcd and Redis Compose examples have passed real guest runs, failure controls
and replay on the qualified Linux workstation. See [the acceptance record](acceptance.md)
for the tested engine, kernel, image digests and lifecycle checks.

These are version-bound results, not qualification of arbitrary OCI runtimes or
the latest changed example inputs. See [draft formats and runtime scope](compatibility.md)
for current readers/writers, exit meanings and remaining release gates.

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
after reaping it. Only the Zen counter profile needs this workaround; on a host
with another profile the executor leases the cores and leaves MSRs alone. Other
profiles are unqualified and run only with `--allow-unqualified-cpu`. An unsuccessful restore retains `output/msr-original.json`.
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
`world_witness` only when the run has baseline overrides, applied a shape or used a connection selector, so
a run without link policy keeps its previous world witness. Shape/heal evidence follows the same strict-before-admission
counter boundary as partitions. Fork snapshots retain baselines, last arrivals,
and both RNG streams; arming a run seed reseeds those streams as before.

Every final report includes `network_witness`, conditionally folded into `world_witness` as described above. Its
versioned, little-endian, length-prefixed encoding hashes pending frame arrival,
sequence, directed indices and SHA-256 of frame bytes; last arrivals; loss/jitter
stream counters; layers ordered by fault ID with handle, pairs and policy; active
cuts; and baselines. Counts delimit collections and strings. It retains bounded
infrastructure state without packet contents or application schemas. This VMM
change creates a new engine identity; hardware/replay qualification is separate
from portable policy tests.

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
A new tuple at capacity evicts the oldest admitted incarnation, including an
actively selected one; stale handles keep their counts but cannot attach to a
replacement. Evicted midstream traffic is unclassified until a fresh opening SYN.
Selector capacity exhaustion is an explicit admission error. Counters and IDs do
not reset on discovery, process restart or healing. Fork snapshots preserve the
classifier and selector state, and reseeding does not reset their identities.

Partitions dominate selected loss, which dominates directed link shapes.
Selected loss drops every matching candidate before link RNG and FIFO updates;
unselected traffic uses the existing link policy and global causal floor. No
selector draws RNG. Broadcast candidate ordering remains ascending machine index.
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
engine bytes does not establish cross-host or cross-backend compatibility; full
host drift admission remains issue #5. Portable fixture replay verifies the bundle
mechanism; offline application replay requires separate hardware acceptance.
