I agree with the overall direction and most of the revised sequencing. I change my recommendation on two points: **the seeded fault generator belongs in P0**, and **native assertions do not need an engine acknowledgement per record**. The remaining amendments concern observable semantics and implementation dependencies.

This response is based on read-only inspection of ec80ea9f and the supplied Antithesis documentation. The proposed mechanisms below remain untested.

**1. AGREE — recommend option C, preserving the separate verdict and assessment.**

Keep `TestVerdict` oracle-only. That is an explicit contract in [status.rs:55](../../../../crates/seedbox-api/src/status.rs:55), not merely current implementation behavior.

Our joint recommendation should be:

- `run` retains its current exit behavior by default and reports the property assessment separately.
- `--fail-on-violation` makes an otherwise successful command exit 1 when safety violations exist.
- Campaigns use the assessment by default.
- Unmet exploration goals are reported separately and become command failures only when explicitly required.
- Neither reporting policy rewrites the stored oracle verdict.

Two details need to accompany that recommendation.

First, preserve execution-error precedence. Today seed sweeps prioritize ERROR, TIMEOUT and INCOMPLETE ahead of FAIL ([simulations.rs:242](../../../../crates/seedbox/src/cli/simulations.rs:242)). An interrupted campaign can retain a definite safety violation while still reporting that execution was incomplete. Do not conceal either fact.

Second, distinguish **violations, unmet obligations and incomplete evidence**. A declared `always` that was never encountered is not a demonstrated safety violation, but it must not appear as passed: Antithesis gives `always` a reachability obligation too (`reference__sdk__fallback__assert.md`). Likewise, “never ready” needs a defined completion condition; reaching a host timeout alone should not manufacture a safety failure.

With the flag, print one status line consistent with the command’s exit policy, followed by separate oracle and property fields. This remains a presentation policy, not a result-changing simulation input.

**2. AMEND — accept buffered transport and compatibility, but narrow the compatibility promise.**

I withdraw the blanket per-record acknowledgement requirement. A framed native socket with an explicit commit barrier provides the necessary contract without imposing a round trip on every assertion.

The native contract should be:

- Successful `emit` means the guest transport accepted the record.
- Successful `flush` means the engine has committed and reduced all preceding records from that producer session.
- A crash can discard records that have not crossed that boundary.
- Checkpoints retain framing state, producer sequencing, committed watermarks and reducer state.
- Overflow, malformed native messages and known evidence loss are explicit; they cannot silently become a passing assessment.

A flush is not automatically a barrier across other processes or machines. An oracle querying a committed summary needs the associated watermarks, and the toolkit should provide an explicit coordinated finalization operation when a workload needs that stronger guarantee. Optional `emit_and_flush` is useful for a critical final assertion; it need not be the default.

The existing termination drain does not yet supply this guarantee. It drains a snapshot of queued stdout/stderr bytes and deliberately does not wait for descendant writers ([compose-init.c:495](../../../../guest/compose-init.c:495)). Extend that bounded termination protocol to SDK sessions and order final exit after the accepted SDK prefix. Do not assume the current function already flushes assertions.

I accept format plus pure reducer in `seedbox-engine-protocol`, provided its scope is **fixed evidence semantics**: declaration consistency, boolean observations, counts, examples and completeness. Default-property policy, campaign scoring and CLI exit decisions belong elsewhere. Engine-core checkpoints the reducer; artifacts invokes the same reducer offline. Shared implementation prevents drift, but independent expected-result fixtures are still needed to catch shared mistakes.

I also accept **Antithesis compatibility as an adapter**. It is a useful adoption feature. I reject only the claim that redirecting a pathname gives reliable, unmodified support for every SDK at small cost:

- The Rust and Python docs promise a **log file**, not a FIFO or socket (`reference__sdk__rust.md:75`, `reference__sdk__python.md:114`). Opening, truncation, buffering and multiprocess behavior require verification.
- Interleaving beyond `PIPE_BUF` is not guaranteed to produce invalid JSON. Malformed-line detection cannot prove that records were preserved correctly.
- Catalog behavior differs. Python documents a separate scan/register workflow; receiving encountered assertions alone cannot identify every unvisited property.
- The fallback documentation recommends emitting only the first true and first false observation. Imported record counts therefore cannot be presented as total assertion-evaluation counts (`reference__sdk__fallback__assert.md:95`).

Recommendation: ship a schema adapter and a verified capture path for an initial SDK subset, with explicit catalog completeness. Expand language support after checking each SDK’s file behavior. Opt-in environment injection belongs in resolved inputs. The adapter supplies assertion compatibility, not automatically equivalent platform randomness or workload scheduling.

