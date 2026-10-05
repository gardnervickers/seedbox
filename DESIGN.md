# Execution architecture

The Rust coordinator owns every execution choice visible to the guest. Native code
runs only inside an instruction emulator. Resource operations either return
immediately or block a logical thread until the coordinator delivers a modeled
completion. All state is rebuilt from the manifest for each run.

```mermaid
flowchart LR
    M[Images, initial files, configuration, seed] --> S[Coordinator]
    S -->|thread and execution budget| B[Unicorn execution backend]
    B -->|syscall, preemption, fault| S
    S --> K[Linux ABI models]
    K --> F[In-memory files]
    K --> N[In-memory UDP fabric]
    K --> T[Virtual clocks, entropy, futexes]
    F --> E[Ordered completion queue]
    N --> E
    T --> E
    E --> S
    S --> R[Deterministic report]
```

## Existing interfaces

| Module | Responsibility and hook points |
| --- | --- |
| `backend.rs` | ELF loading, guest memory, CPU contexts, `run(tid, budget)`, `complete(tid, request, result)`, `clone_thread`; typed execution stops |
| `runtime.rs` | Runnable-thread selection, instruction budgets, virtual time, event queue, thread lifecycle, result reporting |
| `kernel.rs` | Linux ABI decoding and resource state; syscall handling returns the `Action` enum with `Complete` or `Blocked` variants, a typed wait kind, and optional deadline |
| `linux.rs` | Static libc ABI support, deterministic process metadata, TLS setup, signal registration/masks, memory management, and vectored file/output operations |
| `network.rs` | TCP streams, anonymous pipes, level-triggered epoll/poll, readiness wakeups, and socket lifetime handling |
| `random.rs` | Versioned SHA-256 streams, named domains, unbiased bounded choices |
| `config.rs` | Validated starting inputs and model policy; strict JSON parsing |
| `linux_errno.rs` | Guest Linux errno values, independent of the host OS |
| `verification.rs` | Native acceptance driver for fresh-process reruns, temporary host-load workers, and seed sweeps |

These are working boundaries in the code. The backend currently targets x86-64
and Unicorn; there is no general backend plugin API yet. A different emulator
must preserve the budget and syscall-stop contract and pass the existing
acceptance workloads.

### Preemption

The scheduler chooses a runnable logical thread, then a step budget, using its
own seeded stream. The budget is bounded by the global execution limit and the
next timed event. Unicorn executes with a fixed count and no host-time timeout.
The native callback counts steps and uses iced-x86 to reject unmodeled
instructions before they run. It collects entered instructions for the
coordinator's hashing and optional trace. No Python callback is involved.

At a syscall, the backend stops before executing the instruction. The
coordinator counts that boundary as an execution step, decodes the six Linux
x86-64 argument registers, and invokes the resource model. An immediate result
updates the saved CPU context and advances past the syscall. A blocked result
keeps the thread at the syscall until a later completion performs the same
update. RCX and R11 receive the expected syscall clobbers.

Shared-memory threads use separate saved CPU contexts and one address space.
Other nodes have separate address spaces. The race workload uses the actual
`clone` instruction path and shared guest memory, so scheduling can interrupt
its load/increment/store sequence. The corrected workload uses `lock inc`.

An execution step is a Unicorn code-hook entry. It normally corresponds to one
x86 instruction; repeated string operations expose individual iterations.
Faulting or rejected instructions can be counted at entry before any effects.
The tests establish exact budgets and resumable contexts for ordinary loops
and `REP` copies.

### Virtual time and completion order

Time starts at zero. Every entered execution step advances time by `tick_ns`.
Modeled syscalls observe time after their boundary step. Disk operations,
packets, sleeps, and futex timeouts insert events ordered by
`(deadline_ns, insertion_sequence)`. Equal deadlines therefore have a stable
order.

Execution stops early enough to deliver each pending event exactly at its
deadline. For an event between CPU ticks, time moves to the deadline before
executing the next step. If no thread is runnable, time advances directly to
the next event. If no thread can run and no event remains, the result is a
deterministic deadlock.

Futex wakeups and packet deliveries resume logical threads; they never wake
host execution threads. Obsolete timeout events are discarded and cannot move
virtual time. A syscall result is completed once using a monotonically assigned
wait identity, which prevents a stale timeout from completing a later wait.

### Randomness

The random-stream key combines a version marker, the unsigned 64-bit seed, and
a UTF-8 domain. SHA-256 of that key and a monotonically increasing block counter
provides buffered bytes. Bounded choices use rejection sampling.

The domains are:

- `schedule` for thread and budget selection.
- `entropy:<node>` for the initial `AT_RANDOM` bytes and guest `getrandom`.
- `disk:<node>` for operation faults.
- `network:<JSON [source, destination]>` for each directed link's loss,
  duplication, and jitter choices.

