# Simulation inputs and timelines

A simulation is one execution of a complete specification: application
configuration, prepared inputs, a seed, and an ordered external input program.
The seed chooses deterministic randomness. It does not stand in for deliberate
inputs supplied by a user. Host playback controls change how quickly execution
runs, not the specification.

## Authored inputs and observed execution

An authored input has a stable key, simulated time, and action. Its key belongs
to the program. It has no execution fault ID until the coordinator admits it.
Future inputs do not reserve execution IDs or make a fault active early.

The coordinator owns the program cursor alongside the world. It schedules the
next input as an exact boundary, validates it against the current world, and
uses the same fault admission mechanism as other runtime actions. A checkpoint
captures that cursor and the resulting admissions with the world; restoring it
must neither replay consumed inputs nor lose future inputs.

At the same simulated time, existing guest-control and trigger ordering is
preserved. Program entries follow document order. Each program entry's due
effects are processed before the next entry is admitted, so a later entry can
refer to a handle established by its predecessor. No guest execution is inserted
between those entries. Fault effects, including guest acknowledgements, remain
subject to the engine's existing lifecycle and transport rules.

The admission evidence correlates the authored key with the assigned execution
plan. Logs and structured events describe observations; they are not another
source of execution instructions. Reports validate admissions against the
program rather than predicting IDs from the length of a future schedule.

## Replay and editing

One deterministic scheduler executes both unchanged and edited definitions.
Oracle-generated actions come from the oracle program. External overrides merge
with them at recorded effective simulated times, using stable ordering for ties.
Interactive admission captures the semantic frontier already processed by the
world, so a repeated override cannot move before earlier oracle effects.

The input cursor and admissions are part of world snapshots. Explicit keys identify
external overrides; distinct actions at the same time remain distinct. Reusing a
key with different contents is rejected. State-dependent rejection advances the
cursor and records evidence instead of corrupting the remaining program. An
oracle exit prevents admission of the unapplied suffix.

Editing creates a new experiment, whose unchanged prefix should reproduce until
the first changed action. It does not select a different execution mode. Removing
an earlier action does not automatically remove a later heal. Its behavior is
determined by the actual world state when it is due.

## Definition and client ownership

A resolved definition owns its complete inline configuration and binary asset
references. A provenance annotation may name a previous simulation, but it is
not needed for execution and does not cause cascading deletion into independent
simulations. Shared content remains available while retained by the server;
saved definitions require their referenced binary assets to be present.

The TUI holds an immutable base and one editable working copy. Changes update the
working copy; `w` saves a new immutable document, and Enter submits a snapshot via
the ordinary simulation submission API. Submission does not require a prior save
or lookup of the source simulation. Later edits do not mutate a submitted run.
Removing an already applied override only affects a subsequent run.

Timeline entries carry stable IDs, opaque action payloads, simulated time, origin,
and observed execution IDs when available. Oracle events are evidence, not a
second injection program. External program entries can be edited; observations
enrich them without restoring entries excluded from the working copy.

Forms, validation metadata, units, logs, and events come from the server API.
Backend types, private retained journal formats, and execution policy remain out
of the TUI.