I accept a configurable Antithesis-compatible template directory in the later runner, with a Seedbox default.

**3. AGREE — move the seeded generator into P0.**

Your argument is right. Once generation is a pure operation that materializes ordinary inputs, it delivers useful standalone testing and becomes a campaign component rather than throwaway work.

The Antithesis docs support the product value: continuously injected faults are part of the basic workload-testing loop (`product__writing_tests__controlling_faults.md`, `product__writing_tests.md`). Waiting for checkpoint selection would unnecessarily postpone that value.

Recommended P0 scope:

- Opt-in fault profile, candidate count or generation window supplied as request options.
- Pure Rust sampler, with explicit seed domains and bounded output.
- Materialized keyed overrides, visible and editable through the existing timeline.
- Replay executes those overrides without invoking the sampler.
- Generated starts and cleanup actions are distinguished semantically.

Seed-range integration requires more than attaching one generated schedule to the source simulation. The current implementation clones the source spec and changes only its seed ([seeds.rs:206](../../../../crates/seedbox-server/src/catalogue/seeds.rs:206)). Each fresh member must materialize its own schedule; replay must retain that member’s materialization.

Two integration requirements affect P0:

**Readiness.** The smallest version can generate absolute-time candidates and deterministically suppress starts before setup completes. It must report skipped candidates. If the promised behavior is “generate N candidates *after* setup,” then setup-relative activation is an additional input-program feature and must land with that promise.

**Quiet periods.** Quiet must not suppress cleanup. For example, a partition starts at T, quiet begins at T+1, and its scheduled heal occurs at T+2. Rejecting every generated override inside quiet leaves the partition active indefinitely. Suppress new fault starts while allowing teardown; offer an explicit operation to heal active faults in the selected generated group. Oracle and user-authored actions remain unaffected unless explicitly included. This grouping must be semantic ownership in the fault model, not provenance annotations.

Those details do not justify deferring the generator. They define its minimum useful integration.

**4. AMEND — use the one timeline and require cold equivalence, but define the branch position precisely.**

There is no reason to maintain a separate mutation journal when the override timeline can express the recipe. I accept:

> Original root inputs + consumed external-input prefix + explicit reseed + suffix.

Oracle calls remain generated by the oracle. They are not copied into the prefix as external overrides.

I also accept cold/checkpoint equivalence as a required invariant for campaign paths advertised as reproducible without snapshots. However, **adding `reseed` to today’s `at_ns` action list is not sufficient**.

The code distinguishes two capture positions:

- A timed checkpoint precedes every action at T ([world_windowed.rs:992](../../../../crates/seedbox-engine-core/src/world_windowed.rs:992)).
- An interactive checkpoint is requested after `advance_boundaries`, and restore re-enters that existing boundary ([world_windowed.rs:903](../../../../crates/seedbox-engine-core/src/world_windowed.rs:903)).

Moreover, the input format explicitly treats recorded scheduler positions as observations, not timing inputs ([input_program.rs:71](../../../../crates/seedbox-engine-protocol/src/input_program.rs:71)).

A concrete failing schedule is a random-sampling action at T followed by an interactive checkpoint. The warm branch retains the already-sampled effect. A cold recipe that places reseeding before that action can change the sample. Equal timestamps do not make those executions equivalent.

My recommended first implementation is:

1. Restrict campaign branch points to one explicitly defined coordinator phase.
2. Add a typed reseed input operation with explicit seed, branch identity and a precise prefix/phase anchor.
3. Give that operation the same semantics in cold execution and checkpoint continuation.
4. Preserve already-sampled effects; change only future draws.
5. Reject stale anchors after edits rather than silently applying them at a different position.

This deliberately extends the timeline schema. It does not reinterpret existing observational boundary counters as authored timing.

The current reseed function already preserves sampled effects and treats the parent seed as continuation without reseeding ([world.rs:1276](../../../../crates/seedbox-engine-core/src/world.rs:1276)). The new operation must settle that same-seed behavior explicitly. It should also be a typed input operation, not a pretend fault that consumes fault IDs.

Required validation compares cold versus restored continuation, with and without reseeding, including same-time controls, triggers, lifecycle actions and queued traffic, on both backends. Compare witness, properties and final state. This is **L-sized foundational work**, not an assumed property of the current checkpoint API.

After that invariant holds, campaign findings can omit checkpoint RAM. Existing snapshot-origin inputs that lack a reconstructible root recipe still need their checkpoint closure. Checkpoints remain valid reproduction assets as well as optional caches.

**5. AMEND — the held-request mechanism is sufficient, but the instantaneous-crash equivalence argument is too strong.**