This keeps a change in network fault policy from consuming guest entropy or
scheduler randomness. It does not preserve the same schedule after a change
alters which threads are runnable. Run identity includes all model settings.

### Files and networking

Each node begins with private byte arrays for its files. File handles track
offsets and flags. Read, write, and fsync block until a timed completion; a write
captures its submitted bytes. Successful operations apply atomically at that
completion, and fsync takes a durable copy. Injected EIO applies no operation
effects. Opening, truncation, close, and seek are immediate in the current
model.

The fabric maps logical IPv4/port bindings to modeled sockets. Sending captures
the payload, chooses link faults and latency, and inserts packet events.
Delivery either completes a blocked receiver or appends to its bounded queue.
Partitions are checked at submission and delivery. Packets addressed outside
the modeled world are dropped, and nothing connects to a host network.

These are intentionally small resource models. The durable image supports
future crash testing, but process restart and persistence semantics are not yet
implemented. TCP streams now preserve FIFO data and FIN ordering despite jitter,
allow partial reads, and wake blocking reads and epoll/poll waits. Connection
establishment is immediate; data and FIN use timed events. Faults reset both
endpoints. Retransmission, congestion control, and send backpressure are outside
this stream model. Anonymous pipes use the same byte queues with immediate local
delivery. Process exit closes descriptors so waiting peers observe EOF.

Epoll interests are ordered by descriptor. A readiness wait completes when a
resource transition produces a requested event or when its virtual deadline
arrives. Canceled timer completions are discarded using the existing wait IDs.
This lets Redis's unmodified event loop run its own expiration and blocking
command timers as the coordinator advances time.

## Acceptance criteria for this milestone

1. A guest that never enters the kernel is preemptible and resumes without
   losing or duplicating execution steps.
2. Real guest threads share memory and expose an intentional lost update across
   seeds; changing the guest to an atomic increment restores the invariant.
3. Multiple guest processes exchange messages and block on disk, network,
   synchronization, and virtual timers without using host resources.
4. Timed events occur at the requested virtual time even while a CPU-only guest
   remains runnable.
5. A seed and pinned inputs regenerate the complete report in a fresh process,
   including under additional host CPU load.
6. Unknown operations stop explicitly, and guest errors use the Linux ABI
   independently of the host OS.

The Rust tests and acceptance driver exercise these criteria. Their successful
results are evidence for the supported execution contract, rather than a claim
that arbitrary Linux applications already run here.

## The compatibility decision

Unicorn supplies the instruction engine and saved CPU contexts without a host
kernel executing the guest. This makes preemption controllable on the current
Mac and leaves ABI behavior in ordinary code we can inspect. Its primary
upstream references are [the Unicorn project](https://github.com/unicorn-engine/unicorn)
and [its examples](https://www.unicorn-engine.org/docs/tutorial.html).
The runner uses [the upstream Rust bindings](https://docs.rs/unicorn-engine/2.1.5/unicorn_engine/),
with the native library supplied by Nix, and uses goblin for ELF loading.

Rust owns the scheduling, resource models, and reports. Ordered maps provide
explicit stable iteration, and enums describe syscall actions, wait states, and
timed events. The prototype's per-instruction Python boundary has been removed.
The initial Python source remains only as an archived reference; healthy native
workloads have full-trace regression expectations from that implementation.

The static-runtime milestone now uses pinned Redis 7.2.7 built with musl and
`MALLOC=libc`. A native client uses Redis's bundled hiredis. Allocation, TLS,
thread creation, process metadata, entropy, TCP, and epoll were added from real
startup/execution requirements. The race and cluster regression oracles remain.
The client covers lazy and active expiration, absolute deadlines, persistence
of a key's value after removing its TTL, and blocking-command timeout. A wrong
TTL is a negative control. The server's RDB/AOF persistence is disabled.

Next, broaden compatibility using representative applications, including
fork-based persistence and ordinary dynamically linked images, and measure
engine cost before deciding the long-term execution backend. There are two
substantive paths:

| Path | Benefit | Cost to establish |
| --- | --- | --- |
| Expand the user-mode ABI models | Direct control of syscall-level faults and scheduling; simpler isolated models | Memory management, ELF linking, libc/runtime expectations, TCP, readiness, signals, filesystem and process semantics |
| Run a Linux guest under a controlled full-system emulator | Reuse Linux ABI implementation and ordinary container tooling inside the guest | Deterministic kernel/vCPU scheduling and interrupts; modeled block/network devices; backend changes and fresh determinism acceptance |

An OCI image loader only solves input packaging. It cannot supply the missing
Linux ABI or guarantee deterministic execution. A full-system backend would
need resource adapters at the block-device and virtual-NIC boundary; today's
syscall adapters cannot simply be reused unchanged.

The current coordinator, seeded policies, event ordering, and workload oracles
are useful in either path. Broad container compatibility remains an engineering
decision to validate with representative programs. No record/replay mechanism
is required for either architecture.
