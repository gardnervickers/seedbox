# Design

## Progress is part of time

A clock-reading loop must not outrun its next event, but a computation-only loop must advance too.
PV bounds clock reads; translated application blocks periodically charge kernel progress.
Neither host elapsed time nor watchdog interruptions count as guest progress.
[Mechanics](internals.md#bounded-clock-reads-and-progress) cover the paths outside instrumentation.

## Replay and fresh execution answer different questions

The oracle checks application behaviour; a witness comparison checks reproducibility, not correctness.
Replay uses retained runtime inputs, so MATCH alone cannot validate a changed engine.
Independent fresh runs exercise the new engine; comparison first checks resolved input compatibility.
[Comparison](../crates/seedbox-server/src/execution.rs) distinguishes missing evidence from a mismatch.

## Parallelism has a causal bound

Machines execute concurrently only inside a window bounded by minimum network latency.
No frame emitted inside that window can arrive before the round commits.
Commit orders output by occurrence, not worker completion; worker count cannot change results.
The [Driver](../crates/seedbox-driver/src/driver.rs) owns that bound, not the CPU backends.

## Checkpoints exclude their observers

A checkpoint retains execution authority and playback intent, including a fault held before dispatch.
It does not retain log/event/frame sessions; restore starts fresh observers without releasing the hold.
Draining, evicting or resizing observations therefore changes neither continuation nor policy evidence.
The [capture boundary](internals.md#checkpoint-capture) preserves input cursors rather than replaying events.

## Definitions do not borrow resource ownership

A resolved definition owns its binary closure by digest; source IDs and image names are provenance.
Deleting the source simulation does not invalidate a checkpoint branch, but YAML alone cannot move its bytes.
The catalogue and execution scratch are session-scoped; the binary cache is not.
A [portable archive](../crates/seedbox-artifacts/src/archive.rs) carries the closure without restoring source ownership.
