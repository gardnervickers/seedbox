# Seedbox vision

**Any distributed-systems bug you could hit in production, you can find before
you land, reproduce with one command, understand on one screen, and prove
fixed.**

The full story is in [issue #25](https://github.com/gardnervickers/seedbox/issues/25),
along with the gap from today to each capability and the hard problems. The
story follows a developer changing an S3-backed, cross-region Paxos log. They
choose how to handle the S3 dependency, find and shrink a lost-acknowledgement
bug, trace it causally, fix it and prove the fix. This page keeps the parts that
guide day-to-day decisions. The architecture stays as it is: real Linux
machines on KVM, one host thread per world, virtual time from counted guest
progress, and seeds that choose every scheduling, timing and fault decision.

## Requirements

1. **Start from what exists.** A Compose file and the repository's own build are
   enough. Every runtime input has a default. The first `seedbox run` needs no
   flags and no Seedbox knowledge.
2. **Dependencies belong to the world.** Anything the system talks to (S3, a
   database, an in-house service) runs inside the world under the seed, as a
   maintained service double with published fidelity or as your own double
   through the fault-point protocol.
3. **Faults are explored, not scripted.** You declare which fault families
   apply. Seedbox chooses, searches and shrinks. Semantic faults (an ambiguous
   commit, a stalled request, a pause) matter more than random packet loss.
4. **A failure is an artifact.** A content-addressed bundle holds the exact
   inputs, the shrunk scenario and the execution profile. One command reproduces
   it on any qualified host.
5. **Observation never perturbs.** Logs, traces, tracepoints and debuggers work
   after the fact on a replay and cannot change the outcome.
6. **Causality is the interface.** The question is "why does this value
   exist?", answered by following happens-before across machines, the network
   and storage.
7. **Scenarios survive code changes.** Faults pinned to semantic anchors, not
   timestamps, keep a found failure useful as a regression test after refactors.
8. **A fix is proven.** It fails on the base, passes on the fix, its anchors are
   reached, and exploration at an equal budget finds no new failure signatures.
9. **Fast enough for the inner loop.** Warm boot snapshots, forks per seed,
   parallel seeds across a fleet and incremental image preparation.

## Order of work

1. **Integrated simtest** (the current milestone): seeded races and service
   faults with a reproducible failing seed and a fixed control.
2. **Faults and topology**: the scenario protocol from
   [issue #3](https://github.com/gardnervickers/seedbox/issues/3), regions with
   per-link latency, and semantic fault points.
3. **Speed**: boot snapshots and forks, parallel seeds, a remote fleet.
4. **Explore and shrink**: fault-plan search, failure deduplication, shrinking
   and bundles.
5. **Understand**: semantic trace on virtual time, timeline and causal view,
   tracepoints.
6. **Prove and keep**: anchors, base-versus-fix verification, regression corpus
   and CI.
7. **Doubles catalog**: S3 first, each double with a conformance corpus recorded
   against the real service.

When a change touches one of these areas, check it moves toward the
requirements above rather than away from them.
