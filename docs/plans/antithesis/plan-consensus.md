# Seedbox plan: properties, generated faults, guided exploration, triage

**Status:** joint plan of the Opus and Codex planners. Codex signed off on this exact text in the third consensus round; only this status line has changed since.

**Basis:** read-only inspection of Seedbox `main` @ ec80ea9f and the Antithesis docs snapshot of 2026-10-16. Antithesis citations are filenames under `pages/`. Nothing here was built or tested, so every proposed mechanism is a design to validate.

## 0. Thesis

Seedbox already has deterministic multi-machine execution of real Linux guests: PV time, binary-instrumented progress, seeded network, clock and lifecycle faults, one override timeline, checkpoints and branches, a server, a CLI and a TUI.

What it lacks is the loop that turns executions into actionable bugs. In order:

1. Express properties.
2. Produce self-contained, exportable findings.
3. Inject faults automatically, with automatic finding export for campaigns.
4. Explore alternatives from interesting states.
5. Investigate a failure cheaply.

Campaign findings are exported automatically. Single-run findings remain session-scoped until explicitly exported; `run` reports that retention status.

The plan delivers that loop in vertical slices. Each slice is measured against fixed workloads, not against seed counts.

## 1. Capability gaps

| Area | Antithesis (doc) | Seedbox today (evidence) | Gap |
|---|---|---|---|
| Environment/setup | Container-based environments configured through Compose or Kubernetes (`setup__overview.md`, `setup__docker_compose.md`, `setup__kubernetes.md`) | Native `seedbox.yaml` defines OCI-backed machines and a separate oracle (`crates/seedbox-spec/src/project.rs:10`). Linux x86-64 and Apple Silicon backends; no Kubernetes orchestration | **Partial** orchestration parity; native setup exists |
| Setup diagnostics | Setup mode and CLI validation (`setup__setup_mode.md`, `ai__snouty.md`) | `validate`, `doctor`, `prepare`, and preparation events (`crates/seedbox/src/cli/mod.rs:106`, `crates/seedbox-api/src/events.rs:9`) | **Partial** |
| Structured randomness | SDK choices expose alternatives to exploration (`reference__sdk__generate_randomness.md`) | Controlled guest entropy and branch reseeding (`crates/seedbox-engine-core/src/world.rs:1276`), but no structured choice protocol | **Partial** |
| Properties | always / always-or-unreachable / sometimes / reachable / unreachable, catalogued, nonfatal (`product__writing_tests__assertions.md`, `reference__sdk__fallback__assert.md`) | Verdict comes only from the oracle exit (`crates/seedbox-api/src/status.rs:55`). The guest API is controls only (`python/seedbox/`) | **Missing** |
| Default properties | Crashes, unexpected exits, memory, setup (`reference__default_properties.md`) | `MachineExited{code}`, execution errors (`crates/seedbox-engine-protocol/src/observation.rs:49`) | **Partial** |
| Automatic faults | Continuous random faults by default (`product__writing_tests__controlling_faults.md`) | Faults come only from oracle, TUI or authored overrides | **Missing** |
| Fault kinds | Network, node, clock, thread pausing, CPU modulation, custom (`product__writing_tests__controlling_faults__fault_types.md`) | Richer network shaping, connection-scoped drop, lifecycle, throttle, per-machine skew and drift (`crates/seedbox-control/src/lib.rs:106,318`) | **Partial** overall: network, lifecycle, throttle and clock faults exist; targeted thread perturbation does not |
| Storage faults | Restart durability only; customisation via support (`product__writing_tests__controlling_faults__fault_types.md`) | Volatile/stable disk model, torn-write option (`crates/seedbox-engine-core/src/disk.rs:1`). FLUSH only, **no FUA** (`crates/seedbox-vmm/src/virtio_blk.rs:221`; `docs/internals.md:320` is stale) | **Partial** |
| Quiet / recovery periods | `ANTITHESIS_STOP_FAULTS`, `eventually` / `finally` (`product__writing_tests__controlling_faults__pause_faults.md`) | Explicit heal, reboot and resume only | **Missing** |
| Guided exploration | Branch from interesting moments with coverage and assertion guidance (`introduction__how_antithesis_works.md`, `best_practices__sometimes_assertions.md`) | Checkpoint and branch with new branch identity. `explore` is a seed range that clones the spec and changes only the seed (`crates/seedbox-server/src/catalogue/seeds.rs:206`). Live checkpoints capped at 8 (`crates/seedbox-engine-core/src/checkpoint.rs:127,159`) | **Partial** (primitives only) |
| Coverage | Per-language instrumentation (`product__writing_tests__instrumentation__coverage_instrumentation.md`) | DynamoRIO progress client on every application thread, charging progress only (`guest/progress/progress.c`) | **Missing** |
| Findings and triage | Findings history, examples, artifacts (`product__understanding_test_results__reports__findings.md`, `product__understanding_test_results__reports__properties.md`) | Outcome, logs, events and comparison exist. The catalogue and execution files are session-scoped; the binary cache and explicitly exported offline bundles survive shutdown (`crates/seedbox-artifacts/src/bundle.rs:210,337`). A findings resource and its server export/import workflow are missing | **Missing** |
| Debugging | Multiverse debugger, exec, extract, causality (`product__debugging__simple_mvd.md`, `product__debugging__causality_analysis.md`) | Checkpoint, branch, derive with edited timeline, witness divergence (`crates/seedbox-artifacts/src/comparison.rs:17-46`) | **Partial** |
| Workload composition | Test templates (`product__writing_tests__test_templates.md`, `product__writing_tests__test_templates__test_composer_reference.md`) | One arbitrary oracle program | **Partial** |
| Workflow / agents | CLI, API, CI, skills (`ai__snouty.md`, `ai__skills.md`) | Scriptable CLI with `--json` and a server API | **Partial** |

