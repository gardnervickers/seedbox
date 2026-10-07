# Seedbox simulation example

This project is self-contained. The etcd and Redis templates passed real workloads,
intentional failure controls, and exact replays on the supported Linux amd64 AMD Zen
workstation with the controlled Seedbox kernel and 1024 MiB RAM per service.
See the repository's `docs/acceptance.md` for the tested image/kernel hashes.
This establishes those image versions, not compatibility with arbitrary OCI images.

Production preparation uses one service per VM, with private RAM-backed storage
retained across process restart only. The repository's `docs/compatibility.md`
describes current draft formats and version-bound runtime evidence; V0 release
compatibility and host-proof admission remain qualification gates.

The etcd template packages Seedbox's existing Go acknowledgement-ledger workload
in a Dockerfile and describes three registry-image members in Compose. The Redis
template builds a dependency-free Rust workload that checks PING, SET acknowledgements,
and exact GET values. Both use the Rust Seedbox configuration/orchestration APIs.

On the supported Linux amd64 host, after building Seedbox in `nix develop`,
from this directory:

```sh
seedbox validate
seedbox run
seedbox explore --count 2
seedbox inspect .seedbox/runs/ATTEMPT --json
seedbox replay .seedbox/runs/ATTEMPT
```

Preparation unpacks images with root ownership and runs before the execution CPU
lease. Admission briefly checks an eligible core first and releases all temporary
resources before preparation.
`run` picks a free CPU and finds the guest kernel and VMM by default; see
`seedbox help run`. Run/replay must be invoked with the required KVM/MSR access;
see the repository's `docs/runtime.md` for setup and limits. `--margin` and
`--undo` control PMU approach and stepping performance without changing results.
Charged timer I/O retains its full cost across scheduler boundaries; pending
operations are included in terminal witnesses.

For owned reproducible build inputs, `prepare`, `run` and `explore` accept
`--build-epoch SECONDS` (0..=4294967295; default absent, no environment backing).
This explicitly changes new build-layer file and directory timestamps and
requires the qualified project-Nix native Podman, including for exploration.
Registry and base image metadata is preserved. Pin/control other build inputs;
an epoch alone cannot make arbitrary builds reproducible. Services sharing one
frozen native build source reuse preparation work in that call, while retaining
independent launch/config overrides. Replay needs no epoch option and uses the
complete retained layers. See `docs/runtime.md` in the Seedbox repository.

All application storage, including Redis AOF and etcd data directories, is private
RAM-backed storage recreated on each fresh run/replay. No data persists to the host.
Service names resolve through guest hosts files. The workload owns readiness
retries; Compose dependency ordering is not implemented.

For a negative control, set `SEEDBOX_PLANT_FAILURE: "1"` in the Redis workload,
or append `--plant, missing` to the etcd client's command list. The expected result
is FAIL; a fresh replay must reproduce that FAIL with MATCH. Nonempty fault
schedules remain unsupported until their guest effect timing is verified.

`explore` validates runtime inputs and host admission before its one preparation,
then executes a finite consecutive seed sequence sequentially. Admission refusal
retains batch ERROR without preparation or seed dispatch; each seed rechecks its host.
Each run retains its ordinary manifest and replay inputs under the printed batch
path. FAIL and witnessed simulated TIMEOUT continue; infrastructure errors and
cancellation stop. Count defaults to 1 and seed to the Compose seed. `--json` prints
one report with separate outcomes and undispatched seeds. Build exploration requires
the inspected local project-Nix Podman closure; cancelled builds remain
cleanup-unverified ERROR with their private tag retained until cessation is verified.
See the repository's `docs/runtime.md` for bounded preparation evidence and limits.

To share a retained run, use `seedbox export .seedbox/runs/ATTEMPT -o run.seedbox.tar`.
On Linux or macOS, `seedbox import run.seedbox.tar --state imported` prints a new run
directory that `seedbox inspect` can read. Replay that directory with
`seedbox replay IMPORTED_RUN --state imported` on the qualified AMD Linux host
using a compatible installed Seedbox release. No original checkout, registry or
image rebuild is needed. Bundles preserve retained logs and visible omissions;
output omitted by the original capture policy is not transferred. Export/import
never replace existing data; the default transfer cap is 32 GiB for unique objects
and separately for materialized log bytes (`--max-bytes` overrides it).

Run/replay enforce the versioned admitted host/engine/kernel policy. Matching a
second machine is compatibility admission, not cross-host qualification. Old
records stay inspectable/exportable/importable, but absent or drifted host proof
is NOT_COMPARABLE (exit 2) in this CLI and cannot be bypassed with
`--allow-engine-change`. A retained previous CLI is only historical evidence.
