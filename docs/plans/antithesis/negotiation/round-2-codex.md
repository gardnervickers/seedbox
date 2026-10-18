**A: AMEND. B: AGREE.** I accept the sequencing. The draft needs the corrections below before sign-off; most clarify contracts already agreed.

**A. Concrete branch anchor**

Yes: an existing timed checkpoint changes planning before T. The coordinator registers it as a global boundary at [world_windowed.rs:1076](../../../../crates/seedbox-engine-core/src/world_windowed.rs:1076). The scheduler uses that boundary both to cap rounds and to limit early delivery ([window/plan.rs:515](../../../../crates/seedbox-engine-core/src/window/plan.rs:515)).

The existing test at [window/plan.rs:885](../../../../crates/seedbox-engine-core/src/window/plan.rs:885) gives a concrete example: with a horizon ending at 80 and early delivery of 20, a boundary at 90 excludes an event due at 95 from the assignment. Without that boundary, the event is eligible. I inspected this test; I did not execute it.

Your explicit-anchor approach addresses this, with three requirements:

1. The anchor must actually be installed before the first affected round. Adding it retrospectively to the parent’s saved definition is insufficient.
2. Define the cursor position: consume the anchor, optionally capture, then apply the child’s reseed before ordinary actions due at T. Warm continuation and cold execution must cross that position exactly once.
3. The anchor establishes scheduling semantics independently of whether a snapshot is physically captured. Capture/restore itself must remain transparent within that anchored execution.

For v1, **predeclare a bounded set of candidate anchors in each execution recipe** and select which ones to capture based on committed guidance. Retain earlier anchors even if they were not selected. This avoids adding a new requirement to prove that dynamically registering a future boundary cannot affect the prefix.

Thus, anchor plus reseed at the defined phase is a sound design to validate. It resolves my random-sampling-at-T example: both paths reseed before the same due actions, while preserving samples taken earlier. It is not an equivalence guarantee the current code already provides.

**B. Findings export default**

Agreed: campaigns export automatically to a request-selected directory with a documented default; `run` retains explicit export.

The server must own export so detached execution works. A suitable default is a campaign subdirectory under the resolved state directory’s `findings/`, outside `sessions/`. Successful publication must precede claiming that a finding is durable. Export failure must be explicit, and graceful shutdown must finish pending exports or report their failure.

This does not require restoring an old catalogue. Explicit imports populate a fresh session from independently retained bundles.

The following are the exact draft corrections I request.

---

**Status and Basis: no corrections.**

**§0 — align the thesis with P0a and explicit single-run export**

Replace:

> 2. Inject faults automatically.  
> 3. Keep every failure as a durable, reproducible finding.

With:

> 2. Produce self-contained, exportable findings.  
> 3. Inject faults automatically, with automatic finding export for campaigns.

After the numbered list, add:

> Campaign findings are exported automatically. Single-run findings remain session-scoped until explicitly exported; `run` reports that retention status.

Otherwise the thesis promises automatic durability for every failure while §6 explicitly leaves `run` export manual.

**§1 — capability-map corrections**

Replace the Fault kinds row’s status:

> **Have** (no thread pausing)

With:

> **Partial** overall: network, lifecycle, throttle and clock faults exist; targeted thread perturbation does not.

Replace:

> Outcome, logs, events, comparison. Session-only catalogue; shutdown removes evidence

With:

> Outcome, logs, events and comparison exist. The catalogue and execution files are session-scoped; the binary cache and explicitly exported offline bundles survive shutdown. A findings resource and its server export/import workflow are missing.

Existing offline bundle export/import is real functionality ([bundle.rs:210](../../../../crates/seedbox-artifacts/src/bundle.rs:210), [bundle.rs:337](../../../../crates/seedbox-artifacts/src/bundle.rs:337)). The missing part is integration with session simulations and findings.

The original brief also requested environment/setup coverage. Add these rows:

| Area | Antithesis (doc) | Seedbox today (evidence) | Gap |
|---|---|---|---|
| Environment/setup | Container-based environments configured through Compose or Kubernetes (`setup__overview.md`, `setup__docker_compose.md`, `setup__kubernetes.md`) | Native `seedbox.yaml` defines OCI-backed machines and a separate oracle (`crates/seedbox-spec/src/project.rs:10`). Linux x86-64 and Apple Silicon backends; no Kubernetes orchestration | **Partial** orchestration parity; native setup exists |
| Setup diagnostics | Setup mode and CLI validation (`setup__setup_mode.md`, `ai__snouty.md`) | `validate`, `doctor`, `prepare`, and preparation events (`crates/seedbox/src/cli/mod.rs:106`, `crates/seedbox-api/src/events.rs:9`) | **Partial** |
| Structured randomness | SDK choices expose alternatives to exploration (`reference__sdk__generate_randomness.md`) | Controlled guest entropy and branch reseeding (`crates/seedbox-engine-core/src/world.rs:1276`), but no structured choice protocol | **Partial** |