## 2. Where Seedbox can be better, and the honest limits

1. **Self-service storage-fault contracts.**
   - Seedbox can offer deterministic IOERR on selected reads, writes or flushes, and crash while a selected flush is held. Delayed completion and opt-in corruption can follow.
   - Storage receipts will identify the selected operation and sector range, whether mutation occurred, and bounded survival/loss evidence for crashes, with explicit truncation markers. Adding this explanation is part of the storage work; current disk reports primarily expose counts and digests (`crates/seedbox-engine-protocol/src/report.rs:347`).
   - Antithesis documents no request-level block-fault interface.
   - **Limit:** the disk model keeps only the latest unflushed version per sector. It does not enumerate every possible writeback history.
2. **Explicit, editable, reducible fault schedules.**
   - All external inputs live in one keyed override timeline. Checkpoints keep the consumed prefix immutable (`crates/seedbox-engine-core/src/input_program.rs:37`).
   - That makes dependency-aware reduction of external inputs mechanical.
   - **Limit:** the result is reduced, or 1-minimal under the stated transformations. It is not globally minimal.
3. **Event-relative faults with an honest timing contract.**
   - Committed-output marker triggers already exist (`crates/seedbox-engine-core/src/triggers.rs`).
   - Typed workload events, lifecycle, connection and block-request predicates extend them.
   - Only device-request rules intercept the selected request itself. Other predicates take effect at a later committed boundary.
   - A kernel timer deadline is a multiplexed Linux clockevent, not an application timeout (`kernel/seedbox.c:205`).
4. **Local, offline investigation.**
   - Runs on Apple Silicon and Linux x86-64, with retained inputs and offline comparison.
   - **Limit:** this only becomes a workflow once findings survive server shutdown and import into a fresh server (P0a). Portability never implies cross-architecture MATCH.
5. **Inspectable evidence.**
   - Fault receipts, checkpointed disk state and witness divergence provide inspectable evidence. Frame-fate events currently belong to the best-effort live observation stream; they are not a complete retained packet history (`crates/seedbox-engine-protocol/src/observation.rs:75`).
   - Passive inspection of retained device state is observational. It does not include guest page cache or tmpfs.
6. **Not advantages.**
   - Exploration scale.
   - Guidance maturity.
   - Multicore memory-model exploration: guests are single vCPU.
   - Kubernetes, hosted UI and findings history.

## 3. Roadmap

Sizes: **S** localised, **M** several components, **L** substantial vertical feature, **XL** major execution or product work. All sizes include both native backends and validation.

### Dependency order

```
P0a ──► P0b ──► P1a ──► P1b ──► P2 (each item gated by its own measurement)
 │        │       ▲
 │        └───────┤ (generator and storage IOERR feed P1 mutations)
 └────────────────┘ (properties and events feed typed triggers and guidance)
```

### P0a: trustworthy single-run findings

| Item | Behaviour | Size |
|---|---|---|
| Native property transport | Framed guest channel on every machine, separate from privileged control. `emit` / `flush` contract (§4B) | L |
| Record format and reducer | In `seedbox-engine-protocol`. Engine-core checkpoints it; `seedbox-artifacts` runs the same reducer offline | M |
| `PropertyAssessment` | Violations, unmet obligations, incomplete evidence. Reported beside the unchanged oracle `TestVerdict`. `seedbox get properties ID` | M |
| Supervisor-observed exits | Main-process exit code or signal, as default-property evidence | S |
| Durable findings | Finding export to bundle, explicit import into a fresh server, `replay` to MATCH with the same property evidence | L |
| Minimal Rust oracle helpers | Readiness, declare, assert, final flush, summary query | S |

