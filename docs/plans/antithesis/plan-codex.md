# Seedbox product and architecture plan

Seedbox should prioritize a complete loop: **express a property, explore meaningful alternatives, retain a failure, reproduce it, and investigate nearby executions**. Its execution machinery already supports much of that loop. The largest gaps are guidance, structured correctness evidence, durable findings, and debugging ergonomics.

This plan is based on read-only inspection of commit `ec80ea9f` and the supplied Antithesis documentation snapshot dated 2026-10-16. No builds or tests were run. Antithesis citations are filenames under `pages/`. “Missing” below means no implementation was found in the relevant inspected interfaces; it is an inference, not proof of repository-wide absence. Proposed designs and validation work are untested.

## 1. Antithesis capability map

| Group / capability | What Antithesis lets users do | Antithesis documentation | Seedbox status and evidence |
|---|---|---|---|
| Environment: packaged systems | Run applications, dependencies, workloads, and checkers together in a hermetic environment. | `setup__overview.md`; `reference__dependencies.md` | **Have:** OCI-backed machines and a separate oracle; image preparation uses the container engine. [project.rs:10](../../../crates/seedbox-spec/src/project.rs:10), [container_images.rs:116](../../../crates/seedbox-application/src/container_images.rs:116). |
| Environment: orchestration | Import Docker Compose or Kubernetes deployments. | `setup__docker_compose.md`; `setup__kubernetes.md` | **Partial:** native `seedbox.yaml` and legacy Compose support; no native Kubernetes orchestration found. The native schema deliberately models machines and an oracle. [project.rs:10](../../../crates/seedbox-spec/src/project.rs:10). |
| Environment: setup iteration | Validate configuration, stream setup logs, edit configuration, and retry setup. | `setup__setup_mode.md`; `ai__snouty.md` | **Partial:** configuration validation, doctor, preparation milestones, and templates exist. Validation does not exercise workload readiness; preparation events contain coarse milestones rather than build output. [mod.rs:943](../../../crates/seedbox/src/cli/mod.rs:943), [events.rs:9](../../../crates/seedbox-api/src/events.rs:9). |
| Workloads: existing tests | Run an existing integration test as a workload. | `product__writing_tests.md`; `product__writing_tests__test_templates__test_composer_reference.md` | **Have:** the oracle is an arbitrary guest program; generated etcd and Redis projects supply concrete workloads. [templates.rs:13](../../../crates/seedbox-application/src/templates.rs:13). |
| Workloads: composition | Vary granular commands, concurrency, setup, invariant checks, and terminal recovery checks. | `product__writing_tests__test_templates.md`; `product__writing_tests__test_templates__test_composer_reference.md` | **Partial:** users can implement these inside the oracle, but no reusable composer or command-selection interface was found. Current templates package monolithic programs. [templates.rs:13](../../../crates/seedbox-application/src/templates.rs:13). |
| Properties: assertions | Declare nonfatal always, sometimes, reachable, unreachable, and optional-path assertions; catalog assertions that never execute. | `product__writing_tests__assertions.md`; `reference__sdk__fallback__schema.md` | **Missing:** the public result model derives pass/fail exclusively from oracle exit. The guest library exposes controls, not assertion declarations or encounters. [status.rs:55](../../../crates/seedbox-api/src/status.rs:55), [python API:1](../../../python/seedbox/__init__.py:1). |
| Properties: defaults | Detect unexpected crashes, memory pressure, setup failures, and unsuccessful test commands. | `reference__default_properties.md` | **Partial:** machine exits and execution errors are observed, but they are not a default property catalog. [events.rs:39](../../../crates/seedbox-api/src/events.rs:39), [status.rs:32](../../../crates/seedbox-api/src/status.rs:32). |
| Exploration: guided branching | Revisit interesting states and vary faults, workloads, randomness, and scheduling using coverage/assertion feedback. | `introduction__how_antithesis_works.md`; `best_practices__sometimes_assertions.md` | **Partial:** checkpoints and branches exist; `explore` submits a seed range and explicitly rejects checkpoint inputs. No feedback-driven state selection found. [seeds.rs:54](../../../crates/seedbox-server/src/catalogue/seeds.rs:54), [execution.rs:235](../../../crates/seedbox-server/src/execution.rs:235). |
| Exploration: structured randomness | Ask for choices whose alternatives are visible to the search engine. | `reference__sdk__generate_randomness.md` | **Partial:** isolated deterministic RNG streams and branch reseeding exist, but no guest structured-choice API was found. [rng.rs:12](../../../crates/seedbox-engine-core/src/rng.rs:12), [world.rs:1276](../../../crates/seedbox-engine-core/src/world.rs:1276). |
| Exploration: coverage | Collect execution coverage for guidance and source-level investigation. | `product__writing_tests__instrumentation__coverage_instrumentation.md` | **Missing:** existing binary instrumentation charges application progress; it does not expose coverage feedback. [progress.c:1](../../../guest/progress/progress.c:1). |
| Faults: network, nodes, clocks | Inject asymmetric network disruptions, node termination/pause/throttling, clock changes, and custom faults. | `product__writing_tests__controlling_faults__fault_types.md` | **Have, with different semantics:** network shaping, connection drops, process/machine lifecycle, throttling, and per-machine clock skew/drift. Automatic campaign scheduling remains missing. [control/lib.rs:106](../../../crates/seedbox-control/src/lib.rs:106). |
| Faults: thread scheduling | Pause individual instrumented threads to expose interleavings. | `product__writing_tests__controlling_faults__fault_types.md` | **Partial:** instruction-based progress and CPU jitter perturb execution; process-group pause is not thread pausing. [project.rs:144](../../../crates/seedbox-spec/src/project.rs:144), [control/lib.rs:329](../../../crates/seedbox-control/src/lib.rs:329). |
| Faults: recovery periods | Temporarily stop faults and restore disrupted components for recovery checks. | `product__writing_tests__controlling_faults__pause_faults.md` | **Partial:** explicit heal/reboot/resume operations exist, but no campaign-owned recovery phase was found. Antithesis documents clock skips continuing during temporary quiet periods. [control/lib.rs:318](../../../crates/seedbox-control/src/lib.rs:318). |
| Results: searchable history | Search structured events and logs across branches, including temporal relationships. | `product__understanding_test_results__logs_explorer.md`; `reference__event-set-reference.md` | **Partial:** streaming logs/events, world observations, fault timelines, and a TUI exist; cross-branch temporal search does not. Histories have shared bounded budgets. [http.rs:45](../../../crates/seedbox-server/src/http.rs:45), [manager.rs:565](../../../crates/seedbox-server/src/manager.rs:565). |
| Results: findings and triage | Group examples by property, track new/ongoing/rare/resolved findings, and attach artifacts. | `product__understanding_test_results__reports__findings.md`; `product__understanding_test_results__reports__properties.md` | **Missing:** no property/finding resources found in the API. Current status and comparison answer execution, oracle, and replay questions. [status.rs:32](../../../crates/seedbox-api/src/status.rs:32), [http.rs:45](../../../crates/seedbox-server/src/http.rs:45). |
| Debugging: reproduction | Return to a recorded execution and inspect evidence around a failure. | `product__debugging__simple_mvd.md` | **Have foundations:** replay compares retained results; offline diff identifies witness divergence. [simulations.rs:355](../../../crates/seedbox/src/cli/simulations.rs:355), [diff.rs:80](../../../crates/seedbox-artifacts/src/diff.rs:80). |
| Debugging: alternate futures | Branch from a moment, run commands, extract files, and test counterfactuals. | `product__debugging__advanced_multiverse_debugging__moment_branch.md`; `product__debugging__simple_mvd.md` | **Partial:** explicit checkpoint branching and override editing exist. Arbitrary-moment navigation, guest command execution, and file extraction were not found. [simulations.rs:407](../../../crates/seedbox/src/cli/simulations.rs:407), [timeline.rs:122](../../../crates/seedbox-server/src/timeline.rs:122). |
| Debugging: statistical investigation | Resample futures from earlier moments and show where failure frequency changes; compose investigations in a notebook. | `product__debugging__causality_analysis.md`; `product__debugging__advanced_multiverse_debugging__antithesis_notebook.md` | **Missing:** branch primitives exist, but no resampling analysis or notebook/query layer was found. [http.rs:45](../../../crates/seedbox-server/src/http.rs:45). |
| Workflow: CI and agents | Launch short development tests and longer nightly campaigns; query results and debug through CLI/API and agent skills. | `workflows__dev-loop-workflow.md`; `workflows__deployment-loop-workflow.md`; `ai__skills.md` | **Partial:** scriptable server/CLI and structured output exist. Campaign assessment, finding histories, and equivalent agent workflows are missing. [simulations.rs:184](../../../crates/seedbox/src/cli/simulations.rs:184), [http.rs:45](../../../crates/seedbox-server/src/http.rs:45). |

