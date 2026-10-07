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
seedbox inspect .seedbox/runs/ATTEMPT --json
seedbox replay .seedbox/runs/ATTEMPT
```

Preparation unpacks images with root ownership and runs before the CPU lease.
`run` picks a free CPU and finds the guest kernel and VMM by default; see
`seedbox help run`. Run/replay must be invoked with the required KVM/MSR access;
see the repository's `docs/runtime.md` for setup and limits. `--margin` and
`--undo` control PMU approach and stepping performance without changing results.
Charged timer I/O retains its full cost across scheduler boundaries; pending
operations are included in terminal witnesses.

All application storage, including Redis AOF and etcd data directories, is private
RAM-backed storage recreated on each fresh run/replay. No data persists to the host.
Service names resolve through guest hosts files. The workload owns readiness
retries; Compose dependency ordering is not implemented.

For a negative control, set `SEEDBOX_PLANT_FAILURE: "1"` in the Redis workload,
or append `--plant, missing` to the etcd client's command list. The expected result
is FAIL; a fresh replay must reproduce that FAIL with MATCH. Nonempty fault
schedules remain unsupported until their guest effect timing is verified.

To share a retained run, use `seedbox export .seedbox/runs/ATTEMPT -o run.seedbox.tar`.
On Linux or macOS, `seedbox import run.seedbox.tar --state imported` prints a new run
directory that `seedbox inspect` can read. Replay that directory with
`seedbox replay IMPORTED_RUN --state imported` on the qualified AMD Linux host
using a compatible installed Seedbox release. No original checkout, registry or
image rebuild is needed. Bundles preserve retained logs and visible omissions;
output omitted by the original capture policy is not transferred. Export/import
never replace existing data; the default transfer cap is 32 GiB for unique objects
and separately for materialized log bytes (`--max-bytes` overrides it).

Directed network baselines belong in `x-seedbox.network.links`, for example
`[{from: server, to: client, latency: 20ms, jitter: 2ms, loss_ppm: 50000}]`
using this project's service names. Each pair is unique; latency is required,
jitter and loss default to zero. Latency cannot be below global network latency,
jitter must be less than latency, their sum is at most one hour, and loss is an
integer from 0 to 1000000. `seedbox validate` checks these bounds.

Guest controllers can call `world.shape(["server"], ["client"], latency=20_000_000,
jitter=2_000_000, loss_ppm=50000)` and `world.heal(handle)`. Shapes apply only in
the requested direction. The latest live layer wins; healing reveals the prior
policy. Partitions dominate shapes, and already routed frames retain their
send-time decisions. Inspection includes shape handles and `loss_dropped` counters.
Network state contributes a `network_witness` to the final world witness.
