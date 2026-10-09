# Seedbox vision

**Any distributed-systems bug you could hit in production, you can find before
you land, reproduce with one command, and debug across time in the tools you
already use.**

The full story is in [issue #25](https://github.com/gardnervickers/seedbox/issues/25),
along with the gap from today to each capability and the hard problems. It
follows a developer changing an S3-backed, cross-region Paxos log, with MinIO as
the S3 dependency. They write a scenario, explore it under many seeds, narrow a
failure that changes an acknowledged append, debug it across time, and check
the fix. This page keeps the parts that guide day-to-day decisions. The current
[runtime](runtime.md) supports Compose/OCI run and replay, seeded scheduling,
and a Python scenario API for scoped network and process faults. The
[acceptance record](acceptance.md) states what has been tested; the
[backend contract](backends.md#capability-limits) states the
remaining capability limits. Builder plugins, broader faults and topology,
exploration, portable bundles, snapshots, reverse debugging and `verify`
remain future work tracked in issue #25.

The architecture stays as it is: real Linux machines on KVM, one host thread per
world, virtual time from charged guest progress, and seeds that choose every
scheduling, timing and fault decision.

## Division of labour

Seedbox owns the mechanisms: deterministic machines, network and process
faults, the seeded random source, recording, replay, debugger access across
time, and its own metrics. You own what only you can know: the workload, the
faults that matter, the checks, and what your scenario outputs. Seedbox makes
those easy to write with a library and patterns, but doesn't generate or
interpret them.

## Requirements

1. **Start from what exists.** A Compose file and the repository's own build are
   enough. Every runtime input has a default. Seedbox owns deterministic
   execution without application-specific flags, environment overrides, source
   changes or recompilation. Existing images keep their normal entrypoints and
   arguments, including applications with threads, shared libraries and JIT code.
   Runtime-specific predictability flags may help diagnose a bug, but cannot be
   required for correctness or substitute for testing an unmodified application.
2. **Dependencies are ordinary services.** MinIO, a mock you supply, or anything
   else runs inside the world under the seed. Seedbox doesn't ship or verify
   them. A mock can expose hook points over the transport from
   [#24](https://github.com/gardnervickers/seedbox/pull/24).
3. **Network and process faults belong to Seedbox.** Partitions, drops, latency,
   pauses, kills and clock skew work for every service with no cooperation from
   it.
4. **You own the workload, faults and checks.** The library supplies seeded
   randomness, history recording, standard checkers, fault patterns and
   triggers on log markers, hook points and connection events.
5. **Output is yours; metrics are shared.** The scenario returns a verdict and
   outputs whatever else it likes; an optional signature groups failures.
   Seedbox adds simulated duration (an estimate of real elapsed time including
   injected latency), per-link traffic and drops, faults applied and host cost.
6. **A failure is an artifact.** A content-addressed bundle reproduces the run
   exactly on a capable host, against the retained build that failed.
7. **Narrow, don't shrink.** Protocol-free rules prune faults that can't matter,
   and parallel counterfactual runs mark the rest as likely necessary or
   inconclusive. Shrinking is out of reach: runs are chaotic.
8. **Debug in your own tools, across time.** gdbserver and DAP endpoints for any
   guest process at any simulated time. Stopping one machine stops the world;
   stepping backwards replays from a snapshot. Seedbox doesn't decode protocols
   or draw traces.
9. **Observation never perturbs.** Logs, breakpoints and stepping cannot change
   the outcome.
10. **Fixes are checked in your terms.** The bundle replays on the base, a
    targeted scenario you write forces the window and reports whether it was
    reached, and exploration at an equal budget finds no new signatures.
11. **Fast enough for the inner loop.** Warm boot snapshots, forks per seed,
    parallel seeds across a fleet and incremental image preparation.

## Order of work

1. **Integrated simtest** (the current milestone): seeded races and service
   faults with a reproducible failing seed and a fixed control.
2. **Scenarios and output**: build on the [Python fault API](../python/README.md)
   with scenario verdicts and free-form output, Seedbox metrics, and the first
   library pieces.
3. **Network faults and topology**: regions with per-link latency,
   per-connection drops, pauses, clock skew and triggers.
4. **Speed**: boot snapshots and forks, parallel seeds, a remote fleet.
5. **Explore and narrow**: many seeds grouped by signature, bundles, pruning and
   counterfactuals.
6. **Debug across time**: merged logs, debugger endpoints, world-wide stop and
   reverse execution.
7. **Check and keep**: `verify`, regression scenarios in the repository and CI.

**Aspirational:** making a found failure reappear on a patched build. Same-seed
replay of a changed program diverges. Forking VM state before the window and
steering the new build with hook points might get close; it is not planned.

**Not goals:** generating workloads, checkers or fault plans; decoding
protocols or drawing traces; shipping or certifying service doubles.

When a change touches one of these areas, check it moves toward the
requirements above rather than away from them.