**Exit criteria:**

- A known violation survives export, server shutdown and import into a fresh server.
- Replay produces MATCH and identical property evidence.
- Passing, violated, unmet and incomplete cases stay distinguishable in output and `--json`.
- Overflow and malformed native records are visible and never produce a passing assessment.

### P0b: useful automated fault testing

| Item | Behaviour | Size |
|---|---|---|
| Seeded fault generator | Opt-in fault profile, candidate count and window as **request options**. A pure Rust sampler with explicit seed domains materialises keyed overrides into the resolved definition. Replay never reruns it. Seed-range members each materialise their own schedule | M |
| Readiness admission | Absolute-time candidates. Generated starts before setup completes are deterministically suppressed and reported as skipped. Setup-relative activation waits for typed triggers (P1a) | S |
| Quiet / recovery semantics | Oracle `quiet(duration)` suppresses generated fault *starts*. Generated teardown (heals, resumes, reboots) still runs. An explicit operation heals the active faults of a generated group. Oracle and authored actions are unaffected unless included. Group membership is semantic ownership in the fault model, not a provenance annotation | M |
| Storage IOERR | Deterministic read/write/flush IOERR for otherwise valid requests. Preserves descriptor-validation precedence and completed-prefix behaviour. Failed writes don't mutate the disk; failed flushes don't stabilise | M |
| Rust oracle toolkit | Bounded parallel work, recovery phases, coordinated finalisation. Choice helpers here draw from controlled guest entropy and emit choice events; the engine-selected, overridable choice protocol (§4B) arrives in P1a behind the same API | M |
| Kernel fatal-termination and OOM evidence | Distinguishes actual fatal termination from signal delivery. Expected injected kills are not "unexpected crash" | M |
| First Antithesis compatibility adapters | Schema adapter plus a verified capture path for an initial SDK subset, with explicit catalogue completeness (§4B) | M |

**Exit criteria:**

- On fixed workloads, generated faults produce durable, reproducible findings.
- Quiet periods let systems recover: an active partition is healed even when its scheduled heal falls inside quiet.
- Rejected or skipped admissions and evidence failures are visible.
- IOERR rules show matched / applied / ineffective receipts.

### P1a: reproducible branching foundation

| Item | Behaviour | Size |
|---|---|---|
| Typed branch anchor and reseed | A new typed input operation (not a fault, consumes no fault ID). Carries explicit seed and branch identity and is applied at a predeclared `Anchor { at_ns }` input, the timed phase before ordinary actions due at T (§4A). The same semantics apply in cold execution and checkpoint continuation. Stale anchors are rejected after edits | L |
| Structured-choice protocol | Engine-selected choices over the nonprivileged SDK reply path, with choice overrides as typed inputs (§4B). It lands here because it is a planner-mutable input with the same cold/warm equivalence obligations as reseed | M |
| Cold/restore equivalence | Required invariant, validated on both backends: witness, properties and final state, with and without reseed, including same-time controls, triggers, lifecycle, queued traffic and terminal cutoffs | L |
| Checkpoint byte accounting | Replaces the fixed cap of 8 with byte-accounted retention, explicit release and ownership of shared checkpoint assets | M |
| Typed triggers | Predicates on SDK events and assertions, lifecycle transitions, connection open / nth frame, block requests. Effects are admitted fault actions supported by the predicate's timing contract. Anchor, reseed and choice-selection operations remain separate typed inputs, not trigger effects. Separate limits for active matchers and historical trigger IDs | L |
| Held-request crash | Suspend the machine on admission of the selected block request; commit the crash at the next eligible boundary (§4C) | M |

**Exit criteria:**

- Cold and checkpoint executions agree on both architectures across the validation matrix above.
- Held-request crashes record match time and crash-effect time.
- A post-oracle-exit pending crash reports as cut off.

### P1b: guided campaigns and triage

