# Design

## Progress is part of time

A clock-reading loop must not outrun its next event, but a computation-only loop must advance too.
PV bounds clock reads; translated application blocks periodically charge kernel progress.
The shared [budget](../crates/seedbox-spec/src/progress.rs) owns validation, defaults and translator arguments.
Neither host elapsed time nor watchdog interruptions count as guest progress.
[Mechanics](internals.md#bounded-clock-reads-and-progress) cover the paths outside instrumentation.

## Replay and fresh execution answer different questions

The oracle checks application behaviour; a witness comparison checks reproducibility, not correctness.
Guest properties and crash reports qualify the oracle's verdict: violations, unmet requirements and lost evidence are the only property results kept, and they arrive with the final report ([properties](scenarios.md)).
Replay uses retained runtime inputs, so MATCH alone cannot validate a changed engine.
Independent fresh runs exercise the new engine; comparison first checks resolved input compatibility.
[Comparison](../crates/seedbox-server/src/execution.rs) ignores image names and descriptive host/engine metadata,
not launch contracts or binary inputs. Completion freezes definitions and result evidence in the server;
comparison never reconstructs them from attempt paths. Missing evidence is NOT_COMPARABLE, not a mismatch.

## Parallelism has a causal bound

Machines execute concurrently only inside a window bounded by minimum network latency.
No frame emitted inside that window can arrive before the round commits.
Commit orders output by occurrence, not worker completion; worker count cannot change results.
The [Driver](../crates/seedbox-driver/src/driver.rs) owns that bound, not the CPU backends.

## Checkpoints exclude their observers

A checkpoint file is a self-contained starting point for a new simulation on a fresh server.
It carries execution state, the input program and required binary assets; its source simulation
is provenance, never a prerequisite. It retains playback intent, including a fault held before dispatch.
Capture pauses briefly and restores the source playback rate afterward. `run FILE` resumes
the saved execution; `run FILE --paused --detach` retains the captured boundary for inspection.
Checkpoints do not retain log/event/frame sessions; restore starts fresh observers.
Draining, evicting or resizing observations therefore changes neither continuation nor policy evidence.
The [capture boundary](internals.md#checkpoint-capture) preserves input cursors rather than replaying events.

## Definitions do not borrow resource ownership

A resolved definition owns its binary closure by digest; source IDs and image names are provenance.
Deleting the source simulation does not invalidate a checkpoint branch, but YAML alone cannot move its bytes.
The catalogue and execution scratch are session-scoped; the binary cache is not.
A [portable archive](../crates/seedbox-artifacts/src/archive.rs) carries the closure without restoring source ownership.