I accept the simpler mechanism: a terminal held request does not require a general asynchronous block device.

The contract should say exactly:

> Suspend the machine when the selected request is admitted; perform no selected-request disk mutation or completion; commit its crash at the next eligible coordinator boundary.

Record both match time and crash-effect time.

Three holes need addressing.

**Already-queued traffic.** Minimum link latency prevents outputs produced in this round from arriving within that round. It does not eliminate inputs scheduled by earlier rounds. Those inputs must remain unconsumed when the machine suspends.

The scheduler already supports a suspended report with a consumed-input prefix ([window/plan.rs:143](../../../../crates/seedbox-engine-core/src/window/plan.rs:143)). Its lifecycle semantics also retain transit frames across crash: reboot drops frames that arrived while down and permits later arrivals ([world_windowed.rs:815](../../../../crates/seedbox-engine-core/src/world_windowed.rs:815)). Therefore “all addressed frames are dropped after commit” is not the existing behavior.

**Parking is not a drop-in operation.** KVM’s current `park` rejects owed I/O, completes the architectural exit, and records a witness marker ([machine.rs:826](../../../../crates/seedbox-vmm/src/machine.rs:826)). ARM also checks owed I/O before its park RPC ([world_adapter.rs:588](../../../../crates/seedbox-arm64/src/application/world_adapter.rs:588)).

Introduce an explicit terminal suspension reason. Distinguish completion of the CPU’s MMIO instruction from virtio request completion. Preserve earlier completed requests in the same notification, including their dirty writes and IRQ state; withhold the selected request and stop subsequent processing. Do not accidentally repay simulated I/O cost by executing the guest.

**Oracle exit in the matching round.** The coordinator currently ends the experiment before future actions once the oracle has exited ([world_windowed.rs:979](../../../../crates/seedbox-engine-core/src/world_windowed.rs:979)). Recommended behavior is to preserve that cutoff and report the pending crash as cut off, rather than introduce a special post-oracle lifecycle action.

For v1, do not expose a reusable checkpoint containing an unresolved held request: resolve the crash or terminal cutoff first. Preserve existing crash-transaction poisoning on failure.

With these provisions, the optimization is sound as a proposed design. We need not claim that it is observationally identical to an instantaneous crash.

**6. AGREE — synchronous IOERR first; asynchronous latency and explicit corruption later.**

The revised scope is appropriate.

P0 storage should return deterministic read/write/flush IOERR for otherwise valid requests, preserving descriptor-validation precedence and completed-prefix behavior. In particular:

- Failed writes do not mutate the modeled disk.
- Failed flushes do not move volatile data into stable storage.
- Earlier successful requests remain completed.

That requires choosing the fault outcome before the mutations currently performed synchronously in [virtio_block.rs:172](../../../../crates/seedbox-engine-core/src/virtio_block.rs:172).

Held-request crashes belong with the P1 scheduler work described above. True latency requires pending requests and completion deadlines so other guest threads can run; it remains a later L item.

I accept **silent corruption as P2 opt-in**. My objection was to treating it as part of the baseline crash-durability contract, not to testing that fault class. Distinguish:

- Corrupting bytes returned by a read while leaving stored data unchanged.
- Mutating stable stored sectors.

They exercise different guarantees and need separate evidence. The workload must state the redundancy, detection or recovery property being tested; a single-copy application cannot universally promise recovery from arbitrary durable-data corruption.

We agree to exclude lying flush from the baseline plan and to use the actual FLUSH-only device contract, not the stale FUA documentation.

**7. AGREE — P1 measurement spike, conditional P2 feature.**

The proposed coverage sequence is right.

Start guidance with assertions, semantic milestones and structured workload choices. During P1, prototype bounded module-relative edge collection and measure its incremental discovery value against that baseline.

The spike should measure:

- Discovery across several fixed workloads and multiple campaign seeds.
- Host cost, memory consumption and restore cost.
- Repeated-run MATCH and worker-count independence with coverage enabled.
- Whether unstable identities, including JIT code, dominate apparent novelty.

Coverage configuration belongs in resolved inputs because it can change guest allocations, syscalls and timing. We should promise replay within the configured mode, not identical witnesses with coverage enabled and disabled.

Proceed to a product feature only if the discovery improvement justifies its cost. Neither “already using DynamoRIO” nor extra edges alone establishes that benefit.

**8. AMEND — accept the phases, but deliver P0 in slices and tighten the evaluation criteria.**

The merged sequence is substantially right. I recommend the following dependency order.

