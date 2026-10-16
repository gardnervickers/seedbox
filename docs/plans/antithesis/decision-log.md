# Decision log: Opus ↔ Codex consensus

Final status: Codex signed off on `plan-consensus.md` as written (round 4 of the session, the third consensus round). It confirmed that this log fairly represents its positions, after the three wording corrections it requested were applied verbatim.

"Opus" means `plan-opus.md` and the Opus planner; "Codex" means `plan-codex.md` and Codex Astra (`gpt-6-astra`). Rounds are numbered from the first consensus exchange. Every code citation below was checked against ec80ea9f by at least one side.

## 1. Does a failing property make `run` FAIL?

- **Opus initial.** `run` exits 1 when the oracle fails or any `always`/`unreachable`/default property fails.
- **Codex initial.** Keep the oracle-only `TestVerdict` and report a separate assessment. Changing ordinary pass/fail is an explicit contract change.
- **What moved it.**
  - `TestVerdict` is a documented contract (`crates/seedbox-api/src/status.rs:55`, README), not an incidental behaviour. Opus conceded (round 1).
  - Codex added that execution-error precedence (`crates/seedbox/src/cli/simulations.rs:242`) must be preserved without hiding definite violations.
  - Codex also added that a never-encountered `always` is an unmet obligation, not a pass.
- **Resolution.** User decision. Joint recommendation is option C: oracle-only exit by default, plus a `--fail-on-violation` presence flag. Campaigns use the assessment.
- **Residual.** None.

## 2. Guest→engine property transport and Antithesis compatibility

- **Opus initial.**
  - JSONL written to a FIFO at `/run/seedbox/sdk.jsonl`, drained by `compose-init` onto the management port.
  - Parsed by the engine from committed bytes; aggregated offline.
  - `ANTITHESIS_SDK_LOCAL_OUTPUT` injected for day-one multi-language SDKs.
- **Codex initial.**
  - A dedicated framed guest channel, separate from control, available on every machine.
  - Per-record deterministic acknowledgement and an engine reducer.
  - Never derive correctness from the lossy observation stream.
- **What moved it.**
  - **Shared from the start:** committed-byte acceptance, never the live stream.
  - **Opus conceded (round 1):**
    - FIFO JSONL is unsound for concurrent writers beyond `PIPE_BUF`, so the native transport became a framed `SOCK_SEQPACKET` socket relayed as its own frame type;
    - reduction must also live in the engine (checkpointed) so oracles can query it. The pure reducer moved to `seedbox-engine-protocol`, shared with artifacts.
  - **Codex conceded (round 1):** per-record ack adds a round trip on the hot path. It accepted an `emit`/`flush` contract instead, with flush scoped to a producer session, watermarks, and coordinated finalisation.
  - **Codex corrected Opus:**
    - `drain_output` does not wait for descendants or flush SDK records (`guest/compose-init.c:495`);
    - the reducer's scope is fixed evidence semantics only.
  - **Compatibility:** Codex accepted an adapter but narrowed the promise.
    - The SDK docs promise a log file, not a FIFO.
    - Interleaving can produce valid but wrong JSON.
    - The Python catalogue needs separate registration.
    - Fallback SDKs emit only the first true/false, so imported counts are not evaluation totals.
- **Resolution.** Native framed transport with emit/flush. A shared pure reducer. A bounded compatibility adapter with per-SDK verified capture. Environment injection is opt-in and visible.
- **Residual.** Scope and trademark are a user decision (§6.2).

## 3. Seeded fault generator: P0 or campaigns only

- **Opus initial.** P0: a pure seeded sampler that materialises keyed overrides; `run --faults`.
- **Codex initial.** Folded into guided campaigns (P1).
- **What moved it.**
  - Opus's case:
    - continuous random faults are Antithesis's first-line value (`product__writing_tests__controlling_faults.md`, `product__writing_tests.md`);
    - campaigns need the same component;
    - it needs no checkpoints.
  - Codex conceded (round 1) and added required integration:
    - each seed-range member materialises its own schedule (`seeds.rs:206` clones the spec and changes only the seed);
    - readiness suppression with skipped candidates reported;
    - quiet suppresses starts but never teardown, plus group heal;
    - group membership is semantic ownership.
- **Resolution.** P0b, with those requirements.
- **Residual.** None.

## 4. Branch representation

