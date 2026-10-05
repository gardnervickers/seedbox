# Seedbox

Seedbox is a Rust implementation of a seeded, deterministic application sandbox.
It runs real x86-64 Linux ELF machine code in Unicorn, preempts threads at bounded
execution steps, and models time, randomness, files, and networking in memory.
Each run starts fresh. The simulator never consumes an earlier execution trace.

The current milestone runs **real Redis 7.2.7 and a native hiredis client**,
including TTLs, background expiration, and blocking command timers. The original
thread-race and network/storage workloads remain regression tests. Executables
are static, fixed-address x86-64 ELF files. Docker image ingestion and broader
Linux application compatibility are future work.

## Run it

Dependencies are pinned in `Cargo.lock` and `flake.lock`. From this directory:

```sh
nix develop --command cargo run -- build-guests
nix develop --command cargo run -- run examples/cluster.json --seed 42 --output artifacts/cluster-42.json
nix develop --command cargo run -- run examples/race.json --seed 42 --output artifacts/race-42.json
nix develop --command cargo run -- run examples/atomic.json --seed 42 --output artifacts/atomic-42.json
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

## Redis

`redis-fixtures.nix` pins the upstream Redis source and builds unmodified Redis
with musl and `MALLOC=libc`, plus a client using its bundled hiredis library.
Build these x86-64 Linux fixtures on an x86-64 Linux Nix host:

```sh
seedbox_redis_fixtures=$(nix build --no-link --print-out-paths .#redis-fixtures)
mkdir -p build/redis
cp "$seedbox_redis_fixtures/bin/redis-server" "$seedbox_redis_fixtures/bin/redis-cli" "$seedbox_redis_fixtures/bin/redis-ttl-client" build/redis/
```

The three resulting ELF files can also be copied to `build/redis/` on this Mac;
they run under the same emulator. The current workspace already contains them.
Run the client/server world and its acceptance checks:

```sh
nix develop --command cargo run --release -- run examples/redis.json --seed 42 --output artifacts/redis-42.json
nix develop --command cargo run --release -- verify-redis --seeds 8
```

The client checks persistent/missing TTL values, `PTTL` before expiration, lazy
expiration with the background cycle disabled, absolute `PXAT` deadlines,
`PERSIST`, and active expiration through a keyspace notification without reading
the expired key. It also checks a `BLPOP` timeout and clean server shutdown.
All sleeps and Redis event-loop timeouts use virtual time. Redis's expiration
code is unchanged; the test controls its background cycle through Redis's own
debug command. The world enables that command for this test.

`verify-redis` regenerates the full report in fresh processes with different host
timezones and added CPU load, sweeps seeds, and verifies that an intentionally
incorrect TTL makes the client fail. Its reports and summary are written under
`artifacts/`. Persistence is disabled in this world; Redis RDB/AOF recovery and
fork-based persistence remain separate milestones.

## Determinism contract

A seed determines an execution together with the **same runner implementation,
engine/dependency versions, guest executable bytes, initial files, ordered node
definitions, arguments, and model configuration**. Reports fingerprint these
inputs, including the actual runner source. Keep `flake.lock` with a reproducer.
The source fingerprint also includes `Cargo.toml`, `Cargo.lock`, `flake.lock`, and
the build script, and is embedded in the executable at compile time. Reports
also identify the Rust compiler and native Unicorn version.

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

Fresh Rust processes with different host timezones and additional host CPU load
have produced byte-identical reports on the tested host. Equality across host
architectures or engine upgrades still needs acceptance testing.

## Validation

```sh
nix develop --command cargo test
nix develop --command cargo run --release -- verify --seeds 64
nix develop --command cargo clippy --all-targets -- -D warnings
nix flake check
```

The suite covers exact preemption budgets, CPU context resumption, preemption
within a `REP` copy, syscall boundaries, shared-memory threads, futex timeouts,
event delivery while a CPU-only thread runs, virtual sleeps, disk faults,
partitions, invalid guest pointers, and rejection of unmodeled operations.
It also checks anonymous mappings, TLS, invalidated pointers, TCP FIFO ordering,
partial reads, epoll wakeups, process-exit EOF, transport resets, pipes, and poll
timeouts. The real Redis acceptance test needs separately built fixtures:

```sh
SEEDBOX_REDIS_FIXTURES="$PWD/build/redis" nix develop --command cargo test --release --test redis -- --ignored
```

The x86-64 Linux flake adds `checks.x86_64-linux.redis`, which builds the pinned
fixtures and runs this acceptance test automatically. The portable regression
suite remains available without Redis binaries.

The `verify` command checks fresh-process and host-load reruns byte for byte. It
then sweeps the intentional race and its atomic fix across 64 seeds and checks
the deadlock caused by total packet loss. It writes
`artifacts/verification.json`; performance timing belongs only in that
verification artifact. Tests build the guests automatically; `verify` uses the
built guests. `cargo test` includes the 64-seed and host-load acceptance check.
`nix flake check` builds the native Rust executable, runs the suite, and checks
formatting and Clippy in a clean build.

The event traces and instruction digests of all three seed-42 examples match the
original Python proof of concept. These comparisons are checked against golden
files under `tests/golden/`; they are test expectations, never simulation inputs.
The original source is archived under `reference/python/` and is excluded from
the active Nix package. The Rust runtime and its acceptance suite need no Python.

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
Network latency is clamped at zero after uniformly sampled jitter. For UDP, a
duplicate arrives one virtual nanosecond after the original. TCP preserves byte
order despite jitter; transport loss or a partition resets the stream. Its
current model has no retransmission, congestion control, or send backpressure.

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
| Threads | Shared-address-space `clone`, deterministic virtual PIDs/TIDs, `ARCH_SET_FS` and clone TLS, `set_tid_address`, exit/exit-group with descriptor cleanup, `sched_yield` |
| Memory | Bounded `brk`, anonymous private `mmap`, `munmap`, and `mprotect`; deterministic addresses |
| Synchronization | `FUTEX_WAIT`/`FUTEX_WAKE`, including private variants, relative timeout, clear-child-TID wake |
| Time and RNG | Realtime/monotonic `clock_gettime`, `clock_getres`, `gettimeofday`, `time`, `nanosleep`, `getrandom`, seeded `AT_RANDOM` and `/dev/urandom` |
| Files | `open`/`openat(AT_FDCWD)`, `read`/file `readv`, `write`, `close`, `lseek`, `fsync`; memory files with completion latency and injected `EIO`; basic stat metadata |
| Network | IPv4 UDP and TCP, `bind`, `connect`, `listen`, `accept`/`accept4`, stream partial reads, FIFO delivery, EOF/reset, socket addresses and selected options |
| Readiness | Level-triggered epoll, `epoll_pwait` with no replacement mask, poll, anonymous pipes, descriptor nonblocking flags, virtual timeout and readiness wakeups |
| Libc startup | Signal action registration and per-thread masks, umask, virtual uname/sysinfo/resource metadata, affinity query, root working directory |
| Output | Captured stdout/stderr and output `writev`; stdin starts at EOF |

The file model has a flat normalized path namespace. Ownership, permissions,
directory operations, and general device files are not modeled. Read/write/fsync are
atomic at completion; fsync copies current file contents into the durable
image. There is no crash/restart or torn-write model yet.

The network models use no host traffic or DNS. UDP has bounded datagram queues
and automatic ephemeral ports. TCP models byte streams with immediate connection
establishment and delayed data/FIN delivery. Socket options are restricted to
those used by the fixtures. These models do not implement every Linux socket
behavior or a packet-level TCP stack.

Dynamic linking, file-backed mappings, signal delivery, process forks, io_uring,
and Docker/OCI loading remain unimplemented. Signal registration and masks allow
libc startup, but active signal-generating interval timers fail explicitly.
Vectored socket reads and general filesystem/process semantics are also outside
the current ABI profile. The fixtures include direct-syscall assembly and real
musl-linked Redis/hiredis applications.
One logical CPU executes at a time, so this explores thread interleavings under
a sequential memory model. Weak-memory reorderings need a separate model.

This prototype is a test harness, not an audited security boundary for hostile
binaries. See [DESIGN.md](DESIGN.md) for module interfaces, invariants, and the
compatibility decision before expanding it to container applications.

Runs are bounded to 32 nodes, 256 threads per node, 1 MiB per I/O operation,
32 MiB per file/output stream, and one million trace events. The default global
execution limit is 100,000 steps. See [VALIDATION.md](VALIDATION.md) for the tested
host and acceptance results.
