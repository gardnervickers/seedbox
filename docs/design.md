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
[Comparison](../crates/seedbox-server/src/execution.rs) ignores image names, descriptive host/engine metadata
and the quantum, not launch contracts or binary inputs. The prepared Driver inputs are binary inputs and record
the quantum they were prepared with, so a project prepared again with another quantum is NOT_COMPARABLE, while
a definition resubmitted with only its quantum edited shares its source's inputs and compares.
Completion freezes definitions and result evidence in the server;
comparison never reconstructs them from attempt paths. Missing evidence is NOT_COMPARABLE, not a mismatch.

## Parallelism has a causal bound

Machines execute concurrently only inside a window bounded by minimum network latency.
No frame emitted inside that window can arrive before the round commits.
Inside it each machine runs to the round's end on its own lane, taking its timers and arriving
frames at their exact times. A timer or a running receiver's frame therefore never ends a round;
frames for a paused receiver still do. A machine held at a breakpoint takes its frames at their
arrival without a budget, from its hit on, whichever commit carried the hit.
Commit orders output by occurrence, not worker completion; worker count cannot change results.
The [Driver](../crates/seedbox-driver/src/driver.rs) owns that bound, not the CPU backends.

Where a round ends is the host's choice, and four rules keep it out of a run's results.
Code comments cite them by label.

- **C1, consequences follow the output.** What the Driver schedules because of an output (a
  call's admission, a reply, a released command, a flush reply) is due one network latency
  after the output, or one control latency after a call when a controller is configured, never
  at the commit that carried it. So every runnable machine bounds the round.
- **C2, one order for an instant.** Outputs stamped in the same nanosecond take effect in
  (machine, emission) order. Output at an instant a machine can still reach waits for it. A
  barrier every schedule shares (a timeline item, the end of the run, a frame for a paused
  receiver, an input the host admits) first commits everything held, then applies what is due.
- **C3, the same runs for a machine.** However its runs are split, the Driver offers a machine
  the same inputs at the same machine times: a timer wake starts at the timer's own deadline,
  and a run that only continues from a stop nothing needed maps its times as if it had not
  stopped. A machine in turn leaves no trace of a stop that delivers nothing: no record, epoch,
  draw or adopted time ([PV](internals.md#bounded-clock-reads-and-progress),
  [TCG](internals.md#reference-tcg-machine)).
- **C4, the run ends after the oracle.** The run ends one network latency after the oracle's
  terminal output or exit, with every machine run to that time and its output up to then
  published. What is due before the end still happens; what is due at it does not.

So where rounds end, and the quantum, change no result. The quantum is the step of paced playback
and of single stepping, and caps rounds only then; otherwise a round runs to where the simulation
needs a barrier.

`network.latency` is therefore also a delay the oracle sees: an SDK call takes effect, a `flush`
is answered and the run ends one latency after the output that asked. A simulated time limit
has to cover the oracle's exit plus that latency: a run whose oracle exits within one latency
of its limit ends at the limit, not as finished.

## Checkpoints exclude their observers

A checkpoint file is a self-contained starting point for a new simulation on a fresh server.
It carries execution state, the input program and required binary assets; its source simulation
is provenance, never a prerequisite. It retains playback intent, including a fault held before dispatch.
Capture pauses briefly and restores the source playback rate afterward. `run FILE` resumes
the saved execution; `run FILE --paused --detach` retains the captured boundary for inspection.
Checkpoints do not retain log/event/frame sessions; restore starts fresh observers.
Draining, evicting or resizing observations therefore changes neither continuation nor policy evidence.
The [capture boundary](internals.md#checkpoint-capture) preserves input cursors rather than replaying events.

## A branch is a run

A host that saves a world and restores it many times still has only runs: each
restored world is defined by its machines, its seed and the inputs delivered
to it so far. [seedbox-branch](../crates/seedbox-branch/src/lib.rs) keeps that
true by keeping the input program outside the Driver and scheduling each input
at the barrier it falls due. A saved point then
holds no future input, and what follows it is whatever program its branch is
given.
Two things follow. A branch any number of saves deep is reproduced from boot by
its delivered inputs alone. And a line one run wrote at time `t` is written at
`t` by every run that takes the same inputs up to `t`, so its time is a place
to go back to: `Branch::rewind` restores an earlier point, replays the run's
own inputs to just before `t` and checks the replay against the run. An input
due at the point it returns to is delivered there, as it was in the run.
Stops and saves are not inputs, and neither changes what a run's machines do
(see the causal bound above). `Run::advance` stops exactly at the time asked
for, and `advance_watching` asks its watch every stride of simulated time, one
quantum unless told otherwise.
A stopped run's trace holds what its machines wrote before the Driver's
committed frontier (`Barrier::committed_before`): the stop, or the nanosecond
before it while a machine with a slow clock can still stamp output there.
`rewind` checks the replay against the run up to that frontier.
The trace compares lines and lifecycle changes, which have times of their own;
settlements are not compared.

## Definitions do not borrow resource ownership

A resolved definition owns its binary closure by digest; source IDs and image names are provenance.
A submitted checkpoint branch keeps its captured state when the source simulation is deleted; a fetched but unsubmitted branch definition does not, so submit it or export a bundle first. YAML alone cannot move its bytes.
The catalogue and execution scratch are session-scoped; the binary cache is not.
Deleting a simulation reclaims captured checkpoint state and imported binaries no remaining resource names; kernels and images stay cached.
A [portable archive](../crates/seedbox-artifacts/src/archive.rs) carries the closure without restoring source ownership.

`show definition` output can be edited and resubmitted, but it replays the world it prepared.
Edits the Driver reads at run time apply: seed, faults, schedule, network, quantum, limits, machine
command, environment and memory, and `allowCrashes` (except on a checkpoint start).
Edits baked into the prepared boot archives are refused with an error naming the field: added,
removed or renamed machines, `volumes`, `anonymousVolumes`, `clock.epochSeconds`,
`simulator.oracleControl` and, on PV, `simulator.blocksPerProgressCall`. Change those in the
project and submit it again.