| Item | Behaviour | Size |
|---|---|---|
| Campaign planner (`seedbox-explore`) | Generation-synchronous: fixed logical batch size, ordinals assigned before dispatch, feedback folded in ordinal order. Guided by assertions, semantic milestones and structured choices. Bounded corpus with a reserved share for uniform exploration. Budgets are request options | XL |
| Campaign property aggregation | Assessment across children. Counts an inherited ancestral encounter once. Branch-hit frequencies are labelled "observed campaign rate" | M |
| Findings query | Machine, property, fault, interval, ancestry, "preceded by within interval". Paginated, with incomplete-history markers | M |
| External-input shrinking | Dependency-aware ddmin over external overrides, teardown pairs and valid reseed anchors. Reruns the oracle on every probe. Never touches oracle-generated controls. Reports "reduced" / "1-minimal under these transformations" | M |
| CI and agent workflow | Bounded campaign, structured assessment, exported failures, comparison against an explicitly selected baseline | M |
| Coverage measurement spike | Prototype bounded, module-relative edge collection in the DynamoRIO client. Configuration is a resolved input. Measure incremental discovery against assertion-only guidance | M |

**Exit criteria:**

- Better discovery than seed sweep plus generator on workloads whose bugs need meaningful timing or state combinations, with correct negative controls.
- Evaluation uses repeated campaign seeds under fixed work budgets, with unsuccessful searches reported as censored. The etcd `-plant` option (`examples/etcd/workload/main.go:214`) validates the checker and finding pipeline only, not exploration.
- Identical logical plans, branch recipes and semantic findings across worker counts at equal completed budgets. Wall-time deadlines are exempt from this promise.

### P2: measured extensions

Each item ships only when it shows feature-specific discovery or debugging benefit, with replay preserved.

| Item | Gate | Size |
|---|---|---|
| In-worker snapshot tree (content-addressed page store, delta restore) | Only if measured capture/restore cost limits campaigns | XL |
| Coverage-guided exploration | Only if the P1b spike shows discovery value worth its cost | L |
| Seeded preemption / thread perturbation (resolved-definition field, own RNG slot) | Measured interleaving yield | M |
| Asynchronous block requests with delayed completion | Pending requests with completion deadlines, so other threads run; checkpointed pending state | L |
| Counterfactual resampling ("causality") | Stated sampling distribution, sample counts, uncertainty; not proof of causality | M |
| Debug exec on derived branches | `exec` input in a derived simulation; perturbs only that child | M–L |
| Scoped passive extraction | From retained device state, naming the view (stable post-crash disk, latest device-visible disk). Journal interpretation where needed | M |
| Test-template runner | Guest-side Rust binary inside the oracle. Configurable directory with a Seedbox default; Antithesis-compatible layout optional | M |
| Opt-in corruption | Corrupt-on-read (stored data unchanged) and stable-sector mutation, as separate contracts with stated redundancy, detection or recovery properties | M |

### Deliberately excluded

- **Lying (dishonest) flush.** Excluded from the baseline durability contract. Testing it would require explicit assumptions about which devices can violate flush durability and what redundancy is expected to tolerate.
- **Reinforcement-learning guidance.**
- **A distributed execution fleet.**
- **A notebook frontend** before CLI/API investigations show demand.
- **Kubernetes parity.**
- **Treating verbose re-execution as equivalent to retroactive logging.** Changing guest logging can alter execution and timing; refiltering retained evidence is the observational option.
- **Changing the oracle verdict contract without a user decision.**

## 4. Architecture

### Ownership

| Owner | Responsibility |
|---|---|
| New `seedbox-sdk` | Rust guest API: transport handles, declarations, assertions, events, structured choices, lifecycle markers, flush |
| `seedbox-control` | Language-neutral privileged control and fault-action schemas, including targeting and quiet/recovery operations. Property records use the separate SDK protocol owned by `seedbox-engine-protocol`; property submission does not grant control privileges (control admission is controller-only, `crates/seedbox-engine-core/src/control.rs:554`) |
| `seedbox-engine-protocol` | Versioned property record format and **pure reducer** (fixed evidence semantics only: declaration consistency, boolean observations, counts, examples, completeness). Campaign receipts. Typed reseed input |
| `seedbox-engine-core` | Commit-time record acceptance, checkpointed reducer state, trigger matching, device fault semantics, reseed application |
| New `seedbox-explore` | Passive planner: selection, mutation, feedback aggregation, fault-profile sampling. No processes, no async runtime |
| `seedbox-server` | Campaign lifecycle, capacity, child submissions, cancellation, findings and query API |
| `seedbox-application` | Preparation and request/report adaptation. Stays independent of server and CLI |
| `seedbox-artifacts` | Offline reduction (same reducer), finding bundles, reproduction validation |
| `seedbox-api` | Public campaign, finding and assessment resources; submission, query and export request/response types |
| `seedbox-spec` | Resolved simulation definitions and the public representation of typed external inputs |
| CLI / TUI | Presentation and explicit submission only |

Default-property policy, campaign scoring and CLI exit decisions live outside the protocol reducer.