Recent Antithesis direction reinforces the importance of the surrounding workflow: faster debugging startup, streaming setup, event-search APIs, retroactive logging, assertion catalog completeness, and SDK consistency all feature prominently in `release_notes.md`. Matching its fault list alone would miss much of the product.

## 2. Where Seedbox can be better, not just equal

**Inspectable storage failure semantics are a credible advantage.** Seedbox already distinguishes stable and volatile sectors, makes completed flushes durable, and optionally preserves a seeded subset of unflushed sectors on crash. Its model is concrete enough to explain a failure sector by sector ([disk.rs:1](../../../crates/seedbox-engine-core/src/disk.rs:1)).

The comparison needs precision: Antithesis documents restart behavior that can preserve synced filesystem changes or discard modified data, plus customized durability through support. It does not document a request-level block fault interface. Seedbox’s opportunity is therefore explicit, self-service storage contracts: fail this flush, delay that completion, or crash with this volatile state. The current model retains only the latest unflushed version per sector; it does not enumerate every possible writeback history.

Also, the implemented block interface is writeback plus FLUSH. Despite the internals document’s FUA wording, the x86 test explicitly specifies “no FUA” ([virtio_blk.rs:221](../../../crates/seedbox-vmm/src/virtio_blk.rs:221)). Product claims should follow the implementation.

