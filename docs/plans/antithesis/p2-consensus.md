# P2: agreed top ten

Claude and Codex (gpt-6-astra) each proposed P2 ideas independently after P1b,
then converged on this list. It replaces the P2 table in
[plan-consensus.md](plan-consensus.md) as the delivery order. Every item still
ships only on measured, feature-specific benefit with replay MATCH preserved,
ARM-validated; x86 is type-checked only.

1. **Discriminating evaluations and benchmark tooling.** Buggy/fixed pairs whose
   bugs a sweep rarely reaches (crash during recovery, ambiguous retry across a
   leadership change, lease expiry across partition/heal), at least one
   historical regression, and censored discovery measurements driven through
   the existing CLI/API. Gate: checkers validated against known failing
   schedules, valid histories and fixed controls, not just low sweep rates.
2. **Concurrent-history checkers and recovery obligations.** A small pure
   checker crate (register/CAS linearizability, idempotence) over SDK-recorded
   histories with unknown outcomes kept unknown, and bounded "healthy again
   within T after heal" checks in the oracle toolkit, not a temporal engine.
3. **Workload choices that change state.** SDK helpers that choose operations,
   contention, retries and overlap at execution time, so branches reach
   different application states.
4. **Timed semantic state and transition feedback.** Bounded state buckets
   with committed positions, so the planner can tell states apart and pick
   anchors before them.
5. **Event-relative fault mutations.** Search trigger occurrences, delays,
   targets and recovery sequences, keeping causal timing and generated-group
   teardown.
6. **Planner portfolio with swarm fault mixes.** Fresh roots, random branches,
   rare-state selection and sampled fault-kind subsets, every recipe
   materialized; cap budget spent re-finding one known failure.
7. **Cold reproducers and stronger shrinking.** Reconstruct branches cold where
   equivalence holds (dropping checkpoint payloads from bundles), then shrink
   prefixes, choices and fault parameters.
8. **Retained failure timeline.** Operations, actual fault effects and property
   encounters linked with stable identities, ordered evidence and explicit
   omissions.
9. **Asynchronous block I/O.** Bounded pending requests with deterministic
   latency first, with exact crash, reset, checkpoint and FLUSH behaviour;
   completion reordering only with its own contract.
10. **Seeded guest-thread perturbation.** More single-vCPU interleavings at
    deterministic guest boundaries from an independent retained stream.

Items 9 and 10 stay below 8 for delivery but may be prototyped once their
evaluations exist. Deferred: counterfactual experiments, the snapshot tree,
passive extraction, executed-edge coverage (no demonstrated value yet; the
translated-block spike does not settle it), corruption, debug exec, and the
test-template runner (folded into 3). The shrink failure-identity defect found
during review was fixed immediately.