Shared guest record types have one owner in `seedbox-engine-protocol`; the SDK can depend on that schema without introducing a reverse dependency from `seedbox-control`. The world coordinator remains the sole owner of scheduling and lifecycle ordering. Native adapters implement the required stopped-machine operations; neither the explorer nor a worker adds another world scheduler.

### 4A. Exploration and the branch representation

**One timeline.** A branch recipe is: original root inputs + consumed external-input prefix + typed reseed + suffix. Oracle calls stay generated by the oracle and are never copied into the recipe.

**Anchor.** Today two capture positions exist:

- a timed checkpoint precedes every action at T (`world_windowed.rs:992`);
- an interactive checkpoint follows `advance_boundaries` (`world_windowed.rs:903`).

Recorded scheduler positions are observations, never timing inputs (`crates/seedbox-engine-protocol/src/input_program.rs:71`). The reseed input therefore names one defined coordinator phase.

An existing timed checkpoint already changes planning before T: it is registered as a global boundary (`world_windowed.rs:1076`) that caps rounds and limits early delivery (`crates/seedbox-engine-core/src/window/plan.rs:515`; the test at `plan.rs:885` shows a boundary excluding an otherwise eligible early event). Boundary insertion is therefore not transparent, and the anchor must be part of the recipe.

**V1 anchor contract.** A keyed `Anchor { at_ns }` input establishes the timed checkpoint phase before ordinary actions due at T. Its scheduling boundary exists whether or not a snapshot is captured.

Candidate anchors are declared in the execution recipe before execution. The parent that produces a checkpoint and cold re-execution of its child retain the same earlier anchors, including anchors that were not selected for capture. An anchor may not be added retrospectively to an already-executed prefix.

At T, the coordinator consumes the anchor and records its phase/cursor. It may then capture a checkpoint. Warm continuation and cold execution apply the child's reseed at that position exactly once, before ordinary controls, triggers and fault inputs due at T. Existing ordering among those ordinary actions is unchanged. Oracle-exit cutoff still takes precedence.

A checkpoint is an optional cache of this anchored execution. Capturing it must not add guest progress, extra semantic boundary transitions or snapshot-dependent witness records. Boundary insertion itself is not assumed transparent.

Warm reuse requires the unchanged consumed prefix and matching anchor semantics. Editing that prefix requires cold execution and recapture. Interactive checkpoints remain valid reproduction assets with their checkpoint closure, but are not flattened anchors in v1.

Input receipts distinguish fault admission, anchor consumption and reseed application. Anchor and reseed receipts advance the input cursor without allocating fault IDs (today `Program::validate` accounts for consumed inputs only through fault admissions and rejections, `crates/seedbox-engine-core/src/input_program.rs:106`).

**Reseed semantics.** Reseeding preserves already-sampled effects and changes only future draws, matching `reseed_checkpoint` (`world.rs:1276`). The same-seed case is explicit continuation. Guest entropy is reseeded through the existing PV entropy generation (`kernel/apply.py:190`).

**Planner.**

1. Run a few fresh seeds to diverse initial states.
2. Use committed property outcomes, milestones, recovery transitions and declared feature buckets to select captures at predeclared candidate anchors. Retain both the interesting event's position and the selected anchor. Initial v1 does not insert new anchors into an executed prefix.
3. Select from a bounded corpus favouring novelty and short prefixes, with a fixed uniform share.
4. Vary reseed identity, structured choices and generated fault suffixes.
5. Fold results in ordinal order.

Never deduplicate states by raw RAM or disk equality.

**Budgets** (branches, planner work, checkpoint bytes, retained evidence, continuation work) are campaign request options, never `seedbox.yaml` or definitions. Wall-time cancellation is separate. A continuation stopped before oracle exit has an incomplete `TestVerdict`. An intentional continuation boundary can nevertheless complete an exploration segment successfully; it does not by itself make the campaign incomplete. Campaign completion separately distinguishes exhausted logical budget, cancellation and execution/evidence failure. Liveness and reachability assessments retain their declared evaluation scope.

**Limits.** Input programs cap at 4096 entries and reject `/` in keys (`crates/seedbox-engine-protocol/src/input_program.rs:5`). Generated keys must use an allowed separator, and campaign depth must respect the cap.

### 4B. Guest SDK, transport and properties

**Model.**

- `PropertyDeclaration { id, description, kind, scope, location }`
- `PropertyEncounter { id, condition, details }`
- `ScenarioEvent { name, fields }`
- `ChoiceRequest { site, occurrence, alternatives }`
- lifecycle markers: setup, recovery, completion