Finally, replace abbreviated and wildcard citations with actual filenames:

| Draft citation | Replacement |
|---|---|
| `..._fault_types.md` | `product__writing_tests__controlling_faults__fault_types.md` |
| `..._pause_faults.md` | `product__writing_tests__controlling_faults__pause_faults.md` |
| `..._coverage_instrumentation.md` | `product__writing_tests__instrumentation__coverage_instrumentation.md` |
| `..._reports__findings.md` | `product__understanding_test_results__reports__findings.md` |
| `..._reports__properties.md` | `product__understanding_test_results__reports__properties.md` |
| `product__debugging__*.md` | `product__debugging__simple_mvd.md`, `product__debugging__causality_analysis.md` |
| `product__writing_tests__test_templates*.md` | `product__writing_tests__test_templates.md`, `product__writing_tests__test_templates__test_composer_reference.md` |

**§2 — distinguish proposed evidence from existing observations**

Replace:

> Each fault comes with a sector-level explanation of what survived.

With:

> Storage receipts will identify the selected operation and sector range, whether mutation occurred, and bounded survival/loss evidence for crashes, with explicit truncation markers. Adding this explanation is part of the storage work; current disk reports primarily expose counts and digests.

The current [DiskReport](../../../../crates/seedbox-engine-protocol/src/report.rs:347) is not a complete sector-history report.

Replace:

> Fault receipts, frame fates, disk layers and witness divergence are available.

With:

> Fault receipts, checkpointed disk state and witness divergence provide inspectable evidence. Frame-fate events currently belong to the best-effort live observation stream; they are not a complete retained packet history.

The distinction is explicit in [observation.rs:75](../../../../crates/seedbox-engine-protocol/src/observation.rs:75): frame batches are “never part of evidence.”

No other corrections to §2.

**§3 — retain the sequence; correct two scope statements**

No corrections to the P0a/P0b ordering or their exit criteria. No changes to the P1b evaluation criteria or the P2 feature order.

In the Typed triggers row, replace:

> Effects are any action.

With:

> Effects are admitted fault actions supported by the predicate’s timing contract. Anchor, reseed and choice-selection operations remain separate typed inputs, not trigger effects.

“All actions” becomes ambiguous once the input language includes non-fault operations.

Replace:

> **Lying (dishonest) flush.** No software can be correct against it.

With:

> **Lying (dishonest) flush.** Excluded from the baseline durability contract. Testing it would require explicit assumptions about which devices can violate flush durability and what redundancy is expected to tolerate.

The universal impossibility claim is false: replication can tolerate a bounded subset of devices losing supposedly durable data. Our agreement was to exclude this fault from the baseline product, not to claim that every possible application fails under it.

Apply the anchor wording below to the P1a row by reference. Otherwise, no roadmap corrections.

**§4 Ownership — restore the public-type owners and remove schema ambiguity**

Replace the `seedbox-control` responsibility:

> Language-neutral guest messages. Typed targeting operations. Event submission kept separate from privileged control …

With:

> Language-neutral privileged control and fault-action schemas, including targeting and quiet/recovery operations. Property records use the separate SDK protocol owned by `seedbox-engine-protocol`; property submission does not grant control privileges.

Add:

| Owner | Responsibility |
|---|---|
| `seedbox-api` | Public campaign, finding and assessment resources; submission, query and export request/response types |
| `seedbox-spec` | Resolved simulation definitions and the public representation of typed external inputs |

Add after the table:

> Shared guest record types have one owner in `seedbox-engine-protocol`; the SDK can depend on that schema without introducing a reverse dependency from `seedbox-control`. The world coordinator remains the sole owner of scheduling and lifecycle ordering. Native adapters implement the required stopped-machine operations; neither the explorer nor a worker adds another world scheduler.

This preserves the existing public-type and coordinator boundaries in AGENTS.md.

**§4A — replace the proposed-anchor paragraphs**

Replace the paragraphs beginning:

> Proposed v1: campaign branch anchors use the **timed phase at `at_ns`** …

and ending:

> … but they are not flattened anchors.

With:

> **V1 anchor contract.** A keyed `Anchor { at_ns }` input establishes the timed checkpoint phase before ordinary actions due at T. Its scheduling boundary exists whether or not a snapshot is captured.
>
> Candidate anchors are declared in the execution recipe before execution. The parent that produces a checkpoint and cold re-execution of its child retain the same earlier anchors, including anchors that were not selected for capture. An anchor may not be added retrospectively to an already-executed prefix.
>
> At T, the coordinator consumes the anchor and records its phase/cursor. It may then capture a checkpoint. Warm continuation and cold execution apply the child’s reseed at that position exactly once, before ordinary controls, triggers and fault inputs due at T. Existing ordering among those ordinary actions is unchanged. Oracle-exit cutoff still takes precedence.
>
> A checkpoint is an optional cache of this anchored execution. Capturing it must not add guest progress, extra semantic boundary transitions or snapshot-dependent witness records. Boundary insertion itself is not assumed transparent.
>
> Warm reuse requires the unchanged consumed prefix and matching anchor semantics. Editing that prefix requires cold execution and recapture. Interactive checkpoints remain valid reproduction assets with their checkpoint closure, but are not flattened anchors in v1.

This also requires extending input receipts and cursor validation to recognize successful **non-fault** operations. Today [Program::validate](../../../../crates/seedbox-engine-core/src/input_program.rs:106) accounts for consumed inputs through fault admissions and rejections. Add:

> Input receipts distinguish fault admission, anchor consumption and reseed application. Anchor and reseed receipts advance the input cursor without allocating fault IDs.

Replace planner step 2:

> Request timed checkpoints after new property outcomes, milestones, recovery transitions or new declared feature buckets. An interesting event may precede the anchor; retain both positions.

With:

> Use committed property outcomes, milestones, recovery transitions and declared feature buckets to select captures at predeclared candidate anchors. Retain both the interesting event’s position and the selected anchor. Initial v1 does not insert new anchors into an executed prefix.

Replace:

> A continuation stopped before an oracle verdict is incomplete.

With:

> A continuation stopped before oracle exit has an incomplete `TestVerdict`. An intentional continuation boundary can nevertheless complete an exploration segment successfully; it does not by itself make the campaign incomplete. Campaign completion separately distinguishes exhausted logical budget, cancellation and execution/evidence failure. Liveness and reachability assessments retain their declared evaluation scope.

Without this distinction, a campaign composed of deliberately bounded continuations would always be INCOMPLETE.

**§4B — add the missing choice and resource contracts**

The assertion semantics, emit/flush contract, compatibility scope and exit-policy text are accepted.

After `ChoiceRequest`, add:

> **Structured choices.** A choice request receives a selected alternative through the nonprivileged SDK reply path. Engine-core selects it using a dedicated branch-aware RNG domain or an explicitly authored choice override. Request identity includes machine, incarnation, producer session, site and occurrence; the ordered alternatives are validated. Requests, selections, occurrence state and RNG continuation are retained/checkpointed. Replies have deterministic commit and delivery ordering. P1 mutations add choice overrides to the existing input program rather than replaying guest requests as external calls.

This closes an architectural gap: the draft lists `ChoiceRequest` and says the planner varies choices, but never defines how a selected value reaches the guest. Choice replies do not contradict fire-and-forget assertion emission.

After the transport bullets, add:

> Frame size, producer count, declaration count and queued bytes are explicitly bounded. Result-affecting limits are retained semantic inputs or fixed protocol limits. Guest backpressure follows deterministic supervisor processing; host observer or export speed cannot determine which records are accepted. Storage errors and cancellation remain explicit execution outcomes.

These bounds were agreed earlier but are not stated concretely enough in the draft’s transport section.

**§4C — separate initial targeting from timer research; correct the park claim**

Replace:

> **Three timing contracts.**

With:

> **Two initial timing contracts, plus a later timer-targeting experiment.**

Keep the observed-event and device-request contracts as the initial two. Replace the timer item with:

> **Later timer-targeting experiment:** selectors may refer to an incarnation and kernel clockevent generation, with explicit cancellation/rearming behavior and no retroactive pre-expiry effect. This is not application-timeout targeting. It is outside P1’s committed scope until usefulness and deterministic timing semantics are demonstrated.

Timer targeting otherwise appears as a committed architectural feature without a roadmap position.

Replace:

> Existing `park` rejects owed I/O and records a witness marker (KVM …, ARM …), so this is a distinct operation.

With:

> Both adapters guard existing parking against owed I/O. KVM also completes its held architectural exit and records a `P` witness marker; ARM’s physical park validates stopped state. Held-request terminal suspension is a distinct operation on both backends.

ARM’s [Machine::park](../../../../crates/seedbox-arm64/src/machine.rs:233) does not record the KVM-style marker. This was imprecise in my earlier combined description; the backend distinction should be explicit.

After the IOERR policy bullet, add:

> Selector occurrence counters, per-machine RNG state and unresolved match receipts are checkpointed state. A rule’s immutable assignment does not make its execution counters stateless.

No other corrections to the held-request mechanism or crash/reboot ordering.

**§4D — optional examples, reproduction endpoints, durable publication and UI**

Replace:

> A representative failing moment and a passing example.

With:

> A representative failing moment and, when observed, a passing example.

Some findings have no passing example, including an always-false property or an `unreachable` violation.

After:

> Expected witness and property evidence.

Add:

> The request-level logical stopping condition used to collect that evidence, including any continuation endpoint. It stays outside the simulation definition. A partial execution whose endpoint cannot yet be reproduced is labelled accordingly rather than advertised as a validated reproduction.

This matters because a bounded branch can fail before the oracle exits. Replaying its definition to a different endpoint will not reproduce the same whole-run witness.

After the Lifetime and history bullets, add:

> Campaign export is server-owned and incremental: each retained representative is exported once its reproduction evidence is complete, without waiting for the whole campaign to finish. Bundles live outside session scratch storage. A finding is reported as durable only after publication succeeds; failures are explicit and shutdown drains pending exports or reports their failure. `run` retains explicit export and reports whether its finding is still session-only.

Existing export already uses a temporary file, sync and no-clobber publication ([bundle.rs:245](../../../../crates/seedbox-artifacts/src/bundle.rs:245)). The campaign feature must define and validate its durability guarantee, including required metadata synchronization; the existing helper alone is not proof of crash durability.

Add after Investigation:

> **CLI/TUI.** P0 exposes property assessment, evidence completeness, findings and export state. P1 adds campaign/node lists with ancestry and discovery summaries; selecting a node opens its simulation and existing timeline. Fault forms and previews remain API-owned, and consumed-prefix edits retain the existing validation rules. Commands preserve positional resource arguments, long flags, one status line, structured `--json`, existing exit codes and explicit blocking through wait/follow operations.

The draft’s “presentation and explicit submission only” ownership row is correct but does not describe the planned user workflow.

**§5 — align the determinism table with the corrected contracts**

Replace the Reseed preservation cell:

> Typed input at a defined phase. Capture point is a recipe input. Sampled effects preserved. Equivalence validated on both backends

With:

> Predeclared anchor boundary shared by parent and cold recipe; checkpoint phase/cursor retained; reseed applied exactly once before ordinary due actions. Sampled effects preserved. Capture/restore equivalence validated on both backends.

Add two rows:

| Feature | Risk | Preservation |
|---|---|---|
| Structured choices | Reply timing, stale occurrence identity, reused PRNG state | Dedicated branch-aware stream; validated occurrence/alternatives; deterministic reply ordering; request and selection evidence checkpointed |
| Finding export | Detached-client loss, shutdown cleanup, partial publication | Server-owned incremental export outside session storage; explicit publication receipts and failures |

Add to Validation:

> Anchor tests include nonzero early-delivery slack and events on both sides of T. Compare executions with the same anchors and different capture/restore choices; separately measure the effects of adding or removing an anchor.

Otherwise, no corrections to §5.

**§6 — make the export recommendation concrete**

Replace the findings-export joint recommendation:

> Campaigns take an export directory as a request option, with a documented default. Silently losing findings at shutdown cannot be the default.

With:

> Campaigns export retained findings automatically to a request-selected directory, defaulting to a campaign subdirectory under the resolved state directory’s `findings/`, outside `sessions/`. The server owns export, including detached runs. Output reports publication success or failure. `run` keeps explicit export and identifies session-only findings.

No corrections to the other user decisions. Rust and Go remain reasonable **recommended initial adapters**, not a claim that their capture behavior has already been verified.

There are no further disagreements with the phase ordering, exit-policy recommendation, compatibility limitations or evaluation approach. These corrections do not add a new phase or require implementing every P2 item.

**SIGN-OFF CONDITIONAL ON: the quoted replacements and additions above, particularly the predeclared-anchor/cursor contract, server-owned export contract, structured-choice semantics, continuation-versus-campaign completion distinction, and correction of unsupported evidence and dishonest-flush claims.**