# Seedbox

Seedbox is a working prototype of a seeded, deterministic application sandbox.
It runs real x86-64 Linux ELF machine code in Unicorn, preempts threads at bounded
execution steps, and models time, randomness, files, and networking in memory.
Each run starts fresh. The simulator never consumes an earlier execution trace.

The first milestone is **deterministic preemption and blocking**, including a
native two-thread data race and a three-process network/storage workload.
The current executable contract is static, fixed-address ELF with a small Linux
syscall subset. Docker image ingestion and general Linux application compatibility
are future work.

## Run it

Dependencies are pinned in `flake.lock`. From this directory:

```sh
nix develop --command python3 scripts/build_guests.py
nix develop --command python3 -m seedbox run examples/cluster.json --seed 42 --output artifacts/cluster-42.json
nix develop --command python3 -m seedbox run examples/race.json --seed 42 --output artifacts/race-42.json
nix develop --command python3 -m seedbox run examples/atomic.json --seed 42 --output artifacts/atomic-42.json
```

The cluster's client prints `pong`, its server prints `ping`, and its storage
process writes and flushes `/state`, reads seeded random bytes, and sleeps for one
second of virtual time. The one-second sleep requires no one-second host delay.

The race guest has two threads each increment a shared counter 32 times. Seed 42
produces **43** with ordinary loads/stores. The atomic variant produces **64**.
Both guests exit successfully; the counter is the application correctness oracle.
It appears as a four-byte little-endian value in `nodes.race.stdout_hex`.

An installed Nix entrypoint is also available after building the guests:

```sh
nix run . -- run examples/cluster.json --seed 42 --output artifacts/cluster-42.json
```

For an interactive development shell use `nix develop`; `.envrc` supports direnv.

## Determinism contract

A seed determines an execution together with the **same runner implementation,
engine/dependency versions, guest executable bytes, initial files, ordered node
definitions, arguments, and model configuration**. Reports fingerprint these
inputs, including the actual runner source. Keep `flake.lock` with a reproducer.

The scheduler, guest entropy, disk faults, and each directed network link have
separate SHA-256 random streams. No guest operation falls through to a host
syscall, host clock, host entropy source, or host socket. Unknown syscalls and
unmodeled clock/entropy instructions stop the run explicitly.

Reports use canonical JSON and contain:

- The normalized input manifest, seed, and manifest digest.
- An ordered event trace with virtual timestamps and a trace digest.
- A digest of the entered guest instructions, identifying node, thread, PC, and
  instruction bytes, even when detailed instruction tracing is disabled.
- Exit states, captured stdout/stderr, and volatile/durable file contents.

Add `--instruction-trace` to include every entered execution step. This changes
the report's verbosity and manifest while preserving scheduling, instruction
digest, and application results within the trace size limit. No host paths,
host PIDs, or wall-clock timestamps enter successful simulation reports.

Fresh Python processes with different hash seeds and additional host CPU load
have produced byte-identical reports on the tested host. Equality across host
architectures or engine upgrades still needs acceptance testing.

## Validation

```sh
nix develop --command python3 -m pytest -q
nix develop --command python3 scripts/verify.py --seeds 64
nix flake check
```

The suite covers exact preemption budgets, CPU context resumption, preemption
within a `REP` copy, syscall boundaries, shared-memory threads, futex timeouts,
event delivery while a CPU-only thread runs, virtual sleeps, disk faults,
partitions, invalid guest pointers, and rejection of unmodeled operations.

`scripts/verify.py` checks fresh-process and host-load reruns byte for byte. It
then sweeps the intentional race and its atomic fix across 64 seeds and checks
the deadlock caused by total packet loss. It writes
`artifacts/verification.json`; performance timing belongs only in that
verification artifact. `nix flake check` runs both the suite and this acceptance
script in a clean build.

## Describe a world

Executable paths are relative to the manifest file. For example, save this as
`world.json` in the repository root:

```json
{
  "schema": 1,
  "epoch_ns": 1577836800000000000,
  "scheduler": {
    "tick_ns": 10,
    "quantum_min": 3,
    "quantum_max": 17,
    "max_instructions": 100000
  },
  "disk": {"latency_ns": 5000, "error_ppm": 0},
  "network": {
    "latency_ns": 10000,
    "jitter_ns": 500,
    "loss_ppm": 0,
    "duplicate_ppm": 0,
    "partitions": [
      {"source": "client", "destination": "server", "start_ns": 1000, "end_ns": 1000000}
    ]
  },
  "nodes": [
    {
      "name": "client",
      "ip": "10.0.0.1",
      "executable": "build/guests/client",
      "argv": ["/app"],
      "files": {"/config": "initial text", "/bytes": {"hex": "00ff"}}
    },
    {"name": "server", "ip": "10.0.0.2", "executable": "build/guests/server"}
  ]
}
```

Seeds are unsigned 64-bit integers. Fault probabilities are integer parts per
million: `1000000` always injects the fault. Partitions are directed and active
over `[start_ns, end_ns)`; a partition can drop a packet already in flight.
Network latency is clamped at zero after uniformly sampled jitter. A duplicate
arrives one virtual nanosecond after the original.

The CPU tick is a simulation policy, currently 10 ns per execution step. It is
not a hardware performance estimate. `REP` iterations count as separate steps
when Unicorn exposes them to the code hook. Pending events shorten execution
budgets; an event between ticks runs before the next instruction.

The CLI returns `0` for completed zero-exit guests, `1` for a failed simulation
outcome, and `2` for invalid inputs. Outcomes include `guest_failure`, `deadlock`,
`instruction_limit`, `unsupported`, `fault`, and `simulator_error`. A zero guest
exit code is independent of an application invariant, as the race demonstrates.

## Current Linux ABI

| Area | Implemented contract |
| --- | --- |
| Images | ELF64, x86-64, little endian, `ET_EXEC`, no interpreter or dynamic segments; deterministic stack, arguments, empty environment, and auxv |
| Threads | Shared-address-space `clone`, deterministic virtual PIDs/TIDs, TLS base for clone, `set_tid_address`, exit/exit-group, `sched_yield` |
| Synchronization | `FUTEX_WAIT`/`FUTEX_WAKE`, including private variants, relative timeout, clear-child-TID wake |
| Time and RNG | `clock_gettime` for realtime/monotonic, `nanosleep`, `getrandom`, seeded `AT_RANDOM` |
| Files | `openat(AT_FDCWD)`, `read`, `write`, `close`, `lseek`, `fsync`; per-node memory files with completion latency and injected `EIO` |
| Network | IPv4 UDP `socket`, `bind`, `connect`, `sendto`, `recvfrom`, socket `read`/`write`; blocking/nonblocking receives, seeded latency/loss/duplication/partitions |
| Output | Captured stdout/stderr; stdin starts at EOF |

The file model has a flat normalized path namespace. Ownership, permissions,
directory operations, and device files are not modeled. Read/write/fsync are
atomic at completion; fsync copies current file contents into the durable
image. There is no crash/restart or torn-write model yet.

The UDP model has no host traffic, DNS, TCP, congestion control, or ICMP. It
models a bounded datagram queue, automatic ephemeral ports, and a restricted
set of socket flags. It does not implement every connected-UDP or descriptor
lifetime detail of Linux.

Memory mapping/allocation syscalls, libc startup support, dynamic loading,
signals, process forks, epoll, io_uring, and Docker/OCI loading are not
implemented. The current fixtures use assembly and direct Linux syscalls.
One logical CPU executes at a time, so this explores thread interleavings under
a sequential memory model. Weak-memory reorderings need a separate model.

This prototype is a test harness, not an audited security boundary for hostile
binaries. See [DESIGN.md](DESIGN.md) for module interfaces, invariants, and the
compatibility decision before expanding it to container applications.

Runs are bounded to 32 nodes, 256 threads per node, 1 MiB per I/O operation,
32 MiB per file/output stream, and one million trace events. The default global
execution limit is 100,000 steps. See [VALIDATION.md](VALIDATION.md) for the tested
host and acceptance results.