- **Opus initial.** A `reseed` override plus path flattening, so every node replays cold and checkpoints are caches.
- **Codex initial.** Journaled branch recipes and checkpoint closures; replay never reruns guidance.
- **What moved it.**
  - Opus argued (round 1) that a recipe should be an override-timeline entry, and that cold equivalence is a testable invariant. Opus found no existing test of it.
  - Codex accepted both, but showed (round 1) that a timestamp alone is insufficient:
    - timed checkpoints precede the actions at T (`world_windowed.rs:992`), while interactive checkpoints follow `advance_boundaries` (`:903`);
    - recorded positions are observations (`crates/seedbox-engine-protocol/src/input_program.rs:71`);
    - a random-sampling action at T makes cold and warm executions diverge if reseed lands on the wrong side of it.
  - Result: a typed reseed input with a precise phase anchor, preserved sampled effects (as `reseed_checkpoint`, `world.rs:1276`), stale-anchor rejection, and L-sized validation on both backends.
  - Opus proposed (round 2) the timed phase as the v1 anchor, with the capture point as an explicit recipe input to avoid relying on boundary-insertion transparency.
  - Codex confirmed (round 2) that a timed checkpoint already changes planning before T: it is registered as a global boundary (`world_windowed.rs:1076`) that caps rounds and early delivery (`window/plan.rs:515`, test at `:885`). So the anchor must be in the recipe.
  - Codex refined Opus's proposal (round 2) into the predeclared-anchor contract:
    - a keyed `Anchor { at_ns }` input whose boundary exists whether or not a capture happens;
    - candidate anchors predeclared, with unselected anchors retained;
    - no retrospective anchors;
    - reseed applied exactly once at the consumed anchor, before ordinary due actions;
    - capture adds no progress, boundaries or witness records;
    - receipts for non-fault inputs advance the cursor without fault IDs (`input_program.rs:106`).
- **Resolution.** Typed reseed at predeclared anchors in the one override timeline. Cold/warm equivalence is a validated P1a invariant. Interactive checkpoints remain reproduction assets with closure. Codex also supplied the structured-choice contract (round 2); Opus proposed its roadmap placement in P1a (round 3).
- **Residual.** None.

## 5. Block-event crash timing

- **Opus initial.** Park at the matching exit and commit the crash at the recorded mid-round time.
- **Codex initial.** Hold the flush via asynchronous pending requests, then crash at the next committed boundary. An exact mid-round global cut is deferred.
- **What moved it.**
  - Opus conceded the mid-round time claim (round 1). Opus proposed instead: suspend on admission without completing the request, then crash at the next boundary with no async device.
  - Codex accepted the mechanism (round 1) and closed three holes:
    - inputs already queued by earlier rounds stay unconsumed (`crates/seedbox-engine-core/src/window/plan.rs:143`), and transit frames follow existing crash/reboot semantics (`world_windowed.rs:815`), so Opus's "frames dropped" claim was wrong;
    - both adapters guard parking against owed I/O (`crates/seedbox-vmm/src/machine.rs:826`, ARM `world_adapter.rs:588`). KVM additionally records a `P` witness marker; ARM's physical park validates stopped state. Codex clarified this backend distinction in round 2. So a new terminal suspension is needed;
    - an oracle exit in that round cuts off the crash (`world_windowed.rs:979`).
  - Also agreed: no checkpoint containing an unresolved hold, and both times recorded.
- **Resolution.** Held-request crash as a P1a item.
- **Residual.** None.

## 6. Storage scope and mechanism

- **Opus initial.** P0 covers IOERR, a lying flush, corrupt reads and latency via owed I/O.
- **Codex initial.** IOERR plus true asynchronous pending requests; no lying flush; no arbitrary corruption.
- **What moved it.**
  - Opus conceded (round 1):
    - lying flush should be excluded from the baseline; the initial blanket impossibility argument was later withdrawn by both planners (see §11);
    - owed-I/O charging stalls the single vCPU, which is the wrong semantics for latency;
    - durable-data corruption is a separate contract.
  - Codex accepted opt-in corruption as P2, with separate corrupt-on-read and stable-mutation contracts and stated redundancy properties. Opus supplied the FAST'17 example; Codex did not independently verify that paper.
  - Both: the fault outcome is decided before mutation; FLUSH-only product text, not the stale FUA wording.
- **Resolution.** P0b sync IOERR. P1a held-request crash. P2 async latency and opt-in corruption.
- **Residual.** None.

## 7. Coverage

- **Opus initial.** P1 via the DynamoRIO client; always on.
- **Codex initial.** P2, after measured yield; optional.
- **What moved it.**
  - Opus conceded "always on": translator allocations and syscalls are charged, so coverage is a resolved-input setting.
  - Opus also conceded guidance should start from assertions.
  - Codex accepted a P1b measurement spike: discovery across workloads and seeds, host cost, MATCH with coverage on, worker independence, and JIT identity noise.