**Event-relative faults are useful when their timing contract is honest.** Seedbox already supports committed-output marker triggers ([triggers.rs:169](../../../crates/seedbox-engine-core/src/triggers.rs:169)). Extending this to typed workload events, block operations, and armed timers can make rare boundaries easy to exercise.

However, observing a flush after completion cannot cause a crash before it. A hardware timer usually represents a multiplexed Linux deadline, not “the database election timeout.” Antithesis also lets workloads invoke custom faults at specific circumstances. The stronger differentiation is a documented connection between a semantic marker or device phase and an actual injected effect—not a claim that Seedbox understands application intent automatically.

**Local operation and portable evidence can materially shorten debugging.** Seedbox already runs on Apple Silicon and Linux x86-64, with retained inputs and offline comparison. It can make “download the failure and investigate locally” a primary workflow. Antithesis’s documented multiverse debugger uses its service and an active debugging run.

This advantage is incomplete today: public replay takes a session simulation ID, and server shutdown removes session evidence ([daemon.rs:204](../../../crates/seedbox/src/cli/daemon.rs:204)). Offline bundles exist, but their import is not integrated into the public simulation catalogue ([mod.rs:1048](../../../crates/seedbox/src/cli/mod.rs:1048)). Replay also needs suitable native hardware; portability must not imply cross-architecture MATCH.

**Open inspection is useful, but must become usable evidence.** Users can inspect the scheduler, RNG, disk policies, and witness comparison. The product should expose these through fault receipts and failure explanations: what matched, what changed, what survived, and what remained unknown. Source availability alone does not establish stronger determinism or better bug-finding performance.

Per-machine clock faults and ARM support are additional concrete differences. Neither proves broader concurrency coverage: the current single-vCPU model cannot explore true multicore weak-memory behavior.

## 3. Recommended functionality, prioritized

Sizes are relative engineering scope, including both native backends and validation: **S** localized, **M** several components, **L** substantial vertical feature, **XL** major execution or product work.