| Phase | Deliverable and sequence | Completion evidence |
|---|---|---|
| **P0a: trustworthy single-run findings** | Native framed transport and flush; format/reducer; `PropertyAssessment`; `get properties`; supervisor-observed exits; finding export and explicit import into a fresh server. Minimal Rust finalization helpers. | A known violation survives export, server shutdown and import; replay produces MATCH and the same property evidence. Passing and incomplete cases remain distinguishable. |
| **P0b: useful automated fault testing** | Seeded finite generator; readiness admission and quiet/recovery semantics; synchronous storage IOERR; Rust oracle toolkit expansion. Add kernel OOM/fatal-termination evidence and the first verified compatibility adapters. | Fault-driven bugs produce durable reproductions; quiet permits recovery; invalid admissions and evidence failures are visible. |
| **P1a: reproducible branching foundation** | Typed reseed and precise branch anchors; cold/restore equivalence; checkpoint byte accounting and eviction; typed event predicates and held-request crashes. | Cold and checkpoint executions agree on both architectures, including same-time ordering and terminal cutoffs. |
| **P1b: guided campaigns and useful triage** | Generation-synchronous planner; semantic guidance; campaign/property aggregation; findings queries; external-input shrinking; CI/agent workflow. Run the bounded coverage spike. | Improved discovery over seed sweep plus generator on actual rare-bug workloads; equivalent logical plans and findings across worker counts at equal completed budgets. |
| **P2: measured extensions** | Snapshot-tree optimization if restore cost warrants it; coverage if useful; seeded preemption; asynchronous block delays; counterfactual resampling; derived-branch exec; scoped passive extraction; template runner; opt-in corruption. | Feature-specific discovery or debugging benefit, with replay preserved. |

Two sequencing qualifications matter.

**Do not make every kernel detector a prerequisite for the first vertical slice.** Supervisor process-exit evidence can ship first. Kernel detection must distinguish actual fatal termination from signal delivery: a delivered signal can be handled. Expected injected kills must not automatically trigger “unexpected crash.” Machine readiness also needs a separate application-setup assertion; launching a process does not establish application health.

**Use `-plant` for checker sanity, not explorer evaluation.** The etcd option explicitly adds a never-written acknowledgement ([main.go:214](../../../../examples/etcd/workload/main.go:214)). It is good evidence that the checker and finding pipeline work. It cannot establish that checkpoint selection discovers difficult schedules better than a seed sweep.

For campaign evaluation, use bugs requiring meaningful timing or state combinations, plus correct negative controls. Compare repeated campaign seeds under fixed work budgets; report unsuccessful searches as censored observations. Time-to-first-failure is useful alongside distinct bugs found and execution/checkpoint cost.

Worker-count independence should compare completed logical budgets, branch recipes and semantic findings. Identical findings at identical **wall-time deadlines** are not a reasonable promise when parallelism changes how much work completes.

**Additional disagreements the merged plan should resolve**

- **Shrinking is not automatically globally minimal.** Use dependency-aware ddmin over external inputs, including teardown relationships and valid reseed anchors. Report “reduced” or “1-minimal under these transformations.” A changed prefix invalidates later checkpoint reuse and may invalidate branch anchors. Never shrink oracle-generated controls as if they were external inputs.

- **Limits need separate active and historical accounting.** `MAX_TRIGGERS` limits total triggers armed, not just active matchers ([lib.rs:197](../../../../crates/seedbox-control/src/lib.rs:197)). Recycling historical IDs would break references and retained evidence. Reclaim active matcher storage separately. The input program also caps entries at 4096 and rejects `/` in keys, so the proposed `x/<node>/<ordinal>/<n>` keys must change ([input_program.rs:5](../../../../crates/seedbox-engine-protocol/src/input_program.rs:5)).

- **Verbose re-execution is not equivalent to retroactive logging.** Changing guest logging can alter execution and simulated timing. Refiltering already-retained evidence is observational; adding logging belongs to a derived experiment. The original plan’s equivalence claim should be removed.

- **Passive extraction has a narrower promise than “read any guest file.”** Device snapshots do not include dirty guest page-cache contents or tmpfs. Name the view being inspected: stable post-crash disk, latest device-visible disk, or a derived guest filesystem view. An unclean filesystem may also require journal interpretation.

- **Adaptive branch frequency is not a natural failure probability.** Label it an observed campaign rate. Counterfactual resampling needs a stated sampling distribution and sample count. Campaign reduction must also avoid counting a shared ancestral assertion repeatedly merely because many children inherit it.

I would adopt this sequencing as the joint recommendation once the branch-position contract, quiet cleanup semantics and bounded compatibility promise are incorporated. The remaining user decisions are the exit-policy option, the initial compatibility scope, and which workloads define useful discovery improvement.