**Structured choices.** A choice request receives a selected alternative through the nonprivileged SDK reply path. Engine-core selects it using a dedicated branch-aware RNG domain or an explicitly authored choice override. Request identity includes machine, incarnation, producer session, site and occurrence; the ordered alternatives are validated. Requests, selections, occurrence state and RNG continuation are retained/checkpointed. Replies have deterministic commit and delivery ordering. P1 mutations add choice overrides to the existing input program rather than replaying guest requests as external calls.

Explicit stable IDs; descriptions and locations are presentation only. Conflicting declarations are rejected deterministically. The oracle can declare expected properties so that unloaded modules don't hide them.

**Transport.**

- A `SOCK_SEQPACKET` socket per producer session, served by the supervisor (`guest/compose-init.c`).
- The supervisor relays each message as a distinct frame type on the existing management port, alongside application output, never on the control channel.
- Engine-core accepts records only from committed management bytes, at complete-frame acceptance. That defines the encounter's position; a guest-supplied timestamp is never trusted.
- Records are attributed to machine, incarnation, producer session and local sequence, and merged at coordinator commit in fixed order.
- Checkpoints retain framing state, producer sequencing, committed watermarks and reducer state.
- Semantic records, or their digest, enter deterministic evidence.
- The lossy live observation stream (`observation.rs:1`) is only a projection.
- Frame size, producer count, declaration count and queued bytes are explicitly bounded. Result-affecting limits are retained semantic inputs or fixed protocol limits. Guest backpressure follows deterministic supervisor processing; host observer or export speed cannot determine which records are accepted. Storage errors and cancellation remain explicit execution outcomes.

**Contract.**

- Successful `emit` means the guest transport accepted the record.
- Successful `flush` means the engine committed and reduced every preceding record from that producer session. A flush is not a cross-process or cross-machine barrier.
- Summary queries return watermarks. The toolkit provides coordinated finalisation when a workload needs a stronger guarantee.
- A crash may discard records that have not crossed the boundary; this is deterministic.
- Overflow, malformed native messages and known evidence loss are explicit and never yield a passing assessment.
- The termination path is extended so the final exit is ordered after the accepted SDK prefix. Today's `drain_output` drains only queued stdout/stderr and does not wait for descendants (`compose-init.c:495`).
- No per-record engine acknowledgement. `emit_and_flush` is available for critical final assertions.

**Semantics.**

- Five kinds: always, always-if-reached, sometimes, reachable, unreachable.
- A declared `always` never encountered is an **unmet obligation**, neither passed nor a safety violation.
- Safety violations, unmet exploration goals and incomplete evaluation are distinct.
- "Never ready" requires a defined completion condition; a host timeout alone is not a safety failure.
- Expensive details may be truncated under deterministic size rules, keeping the violation and a truncation marker.
- Host storage failure is an execution failure, not a property violation.

**Verdict.** `TestVerdict` stays oracle-only. `PropertyAssessment` sits beside it. Exit policy is a user decision (§6.1). In every policy, execution-error precedence (ERROR, TIMEOUT, INCOMPLETE ahead of FAIL, as in `crates/seedbox/src/cli/simulations.rs:242`) is preserved without hiding a definite violation.

**Antithesis compatibility (adapter).**

- A schema adapter for fallback-schema records (`reference__sdk__fallback__schema.md`).
- A capture path verified per SDK: the Rust and Python docs promise a **log file** (`reference__sdk__rust.md:75`, `reference__sdk__python.md:114`), so open, truncate, buffering and multiprocess behaviour must be checked for each SDK.
- Catalogue completeness handled explicitly, since Python uses a separate scan/register workflow.
- Imported counts are not presented as total evaluations, because the fallback docs recommend emitting only the first true and first false (`reference__sdk__fallback__assert.md`).
- Line interleaving beyond `PIPE_BUF` may not produce invalid JSON, so malformed-line detection is not proof of preservation.
- Environment injection is opt-in and visible in resolved inputs.
- The adapter provides assertion compatibility, not platform randomness or workload scheduling.

### 4C. Faults and targeting

**Typed rules.** `TriggerSpec`, `EventSelector`, `FaultRule` and `FaultReceipt`. Receipts distinguish armed, matched, applied, ineffective, expired, rejected and cut-off.

All external rules enter the one override program, with stable authored keys. Fault IDs are allocated only at admission, same-time document order is preserved, and the consumed checkpoint prefix is immutable.

**Two initial timing contracts, plus a later timer-targeting experiment.**

1. **After an observed event:** a future effect time respecting causal latency, as marker triggers do today.
2. **At a device request phase:** the rule is installed before execution and intercepts the request locally, before its effect.