| Priority | User-visible behavior and value | Dependencies / size | Deliberately exclude |
|---|---|---|---|
| **P0: Structured properties** | Declare stable properties; see violations, witnesses, unvisited checks, and incomplete evidence. Finds silent corruption and exposes vacuous tests. | Reliable guest event path, reducers, API/CLI. **L** | Full temporal logic; broad language support initially; silently changing oracle verdict semantics. |
| **P0: Durable reproductions and first triage** | Save a finding with inputs, evidence, property details, and a reproduction command; import it after server shutdown. Makes every discovered bug actionable. | Extend existing artifact/bundle machinery and server import/export. **L** | Persistent execution catalogue, cloud storage service, automatic “fixed” classification. |
| **P0: Useful oracle toolkit** | Rust helpers for readiness, structured choices, bounded parallel work, recovery checks, and final property summaries. Include examples with explicit durability/consistency assumptions. | Property SDK; existing controls/templates. **M** | A host scenario runner or replacement workload language. |
| **P1: Guided campaigns** | Explore from fresh starts and selected checkpoints, favoring new semantic states and unmet reachability goals; report effective exploration. | P0 evidence, reproducible branch mutation, checkpoint budgeting. **XL** | Reinforcement learning, distributed execution fleet, instruction coverage as a prerequisite. |
| **P1: Targeted storage faults** | Fail/delay selected reads, writes, or flushes; hold a request and crash before its completion. Exposes recovery and error-handling bugs. | Device request phases, deterministic fault rules, pending-I/O snapshots. **L–XL** | Arbitrary filesystem corruption, dishonest successful flushes, universal hardware emulation. |
| **P1: Typed event targeting** | Arm faults after named workload events or relative to eligible timer deadlines; show unmatched/rejected rules. Makes targeted scenarios reusable. | Event identities, trigger extension, timeline receipts. **L** | Negative offsets from unseen events; pretending kernel timers identify application operations. |
| **P1: Findings and branch investigation** | Search failures across branches, inspect related faults, open a checkpoint branch, and minimize external interventions. Reduces triage effort. | Durable evidence, campaigns, property identities. **L** | Reactive notebook, unrestricted query code, claims of causal proof. |
| **P1: CI and agent workflow** | Run a bounded campaign, emit a structured assessment, export failures, compare against an explicitly selected baseline. | Stable findings/reproduction APIs. **M** | Hosted CI, issue-tracker automation, autonomous correctness specifications. |
| **P2: Coverage and concurrency guidance** | Add optional block/edge feedback; investigate controlled per-thread perturbation where it improves measured yield. | P1 baseline metrics; translator/kernel design. **XL** | Coverage percentages as correctness evidence; multicore support by implication. |
| **P2: Advanced debugging** | Navigate to retained moments, execute diagnostics on child branches, extract artifacts, and resample futures. | Reliable replay-to-position and guest debug transport. **XL** | A notebook frontend until CLI/API investigations establish demand. |

Deliver P0 as one coherent workflow before expanding the fault catalogue. Measure progress with time to first reproducible failure, distinct known bugs found per host-hour, successful replay rate, meaningful scenarios reached, and faults that actually took effect. Use fixed workloads with known valid and invalid behaviors; raw seed count is insufficient.

## 4. Required architecture

### Ownership and data flow

Keep execution policy in the existing core and orchestration above it:

| Owner | Proposed responsibility |
|---|---|
| New `seedbox-sdk` | Rust guest API, explicit transport handles, declarations, assertions, choices, lifecycle markers. Thin language adapters later. |
| `seedbox-control` | Language-neutral public guest messages and typed targeting operations. Separate event submission from privileged world control. |
| `seedbox-engine-protocol` | Versioned worker/evidence envelopes, property records, campaign receipts, checkpoint extensions. No policy. |
| `seedbox-engine-core` | Deterministic property reduction, choices, trigger matching, device fault semantics, snapshot state, witness contributions. |
| New `seedbox-explore` | Passive Rust campaign planner: state selection, mutation, feedback aggregation, bounded corpus. No worker processes or async runtime. |
| `seedbox-server` | Campaign lifecycle, capacity reservations, child submissions, cancellation, findings/query API. |
| `seedbox-application` | Preparation and worker request/report adaptation; remains independent of server and CLI. |
| `seedbox-artifacts` | Offline property reduction/reporting, evidence indexing, findings, bundles, and reproduction validation. No engine-core dependency. |
| CLI/TUI | Presentation and explicit submissions; no duplicated scheduling or fault interpretation. |

