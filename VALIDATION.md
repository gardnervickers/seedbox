# Validation on 2026-10-05

Tested on macOS, Apple Silicon (`aarch64-darwin`), with the project Nix lock.
The Rust port uses Rust 1.98.1, native Unicorn 2.1.4 with Rust bindings 2.1.5,
iced-x86 1.21.0, and goblin 0.10.7. The other declared Nix systems have not been
tested on their corresponding hosts.

| Check | Observed result |
| --- | --- |
| Execution suite | 47 Rust tests passed; separate real Redis integration test also passed using pinned fixtures |
| Format and lint | Cargo format check and Clippy with warnings denied |
| Clean Nix execution check | Builds the Rust executable and runs the same suite, including the 64-seed host-load check |
| Built Nix CLI | Completed the Redis TTL workload; report byte-identical to the development-shell run |
| Fresh-process reruns | Entire reports byte-identical |
| Different host timezones | Entire reports byte-identical |
| Rerun with two additional host CPU workers | Entire report byte-identical |
| Intentional two-thread race, seeds 0–63 | 64 failures of the counter invariant; values ranged from 38 to 58 |
| Atomic fix, seeds 0–63 | Counter invariant passed in all 64 runs |
| Total UDP packet loss | Expected deterministic deadlock |
| Seed 42 cluster | Completed 130 execution steps in 1,000,016,020 ns of virtual time |
| Python/Rust port equivalence | Full event trace, guest results, instruction digest, and virtual time match for the cluster, race, and atomic seed-42 examples |
| Real Redis 7.2.7 with bundled hiredis client | TTL suite passed at seed 42 and seeds 0-7 |
| Redis TTL coverage | Missing/persistent TTLs, PTTL before expiry, lazy expiry, PXAT, PERSIST, active expiry notification, BLPOP timeout, clean shutdown |
| Redis fresh processes, host timezones, two CPU workers | Full reports byte-identical at seed 42 |
| Incorrect Redis TTL negative control | Client failed GET after the intended deadline; reported guest_failure |
| Native TCP regression | FIFO delivery despite jitter, partial reads, epoll wakeups, exit EOF, and resets of both peers |

The seed 42 cluster's entered-instruction digest was
`50b00b04e3b86d0f16aafd325ed1916ea94625c9383b49086f17bd32c5651ab5`.
Its event-trace digest was
`0d9056c7c8fd6654f3009b6f4707d31e0ebc6d3b3c0e5c1ae9bf7ab0a222e38e`.
Report digests also depend on image bytes and the implementation fingerprint;
use the full report for identifying a reproducer.

The real Redis seed-42 run executed 8,083,866 guest steps and completed at
1,272,741,868 ns of virtual time. Its client output was:

```text
PTTL before sleep: 249
PASS lazy expiration
PASS absolute expiration
PASS persisted key
PASS active expiration after 182 virtual ms without reading the key
PASS blocking command timeout after 300 virtual ms
PASS Redis TTL suite
```

Its entered-instruction digest was
`99c080eb7db5a088546103748413bc73cbf5feac77c7f70c998dfe754dd04baf`.
Its event-trace digest was
`9c1ff5def2448c8a8c1766a07e92f727a0e698191a0294fef97608c012aac0ee`.
The complete Redis acceptance driver took about 22.0 host seconds for three fresh
seed-42 executions, eight other seeds, and the negative control on this host.
Host timing is recorded only in its acceptance summary. That measurement is
specific to this workload and machine.

The Redis binaries were built through the pinned Nix fixture derivation on the
x86-64 NixOS workstation and then executed by the Rust runtime on this Mac.
Redis source is unmodified; its allocator is libc and persistence is disabled.
The negative control changes the client's requested TTL from 250 ms to 60 s,
which must fail its post-sleep GET check. It is not a canned expected trace.

The original Python CLI help command worked within the Codex command sandbox,
but guest execution there terminated with SIGILL. Its execution outside that
command sandbox succeeded. The Rust acceptance suite runs outside the Codex
command sandbox in the Nix development environment and clean build.
Unicorn uses executable memory for its instruction engine; compatibility with
an additional OS sandbox needs separate verification.

Reproduce the checks from the repository root:

```sh
nix develop --command cargo test
nix develop --command cargo run --release -- verify --seeds 64
nix develop --command cargo run --release -- verify-redis --seeds 8
SEEDBOX_REDIS_FIXTURES="$PWD/build/redis" nix develop --command cargo test --release --test redis -- --ignored
nix flake check
```

The Rust acceptance driver writes local reports and a summary under `artifacts/`.
Redis reports and acceptance summary are under `artifacts/redis-*.json`.
These results cover the explicit prototype ABI, small native workloads, and the
static musl Redis/hiredis workload. Broad C/Rust runtime compatibility, container
workloads, Redis RDB/AOF recovery, cross-host equality, and production-scale
service throughput remain future acceptance milestones.