**Later timer-targeting experiment:** selectors may refer to an incarnation and kernel clockevent generation, with explicit cancellation/rearming behavior and no retroactive pre-expiry effect. This is not application-timeout targeting. It is outside P1's committed scope until usefulness and deterministic timing semantics are demonstrated.

**Generator.**

- Profiles (kinds, weights, inter-arrival, durations, targets, storage rules, trigger templates once typed triggers exist) are request inputs.
- The sampler lives in `seedbox-explore`, uses domain-separated seeds and produces bounded output.
- Generated starts and teardowns are semantically distinct members of a generated group.

**Storage IOERR (P0b).**

- The fault outcome is chosen before the mutations `Block::notify` currently performs synchronously (`virtio_block.rs:172`).
- Uses the existing `S_IOERR` path.
- The policy is part of the machine's immutable round assignment, with a per-machine RNG stream, so the parallel scheduler stays exact.
- Selector occurrence counters, per-machine RNG state and unresolved match receipts are checkpointed state. A rule's immutable assignment does not make its execution counters stateless.

**Held-request crash (P1a).** On admission of the selected request:

- Earlier requests in the same notification complete normally, including dirty writes and IRQ state.
- The selected request gets no disk mutation and no used-ring or IRQ completion. The CPU's MMIO instruction completes but the virtio request does not.
- Subsequent processing stops, and the machine reports a new **terminal suspension** reason.

Implementation notes:

- Both adapters guard existing parking against owed I/O (KVM `crates/seedbox-vmm/src/machine.rs:826`, ARM `crates/seedbox-arm64/src/application/world_adapter.rs:588`). KVM also completes its held architectural exit and records a `P` witness marker; ARM's physical park validates stopped state (`crates/seedbox-arm64/src/machine.rs:233`). Held-request terminal suspension is a distinct operation on both backends. It must not repay simulated I/O by executing the guest.
- Inputs queued for the machine by earlier rounds stay unconsumed, using the suspended-report consumed prefix (`crates/seedbox-engine-core/src/window/plan.rs:143`).
- At the next eligible boundary the coordinator commits the crash through the ordinary lifecycle path, which already rejects effects in committed machine past (`world_windowed.rs:804`). Transit frames follow existing crash and reboot semantics (`world_windowed.rs:815`): frames that arrive while the machine is down are dropped at reboot, later ones are kept.
- If the oracle exits in that round, the existing cutoff holds (`world_windowed.rs:979`) and the pending crash is reported as cut off.
- No reusable checkpoint may contain an unresolved held request.
- Crash-transaction poisoning is unchanged.

The contract does **not** claim equivalence to an instantaneous crash at match time. It records both times.

**Later storage.** Asynchronous pending requests with completion deadlines, for latency. Opt-in corruption as two contracts. No lying flush. Product text follows the FLUSH-only device.

**Limits.** `MAX_TRIGGERS` (`crates/seedbox-control/src/lib.rs:197`) counts every trigger ever armed. Historical IDs are never recycled; active matcher storage is reclaimed separately.

### 4D. Findings, reproduction, debugging

**Types.** `Moment` (branch identity plus deterministic position and sequence, not time alone), `PropertyResult`, `Finding`, `Example`, `ReproductionRecipe`.

**What a finding retains.**

- Stable property or diagnostic identity and bounded grouping fields.
- A representative failing moment and, when observed, a passing example.
- Actual fault effects, rejections and evidence limitations.
- A self-contained resolved definition and its retained engine, kernel and guest assets.
- The checkpoint closure when the recipe is not cold-reconstructible.
- Expected witness and property evidence.
- The request-level logical stopping condition used to collect that evidence, including any continuation endpoint. It stays outside the simulation definition. A partial execution whose endpoint cannot yet be reproduced is labelled accordingly rather than advertised as a validated reproduction.
- A reproduction command.

**Lifetime and history.**

- Findings bundles live outside session scratch storage and are imported explicitly. The catalogue stays session-only. Parent IDs describe ancestry and are never needed for replay.
- Triage grouping starts with property ID plus caller-selected detail fields.
- Across campaigns, findings are labelled "new", "observed again" or "not observed". "Resolved" requires a user decision or a defined statistical policy.
- Campaign export is server-owned and incremental: each retained representative is exported once its reproduction evidence is complete, without waiting for the whole campaign to finish. Bundles live outside session scratch storage. A finding is reported as durable only after publication succeeds; failures are explicit and shutdown drains pending exports or reports their failure. `run` retains explicit export and reports whether its finding is still session-only.
- Existing export already uses a temporary file, sync and no-clobber publication (`crates/seedbox-artifacts/src/bundle.rs:245`). The campaign feature must define and validate its durability guarantee, including required metadata synchronisation; the existing helper alone is not proof of crash durability.

