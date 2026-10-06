# OCI and KVM integration under development

Linux compilation and OCI acceptance have passed; guest execution acceptance is
pending. These commands describe the runtime path and remaining checks before
claiming it works. See [the acceptance record](ACCEPTANCE.md) for the evidence and
its limits.

## Build on the qualified Linux host

From this checkout, using the existing simulator flake:

```sh
nix develop ./sim
cargo build --release --manifest-path sim/seedbox/Cargo.toml --locked
cargo build --release --manifest-path sim/vmm/Cargo.toml
bash sim/guest/build-compose.sh /tmp/seedbox-compose-init
```

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

Supply a supported Compose file and the controlled kernel. For example, after
building the binaries above, inside a root shell with the Nix tools on PATH:

```sh
seedbox=./sim/seedbox/target/release/seedbox
vmm=./sim/vmm/target/release/seedbox-vmm

"$seedbox" prepare compose.yaml \
  --kernel /path/to/controlled/bzImage --vmm "$vmm" \
  --guest-init /tmp/seedbox-compose-init --busybox "$SEEDBOX_BUSYBOX" \
  --output prepared.json

"$seedbox" run compose.yaml --seed 1009 --cpu 2 \
  --kernel /path/to/controlled/bzImage --vmm "$vmm" \
  --guest-init /tmp/seedbox-compose-init --busybox "$SEEDBOX_BUSYBOX"

"$seedbox" inspect .seedbox/runs/ATTEMPT --json
"$seedbox" replay .seedbox/runs/ATTEMPT --cpu 2
```

CPU 2 is illustrative; choose an available qualified core. The executor reserves
that core and all SMT siblings with the same `/tmp/seedbox-msr-cpu-N.lock` flock
protocol as `sim/batch.py`, before reading or changing MSRs. It snapshots exact
values, sets bit 54 with readback, pins the VMM, and restores with readback only
after reaping it. An unsuccessful restore retains `output/msr-original.json`.
Library cancellation and CLI SIGINT/SIGTERM/SIGHUP cause cleanup through the same
path. SIGKILL and host loss cannot guarantee cleanup.

`--state` chooses the content store and attempts root (default `.seedbox`).
`--memory-mb` is a semantic parameter; `--full-state` requests RAM/register
digests. `--margin` is recorded per attempt and does not change identity.
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

## Pending acceptance

1. Completed: compile the static bootstrap and Linux VMM/executor; portable tests
   pass on Linux and Darwin.
2. Completed: run the two ignored `tests/oci.rs` tests as root with the Nix tools. They pull
   Redis/etcd and build an image covering whiteouts, symlinks, `.dockerignore`,
   command defaults and user/workdir/environment metadata. They do not boot guests
   or modify CPU/MSR state.
   `bash sim/seedbox/tests/accept-oci.sh` performs steps 1–2 from the repository
   root in the Nix shell. It compiles/tests as the current user and invokes only
   the final OCI test executable with `sudo -n`.
3. Prepare and boot etcd/Redis with a custom workload; test PASS and intentional
   FAIL, exact replay, changed performance settings, timeout and cancellation.
   `bash sim/seedbox/tests/accept-runtime.sh KERNEL CPU FRESH_OUTPUT_DIRECTORY`
   builds and runs the normal/intentional-failure templates and their replays,
   checking original MSR values after each run. This separate stage starts KVM
   guests and temporarily sets the scoped MSR workaround; OCI acceptance does not.
4. Check shared lease contention and real MSR readback before/after each exit path.
5. Publish validated templates and migrate the existing application world files.

Fault authoring remains disabled pending #3 integration and effect-time validation.
