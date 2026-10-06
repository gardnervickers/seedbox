# Seedbox simulation example

This project is self-contained. The etcd and Redis templates passed real workloads,
intentional failure controls, and exact replays on the supported Linux amd64 AMD Zen
workstation with the controlled Seedbox kernel and 1024 MiB RAM per service.
See the repository's `sim/seedbox/ACCEPTANCE.md` for the tested image/kernel hashes.
This establishes those image versions, not compatibility with arbitrary OCI images.

The etcd template packages Seedbox's existing Go acknowledgement-ledger workload
in a Dockerfile and describes three registry-image members in Compose. The Redis
template builds a dependency-free Rust workload that checks PING, SET acknowledgements,
and exact GET values. Both use the Rust Seedbox configuration/orchestration APIs.

On the supported Linux amd64 host, after building Seedbox and its static guest
supervisor in `nix develop ./sim`:

```sh
seedbox validate compose.yaml
seedbox run compose.yaml --seed 1009 --cpu 2 \
  --kernel /path/to/bzImage --vmm /path/to/seedbox-vmm \
  --guest-init /path/to/compose-init --busybox "$SEEDBOX_BUSYBOX"
seedbox inspect .seedbox/runs/ATTEMPT --json
seedbox replay .seedbox/runs/ATTEMPT --cpu 2
```

Preparation unpacks images with root ownership and runs before the CPU lease.
Choose an available qualified CPU. Run/replay must be invoked with the required
KVM/MSR access; see the repository's `sim/seedbox/RUNTIME.md` for setup and limits.

All application storage, including Redis AOF and etcd data directories, is private
RAM-backed storage recreated on each fresh run/replay. No data persists to the host.
Service names resolve through guest hosts files. The workload owns readiness
retries; Compose dependency ordering is not implemented.

For a negative control, set `SEEDBOX_PLANT_FAILURE: "1"` in the Redis workload,
or append `--plant, missing` to the etcd client's command list. The expected result
is FAIL; a fresh replay must reproduce that FAIL with MATCH. Nonempty fault
schedules remain unsupported until their guest effect timing is verified.
