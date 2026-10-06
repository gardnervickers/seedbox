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
The guest supervisor is built into `seedbox`. It uses Linux syscalls directly and
requires no separate BusyBox input. A missing input fails before preparation starts and names
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
`--layout shared-kernel` is the default: every service runs in private namespaces
on one machine named `kernel`. `--layout machine-per-service` uses independent
kernels and clocks. Layout changes execution and is recorded in the profile.
`--memory-mb` is RAM per machine: 2048 MiB by default with the shared kernel, 1024
MiB otherwise, in the range 64–3328 MiB. `--disk-mb` is writable capacity per
service, defaults to 1024 MiB, and accepts positive unsigned 32-bit MiB values.
Both sizes are semantic parameters. `--full-state` requests RAM/register
digests per machine (one with the shared kernel). Preparation checks a conservative archive boot-memory floor before acquiring
a CPU lease. Application memory can require a larger value; the etcd image failed
to unpack at 512 MiB and passed at 1024 MiB. `--margin` and `--undo` are recorded per attempt and do not change identity.
`--undo` defaults to 256 branches; zero disables undo and 16 exercises a smaller
margin. The accepted range is 0..262144. Both settings are passed explicitly to
the retained engine, without inheriting VMM environment variables.
Replay defaults to the retained engine. An explicit `--vmm NEW_BINARY
--allow-engine-change` permits a fresh identity and witness comparison with it.
Replay does not require registry access, a builder, or the original Compose files.

Run exits are 0 for PASS, 1 for FAIL, 2 for ERROR, 3 for INCOMPLETE, and 124 for
simulated TIMEOUT. Replay exits are 0 for MATCH (including a reproduced FAIL),
1 for MISMATCH, and 2 for NOT_COMPARABLE. Results print the attempt path.

## Guest semantics and limits

Each service runs in its own mount, PID and UTS namespaces with a private overlay
of a read-only image. Each distinct image is packed once per guest archive.
`pivot_root` detaches the bootstrap filesystem from the service. Writable layers
live on separate tmpfs mounts sized by `--disk-mb`; capacity exhaustion returns
ENOSPC to that service. Image volumes use the same disk and per-run lifetime.
Process stop/start preserves the writable layer; fresh runs and replays recreate it.
The disk limit is a capacity limit, not a memory reservation: written pages and
application memory draw on the machine's RAM, so physical memory exhaustion can
still affect all services on that machine.

Shared-kernel services also have private network namespaces, joined through a
routing namespace. Netem delays outgoing packets once at their destination, using
seeded latency/jitter and preserving destination order. Partitions install paired
blackhole routing rules, one copy per handle. Queued packets remain in flight.
Queues are per destination, not per link; broadcast/multicast and per-link counters
are not implemented in this layout. The independent-machine layout uses the
engine's fabric and its directed-link counters. Neither model permits external traffic.

CPU, RAM and clocks are shared in the default layout; a kernel failure affects
all services. Independent clocks/kernel failures require `machine-per-service`.
Process restart launches retained argv on the existing disk and network namespace;
it does not model a reboot or loss of unsynced writes.

Services receive private `/dev` mounts containing null, zero, full, random, urandom
and tty. Only the controller receives `/run/seedbox/control.sock`; applications
cannot access the raw management port. Applications lose SYS_ADMIN, MKNOD,
SYS_RAWIO, SYS_BOOT, SYS_MODULE, SYS_TIME and SYS_PTRACE capabilities in all sets,
including the bounding set, and run with no_new_privs. The service init is not
dumpable. These limits protect the supervisor's authority; this simulator is not a
security sandbox for hostile images. `/dev/shm`, `/dev/pts`, custom file-descriptor
limits, cgroup memory limits and per-service reboot remain follow-up work.

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
is not enabled by this adapter. With several services per machine, console lines
are prefixed `service | ` in `kernel.console`; a lone service passes through unchanged.

The launch recipe is `SBXLAUNCH3`, and the profile uses `guest_abi = oci-init-v3`.
Replay refuses older guest ABIs; use the Seedbox binary that recorded those runs.
Replay retains the original layout and disk capacity and has no override for either.

## Reproduce acceptance

From the repository root in `nix develop`:

```sh
bash crates/seedbox/tests/accept-oci.sh
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU FRESH_OUTPUT_DIRECTORY
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU ANOTHER_FRESH_DIRECTORY lifecycle
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU FAULT_OUTPUT_DIRECTORY faults
bash crates/seedbox/tests/accept-runtime.sh KERNEL CPU ISOLATION_OUTPUT_DIRECTORY isolation
```

Runtime suites default to `shared-kernel`; append `machine-per-service` after the
mode (or an empty mode argument) to qualify the other layout in a fresh directory.
The isolation suite boots root applications, checks detached roots and denied
management-device/capability access, fills one 8 MiB disk to ENOSPC, then verifies
that the other service can still write its own disk. Every semantic case is replayed.

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

In the independent-machine layout, directed network counters distinguish NIC acceptance from drops caused by
an unready queue or full receive backlog. This corrects the previous delivered
counter even in runs without faults; compare counters only with that change in
mind when allowing a different engine for replay.

The fault socket exists only on the configured controller. Connections may be
closed and reopened between completed calls; the transcript preserves both
run-wide relay IDs and connection-local request IDs. Redundant lifecycle commands
and commands targeting exited VMs return recoverable RPC errors. A service that
exits on a shared kernel is reported in `processes` and can be restarted directly;
stopping an exited service returns `invalid_state`. The controller
can inspect the application exit and choose PASS or FAIL.

Independent-machine inspection and partition/heal counter snapshots count nominal send, drop and
arrival times strictly before admission. A packet physically delivered in an
early scheduler window is excluded until its nominal arrival precedes that
boundary. Final report counters still record physical NIC acceptance totals.