The main flow is: guest events → deterministic engine processing → retained evidence and feedback → campaign planner → ordinary child simulation submissions. Best-effort live notifications are projections of that evidence.

### A. Exploration engine

Introduce `CampaignRequest`, `ExplorationPolicy`, `CampaignBudget`, `CandidateState`, `BranchRecipe`, and `Feedback`. A campaign owns child simulations; a child remains independently replayable.

Start with a transparent algorithm:

1. Run several fresh seeds to reach different initial states.
2. Request checkpoints after new property outcomes, named scenario milestones, recovery transitions, or new bounded state buckets.
3. Keep a bounded corpus, favoring novelty and shorter prefixes while reserving a fixed share for uniform exploration.
4. Select a checkpoint and vary future entropy, structured choices, or external fault rules.
5. Evaluate the continuation, retain useful states and failures, and repeat.

Do not deduplicate states using raw RAM or disk equality as a proxy for semantic equivalence. Initially bucket declared features—leader changes, successful post-crash reads, outstanding-operation counts—and accept that this heuristic misses distinctions.

Existing snapshots already retain scheduler continuation, controls, faults, RNG-related state, and the input-program cursor ([world.rs:1342](../../../crates/seedbox-engine-core/src/world.rs:1342)). Reuse them. Same-seed continuation must preserve captured streams; entropy-changing branches must reseed guest entropy too, as the current implementation does.

Checkpoint selection must happen through the windowed coordinator at a committed, snapshot-safe boundary. An interesting event may precede the actual checkpoint; retain both positions. Do not acknowledge capture before backend persistence succeeds. Current live retention is capped at eight snapshots ([checkpoint.rs:124](../../../crates/seedbox-engine-core/src/checkpoint.rs:124)); a campaign needs byte-accounted retention, release operations, and explicit ownership of shared checkpoint assets before scaling.

**Budgets belong to campaign and execution requests**, never `seedbox.yaml` or resolved simulation definitions. Bound branches, planner work, checkpoint bytes, retained evidence, and continuation work. Keep wall-time cancellation separate. A continuation stopped before an oracle verdict remains incomplete.

For reproducible planning, assign branch ordinals before dispatch, process feedback in ordinal order, and use a fixed logical batch size independent of worker count. Host timing can determine how much work finishes before cancellation, but must not alter any branch’s recorded inputs or witness. Journal selected recipes so replay never needs to rerun the guidance algorithm.

Initially guide on assertions and semantic milestones. Later add coverage only after measuring incremental yield. Existing progress instrumentation is a useful insertion point, but coverage callbacks must not accidentally add application instruction charges, consume existing RNG streams, or rely on translator cache creation order. Use campaign-local module-relative identities; defer JIT coverage until its identity contract is defined.

### B. Guest SDK and property model

Define a small model:

- `PropertyDeclaration { id, description, kind, scope, location }`
- `PropertyEncounter { id, condition, details }`
- `ScenarioEvent { name, fields }`
- `ChoiceRequest { site, occurrence, alternatives }`
- Lifecycle markers for setup, recovery, and completion.

Use explicit stable IDs; descriptions and source locations are presentation. Reject conflicting declarations deterministically. Register declarations independently of encounters so unexecuted assertions remain visible. Dynamically loaded modules cannot establish a complete catalog merely by never loading; allow the oracle to declare expected properties explicitly.

Support always, always-if-reached, sometimes, reachable, and unreachable. Keep safety violations distinct from unmet exploration goals and incomplete evaluation. “Sometimes succeeded” across a campaign does not establish that every execution eventually succeeds. Recovery properties need explicit assumptions about healthy quorum, delivery, workload termination, and their checking interval.

**Do not derive correctness from the existing live event stream.** Its contract explicitly permits loss and forbids delivery outcomes from affecting execution ([observation.rs:1](../../../crates/seedbox-engine-core/src/observation.rs:1)).

