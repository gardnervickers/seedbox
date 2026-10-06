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
the retained engine, without inheriting VMM environment variables.
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
```

The OCI suite builds/tests as the current user and invokes only its final import
executable with `sudo -n`. It verifies the registry images and Dockerfile metadata,
whiteouts, symlinks and build-context filtering. It does not start guests.
The default runtime suite runs both application examples and their intentional
failure controls, then replays each. The lifecycle suite checks full-state and
changed-margin/undo replay, simulated timeout, wall expiry and SIGTERM cancellation.
Both runtime suites start KVM guests, temporarily apply the scoped MSR workaround,
and compare original MSR readbacks after every execution. Choose available CPU/SMT
lanes; contention fails without taking over another run's lease.

Fault authoring remains disabled pending #3 integration and effect-time validation.
The legacy fault campaigns are retained as described in [the migration map](../harness/worlds/README.md).
