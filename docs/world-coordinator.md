# Shared world coordinator

`seedbox-engine-core::world` owns machine selection, causal slice limits, event
routing, network counters, control and trigger admission, fault ordering,
incarnation orchestration, launch/time forks and world snapshot restoration.
KVM calls this coordinator through `seedbox-vmm/src/world_adapter.rs`; its former
world loop is removed. ARM also calls it through `application/world_adapter.rs`;
its worker owns physical CPU/PV execution and device state, with no second
scheduler inside the worker.

`WorldMachine` supplies physical boot, stopped execution, delivery, CPU/RAM
capture and restoration, and architecture consequences of lifecycle operations.
The clock, factory, boot inputs and snapshot types are opaque to the coordinator.
Transport-facing primitives are fallible. Timer reseeding uses the same explicit seed/domain as KVM, while ARM
declares fixed PV cost rather than inventing randomized syscall-speed support.

## One machine record

Each real machine or IPC proxy embeds one core-defined `MachineState`. There is
no second mutable scheduling copy to synchronize after a slice. Physical execution
commits time and CPU state; the core orders lifecycle operations. Core lifecycle
helpers own their guards and metadata edits. Adapters invoke them at explicit
commit sites, preserving the original order around fallible CPU completion,
timer expiry and witness writes. For example, parking checks owed I/O first,
completes the architecture exit, and only then marks the machine parked. Resume
updates paused/time state before timer expiry, as the reference KVM path did.

IPC workers must validate generation/incarnation and stopped observations before
updating the proxy. Shared validation also rejects changed identity/lifecycle,
backwards time, time past the requested hard limit, inconsistent stop/state and
out-of-slice or reordered effect timestamps before consuming returned effects.
A worker binds its immutable process generation and initial machine identity at
Boot. Every request carries a sequence and expected identity; every response
carries before/after identity and a physical stopped observation. A malformed,
stale or unexpected response poisons the transport before effects are published.
Poisoning closes, kills and reaps the process; later calls send no requests.

Both adapters construct worlds with `World::new(WorldInputs { ... })`. The
constructor owns queue, counter and incarnation-bookkeeping initialization and
validates admission before returning. Coordinator policy fields are private;
report projection uses immutable accessors and `machine_with_history` for split
borrows. Lifecycle fields in `MachineState` are private to the core; adapters use
its guarded commit methods and read-only accessors.

World admission rejects empty machine sets, mismatched parallel inputs and early
slack greater than or equal to lookahead. Raw worker inputs preserve main's
handling of empty or duplicate names and MACs; Compose supplies unique names.

## Stops and effects

A slice never passes its requested hard limit, even to pay previously committed
I/O cost. A time fork may explicitly request a later stopping point to settle that
cost, but the limit remains clamped by earlier control, trigger, fault, strict
reply and delivery boundaries. Parking and capture require no owed I/O.

Adapters return at the first causal stop. Frames carry that stop's committed
time. Committed console observation stops on the first relevant byte; an adapter
must not scan a horizon-sized log batch and reconstruct trigger timing afterward.
The existing effect channels preserve reference processing order: console,
fault acknowledgements, exit availability, control requests, fault observation,
then frames. This is not a promise that arbitrary batched effects can be reordered
into a single global stream. `Stop::Control` may represent console or fault effects
as well as a control request.

Equal-time coordinator boundaries retain control, then trigger, then fault
precedence. Strict replies are never delivered early, including a reply owed
before a fatal control failure terminates execution. Shared finalization cuts off
control and trigger evidence at the designated end machine's committed exit time
(or the duration boundary), even when another machine ran ahead by lookahead. A crash discards the dead
incarnation's owed I/O and management queue while retaining world-transit frames;
reboot drops frames that arrived before the new NIC existed. Pause/resume at a
stopped boundary preserves any pending completion and remaining I/O cost; it does
not impose the no-owed-I/O requirement of parking.

ARM crash succession uses an owned temporary transfer artifact. The old worker
captures evidence and applies seeded disk survival exactly once, then exports the
stable overlay with its original base identity and destroys its VM. The fresh
worker has entered no guest instructions. It validates the configured base,
capacity, fault policy, ordered in-range sectors, digest and complete artifact
before installing it. The transfer binds predecessor, intended successor and
transaction identities; installation is a one-shot transition. The artifact is
removed on success or failure. Raw image rebasing is avoided because it would
change the retained disk identity.

Any crash-transaction failure immediately terminates owned worker transports,
preserves the original error with cleanup context, and marks World terminal.
Retaining World after that error does not permit execution or another replacement.
In-process KVM explicitly has no worker transport to terminate; its VM resources
remain governed by normal ownership. Partial worker boot owns its own cleanup.

## ARM capabilities

Cold run/replay use no launch barrier or snapshot. Explicit launch release requires
the real `seedbox.barrier=1` handshake and physical time/entropy reconciliation.
Deterministic time parking requires a completed stopped boundary without owed I/O.
Release from arbitrary time parking and full snapshot capture/restore/fork reuse
remain unsupported and return errors; empty snapshots or metadata-only restoration
are never successful substitutes.

ARM report rows for a machine currently down describe the retired predecessor's
evidence, incarnation and time. The sole scheduler record describes the successor
waiting to reboot; report projection never rewrites that state.

## Forks and validation

The world snapshot retains machine snapshots, event queues, sequence numbers,
network counters and topology. A normal boot fork retains readiness and rebuilds
control/fault plans for the next run seed. If triggers exist, their partial
matchers, scheduled effects, control and fault state are retained instead. A
machine that crashed since capture is rebuilt before restoring the original
incarnation. Restore keeps the original lifecycle and outbox reset behavior.

Portable scripted-machine tests cover selection, halted wake keys, equal-time
ordering, strict delivery, errors with owed replies, crash/reboot, forked I/O,
trigger matcher/effect restoration, readiness reset and invalid adapters. The
extraction is also checked against the actual pre-extraction KVM coordinator with
identical mocked physical primitives and retained state/trace receipts. These are
policy and extraction checks on Mac, not Linux CPU/KVM qualification. The existing
real KVM tests remain for the final Linux merge train.