Add a dedicated framed guest channel, available to every machine. Reuse the existing virtio management transport mechanisms, but do not grant every application oracle control privileges: current control admission intentionally accepts only the designated controller ([control.rs:554](../../../crates/seedbox-engine-core/src/control.rs:554)).

The protocol should:

- Bound frames, declarations, details, and outstanding requests.
- Attribute records to machine, incarnation, process/session, and local sequence.
- Define an encounter’s engine position at complete-frame acceptance, rather than trusting a supplied timestamp.
- Acknowledge deterministic acceptance; document that unacknowledged emission interrupted by a crash is uncertain.
- Merge records at coordinator commit using a fixed order.
- Retain reducer state, partial frames, counters, and pending responses in checkpoints.
- Include semantic records or their digest in deterministic evidence.
- Fail explicitly on semantic capacity exhaustion; never silently discard a counterexample.

Live UI publication remains droppable. Host storage failures are execution/evidence failures, not application property violations. Expensive details may be truncated under deterministic size rules while retaining the violation and truncation marker.

Assertions remain nonfatal. Preserve the existing oracle-only `TestVerdict`: SDK-assisted oracles can query the committed summary and choose their exit status. Campaign assessment can separately fail on property violations. If ordinary `run` should fail despite oracle exit zero, that requires an explicit change to the documented verdict contract, not an incidental implementation decision.

Structured choices should use named streams and retained decision identities. Replay consumes the same choices; exploration mutates future choice inputs. Start in the oracle, avoiding a synchronous host round trip for every application random draw.

### C. Fault expansion and precise targeting

Add typed `TriggerSpec`, `EventSelector`, `FaultRule`, and `FaultReceipt`. Receipts distinguish armed, matched, applied, ineffective, expired, and rejected states. A useful report says “matched flush 17; returned IOERR before durability” rather than merely “disk fault injected.”

All external rules and mutations must enter the **one override program**. Oracle-generated controls remain generated by the oracle. Do not replay their observed effects as additional overrides. Extend the typed action representation where necessary; retain stable authored keys and allocate execution fault IDs only at admission. Preserve same-time document order and the immutable consumed checkpoint prefix ([input_program.rs:37](../../../crates/seedbox-engine-core/src/input_program.rs:37)).

There are three distinct timing contracts:

1. **After an observed event:** use a future effect time respecting the coordinator’s causal latency. Existing marker triggers already enforce that an effect cannot land in committed machine past.
2. **Relative to an armed timer:** bind to machine incarnation and timer generation. Only schedule a pre-expiry effect while that boundary is still in the future; cancellation/rearming invalidates or rematches according to explicit policy.
3. **At a device request phase:** install a rule before execution and intercept the request locally, before the selected effect occurs.

For storage, split the shared block model into admission, operation effects, and completion. Today `notify` processes requests synchronously, including disk mutation and used-ring writes ([virtio_block.rs:172](../../../crates/seedbox-engine-core/src/virtio_block.rs:172)). Introduce explicit pending requests and simulated completion deadlines.

Begin with read/write/flush IOERR and delayed completion. A failed flush must not report successful durability. Distinguish failure before any effect from a later, explicitly modeled partial effect. Preserve completed-prefix behavior, dirty-write ordering, IRQ timing, queue resets, and pending state across checkpoints.

A practical targeted crash can mean: **hold the next matching flush, leave it uncompleted, then crash the machine at the next committed world boundary**. This preserves coordinator lifecycle ownership and establishes a precise durability scenario. It does not claim an instantaneous global crash at the original request timestamp.

Exact global intervention at an unexpectedly discovered point inside a parallel round is substantially harder. Other machines may already have executed beyond it. Supporting that requires a validated barrier/replay or rollback mechanism within the shared windowed scheduler—not firing a callback from whichever worker finishes first. Defer that stronger promise.

Treat exhausted storage capacity separately from block IOERR: realistic ENOSPC belongs to filesystem capacity and allocation behavior. Likewise, corruption of already durable data is a distinct fault contract, not ordinary crash loss.

### D. Findings, reproduction, and debugging

