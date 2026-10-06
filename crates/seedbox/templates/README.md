# Seedbox simulation example

This project is self-contained. The repository's `docs/acceptance.md` records the
exact profiles and image/kernel hashes for etcd and Redis workloads, intentional
failure controls, and replays on the supported Linux amd64 AMD Zen workstation.
The current default places every service
in its own namespaces on one shared kernel with 2048 MiB RAM;
`--layout machine-per-service` uses 1024 MiB per machine and independent kernels.
Qualification applies to the recorded profiles and image versions; arbitrary OCI
image compatibility requires its own acceptance.

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
see the repository's `docs/runtime.md` for setup and limits.

All application storage, including Redis AOF and etcd data directories, is private
RAM-backed storage recreated on each fresh run/replay. Each service has a detached
image overlay with a 1024 MiB writable disk (`--disk-mb`). Disk capacity is separate
from `--memory-mb`, the RAM per machine; written disk pages consume that RAM.
Process stop/start retains the disk. CPU, RAM and clocks are shared in the default
layout. Filling one disk produces ENOSPC there; memory pressure can affect peers. No data persists to the host.
Service names resolve through guest hosts files. The workload owns readiness
retries; Compose dependency ordering is not implemented.

For a negative control, set `SEEDBOX_PLANT_FAILURE: "1"` in the Redis workload,
or append `--plant, missing` to the etcd client's command list. The expected result
is FAIL; a fresh replay must reproduce that FAIL with MATCH. Use a Python scenario controller for runtime stop/restart and partition/heal
operations; static fault schedules remain unsupported. Only that controller
receives the fault socket. Replay uses the recorded layout and disk limits;
older guest ABIs require the Seedbox binary that recorded them.