**Investigation.**

- Shrinking and resampling as in P1b and P2.
- Debugging grows from explicit checkpoint branching. Exec runs only on derived children, because inspection can perturb execution.
- Passive extraction reads retained device state and names its view.

**CLI/TUI.** P0 exposes property assessment, evidence completeness, findings and export state. P1 adds campaign/node lists with ancestry and discovery summaries; selecting a node opens its simulation and existing timeline. Fault forms and previews remain API-owned, and consumed-prefix edits retain the existing validation rules. Commands preserve positional resource arguments, long flags, one status line, structured `--json`, existing exit codes and explicit blocking through wait/follow operations.

## 5. Determinism risks and how each design preserves MATCH

| Feature | Risk | Preservation |
|---|---|---|
| Property records | Host timing, lossy streams | Accepted only from committed bytes at frame acceptance. Reducer checkpointed. Evidence includes records or their digest |
| SDK flush and finalisation | Exit races with descendant writers | Termination ordered after the accepted SDK prefix |
| Generator | Sampler drift across versions | Materialised overrides; replay never reruns the sampler. Each seed-range member materialises its own schedule |
| Quiet periods | Teardown suppressed | Only starts are suppressed; teardown and group heal stay admitted |
| Storage IOERR | Parallel workers | Rule in immutable round assignment, per-machine stream. Decided before mutation |
| Held-request crash | Mid-round lifecycle, queued inputs, owed I/O | Terminal suspension with consumed prefix. Crash at next boundary via ordinary lifecycle. Oracle-exit cutoff kept. No checkpoint with an unresolved hold |
| Reseed | Cold path differs from warm (same-time actions, sampled effects) | Predeclared anchor boundary shared by parent and cold recipe; checkpoint phase/cursor retained; reseed applied exactly once before ordinary due actions. Sampled effects preserved. Capture/restore equivalence validated on both backends |
| Planner | Worker completion order | Ordinals before dispatch, fixed batch, ordinal-order fold, own RNG domain |
| Coverage | Translator allocations and syscalls change timing | Resolved-input setting. Replay promised only within the configured mode |
| Preemption | Shares jitter stream | Own PV seed slot; resolved-definition field |
| Debug exec | Perturbs timeline | Derived children only; recorded as an input |
| Structured choices | Reply timing, stale occurrence identity, reused PRNG state | Dedicated branch-aware stream; validated occurrence/alternatives; deterministic reply ordering; request and selection evidence checkpointed |
| Finding export | Detached-client loss, shutdown cleanup, partial publication | Server-owned incremental export outside session storage; explicit publication receipts and failures |

**Validation.**

- Independent small models for reducers, trigger ordering and storage durability.
- Frame-fragmentation and snapshot-continuation tests.
- Same-time oracle and override ordering and rejection.
- Serial versus parallel execution with reordered worker completion.
- Portable checks plus both native suites.
- Fresh executions as well as retained-engine replay, since MATCH alone does not validate a changed engine.
- Anchor tests include nonzero early-delivery slack and events on both sides of T. Compare executions with the same anchors and different capture/restore choices; separately measure the effects of adding or removing an anchor.

## 6. Open user decisions (joint recommendations)

1. **Exit policy for `run`.**
   - **Options:**
     - (A) oracle-only exit with the assessment printed;
     - (B) exit 1 on oracle failure or any safety violation;
     - (C) A by default, plus a presence flag `--fail-on-violation`.
   - **Joint recommendation:** C. Campaigns use the assessment by default. Unmet goals fail only when required. Neither policy rewrites the stored oracle verdict.
2. **Initial Antithesis compatibility scope.** Which SDKs get verified capture paths first, whether to inject `ANTITHESIS_*` variables (opt-in, visible), and the trademark/licence check. **Recommendation:** a schema adapter plus Rust and Go as the recommended initial adapters (their capture behaviour is not yet verified), with Seedbox-named equivalents always available.
3. **Evaluation workloads.** Which systems and planted or historical bugs define "better discovery". **Recommendation:** storage engines, databases and replicated services (etcd, Redis, MinIO examples), using bugs that need timing or state combinations, with negative controls.
4. **Findings export default.**
   - **Options:** campaigns export failures automatically to a caller-selected directory, or require explicit export.
   - **Joint recommendation:** campaigns export retained findings automatically to a request-selected directory, defaulting to a campaign subdirectory under the resolved state directory's `findings/`, outside `sessions/`. The server owns export, including detached runs. Output reports publication success or failure. `run` keeps explicit export and identifies session-only findings.
5. **First users.** **Recommendation:** storage and replicated-data systems before broad orchestration parity.