Introduce `Moment`, `PropertyResult`, `Finding`, `Example`, and `ReproductionRecipe`. A moment needs a branch identity and deterministic position/sequence, not time alone; several observations can share a timestamp.

A finding should retain:

- Stable property or diagnostic identity and bounded grouping fields.
- A representative failing moment and relevant passing example.
- Actual fault effects, rejected actions, and evidence limitations.
- A self-contained resolved definition, retained engine/kernel/guest assets, and any checkpoint closure.
- Expected witness/property evidence and a reproduction command.

Make export/import work through server resource IDs and offline bundles. Preserve the session-only catalogue: durable reports and reproduction archives live outside session scratch storage and are imported explicitly. Parent simulation IDs may describe ancestry but cannot be required to replay a child.

Start triage grouping with property ID plus caller-selected detail fields. Do not assume all violations of one invariant share a root cause. Across campaigns, report “new,” “observed again,” and “not observed”; reserve “resolved” for a user decision or a carefully defined statistical policy.

Add a bounded event-query API for machine, property, fault, time interval, branch ancestry, and “preceded by within interval.” Include pagination and incomplete-history markers. The TUI should open a finding into its timeline and branch context; edits continue using one working copy with frozen submissions.

Minimization first removes external overrides and reduces their parameters while preserving dependencies and the same property violation. Re-run the oracle each time. Do not turn oracle events into replay inputs, and do not count an invalid scenario or checker timeout as a successful reduction.

Multiverse-style debugging should grow from explicit checkpoint branching. Later, replay from a retained checkpoint to a moment and execute diagnostic commands on a child branch. Guest inspection can perturb execution; file extraction through a running guest belongs to that branch. Passive inspection of retained snapshots can remain observational.

Resampling earlier checkpoints can show where a failure becomes more frequent under a stated continuation policy. Report sample counts and uncertainty; correlation, adaptive sampling, and unequal exposure prevent calling that graph proof of causality.

**Validation requirements:** use small independent models for property reduction, trigger ordering, and storage durability; test snapshot continuation and frame fragmentation; exercise same-time oracle/override ordering and rejection; compare serial and parallel execution with reordered worker completion. Run portable checks and both relevant native suites. Test fresh executions as well as retained-engine replay: MATCH alone does not validate a changed engine.

## 5. Open questions and risks

1. **Initial users and workloads.** I recommend storage engines, databases, and replicated services first. They fit the existing disk, network, checkpoint, and oracle machinery. Broad Kubernetes parity would consume substantial effort before strengthening the core testing loop.

2. **Meaning of failure.** Should property violations be an additional assessment alongside the oracle verdict, or redefine ordinary simulation pass/fail? The former preserves today’s contract and is the recommended starting point.

3. **Evidence lifetime.** Decide whether campaigns automatically export failures to a caller-selected destination or require explicit export. Either can preserve the session-only server model; losing findings on shutdown cannot remain the default campaign experience.

4. **Targeting precision.** Decide whether “hold this flush, then crash at the next boundary” meets the first storage use cases. Exact arbitrary-event global cut points should be justified by bugs the simpler contract cannot expose.

5. **Exploration efficiency.** Full snapshots may cost more than restarting short tests. Measure capture/restore time, memory, corpus value, and useful continuations before implementing incremental snapshots or a large checkpoint cache.

6. **SDK scope.** Start with Rust plus a documented wire format and a small shell helper. Preserve existing Python controls; add bindings when users need them. The existing C supervisor/translator bridge is a reasonable exception to Rust-first reusable tooling.

7. **Trust in properties.** A perfect replay of a bad oracle is still a bad test. Ship examples with explicit fault assumptions, valid edge cases, and deliberately broken variants that demonstrate detection. Preserve inconclusive outcomes for bounded history checkers.

8. **Coverage and concurrency limits.** Progress instrumentation does not provide instruction coverage, thread scheduling control, or multicore memory-model exploration automatically. Each is a separate investment with its own determinism risks.

9. **Compatibility promises.** Keep versioned protocols and retained executable assets, but introduce no source stamps, qualification lists, or replay admission pins. Replay MATCH remains evidence about results under the retained execution contract; broader host and architecture portability must be demonstrated separately.