- **Resolution.** P1b spike; P2 feature only if justified. Replay is promised within the configured mode only.
- **Residual.** None.

## 8. Sequencing

- **Opus initial.** P0 (properties, defaults, generator, quiet, storage faults, findings), P1 (reseed, explorer, coverage, triggers, preemption, snapshot tree, template runner), P2 (shrink, causality, debug, query, timeout-aware, skills).
- **Codex initial.** P0 (properties, durable reproductions, oracle toolkit), P1 (campaigns, storage, typed targeting, findings and branch investigation, CI), P2 (coverage and concurrency, advanced debugging).
- **What moved it.**
  - Opus's round-1 merge brought durable findings and the toolkit into P0, and deferred the snapshot tree to P2 behind measurement.
  - Codex split the work into P0a/P0b/P1a/P1b with completion evidence (round 1), and added that:
    - kernel detectors must not block the first slice;
    - fatal termination is not the same as signal delivery;
    - `-plant` validates the checker only;
    - evaluation needs censored, fixed-budget comparisons;
    - worker independence holds at equal completed budgets, not wall-time deadlines.
- **Resolution.** Sequencing as in `plan-consensus.md` §3.
- **Residual.** None.

## 9. Additional points raised in round 1 (Codex), all accepted by Opus

- **Shrinking** reports "reduced / 1-minimal under these transformations". It is dependency-aware and never touches oracle-generated controls.
- **`MAX_TRIGGERS`** counts all triggers ever armed (`crates/seedbox-control/src/lib.rs:197`). Active matcher storage is reclaimed separately and IDs are never recycled.
- **Input keys** can't contain `/` and the program caps at 4096 entries (`crates/seedbox-engine-protocol/src/input_program.rs:5`). Opus's `x/<node>/...` keys must change.
- **Retroactive logging.** Opus withdrew "verbose re-execution ≡ retroactive logging".
- **Passive extraction** names its view; no page cache or tmpfs.
- **Campaign rates.** Branch frequency is labelled "observed campaign rate", and inherited encounters are counted once.

## 10. Findings export default (raised as Codex open question; settled round 2)

- **Opus proposal.** Campaigns take an export directory as a request option with a documented default.
- **Codex addition.** Server-owned, incremental export, so detached runs work. The default is a campaign subdirectory under the state directory's `findings/`, outside `sessions/`. A finding is durable only after publication succeeds, and shutdown drains or reports pending exports. `run` keeps explicit export and labels session-only findings.
- **Resolution.** User decision, with this joint recommendation. It does not restore an old catalogue.

## 11. Round-2 corrections from Codex to the draft text, all accepted by Opus

- **Thesis.** Automatic export applies to campaigns only.
- **Capability map.**
  - The fault-kinds row is Partial.
  - Offline bundles already survive shutdown (`bundle.rs:210,337`).
  - Environment/setup, setup-diagnostics and structured-randomness rows added.
  - Full doc filenames used.
- **Evidence claims.**
  - Storage sector explanations are future work; `DiskReport` exposes counts and digests (`report.rs:347`).
  - Frame fates are live-only, not evidence (`observation.rs:75`).
- **Trigger effects** are admitted fault actions only. Anchor, reseed and choice are separate typed inputs.
- **Dishonest flush.** The claim that no software can tolerate it was false: replication can tolerate bounded loss. It is excluded from the baseline contract rather than declared impossible to survive.
- **Ownership.**
  - `seedbox-control` owns privileged control and fault schemas only.
  - `seedbox-api` and `seedbox-spec` rows added.
  - The coordinator remains the sole scheduler.
- **Campaign completion** is distinct from an incomplete `TestVerdict` on intentionally bounded continuations.
- **Transport bounds.** Explicit bounds; host speed never decides acceptance.
- **Timing contracts.** Two initial contracts. Timer targeting is a later experiment outside P1 scope.
- **Parking.**
  - Both backends guard against owed I/O.
  - Only KVM records the `P` witness marker; ARM's park validates stopped state (`arm64/machine.rs:233`).
- **Checkpointed state.** Selector counters, RNG state and unresolved receipts.
- **Findings.**
  - The passing example is included only when observed.
  - The request-level stopping condition is retained outside the definition.
  - Export is server-owned and incremental.
  - CLI/TUI workflow section added.
- **Determinism table.** Anchor wording updated; choice and export rows added; anchor validation tests added.
