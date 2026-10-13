# Seedbox interface refactor handover

**Temporary handover — REMOVE BEFORE MERGE.** Implementation is paused at the
user's request. This is an incomplete checkpoint, not a claim of API parity.
Before removing this document, move lasting decisions into rustdoc and the
existing architecture documentation.

## Objective and scope

Build an isolated interface-oriented simulation stack that could support the
existing UI/API through a later bridge. Do not connect that bridge yet. Work
belongs in `crates/seedbox-machine` and its own guest/build inputs; do not import
or modify legacy crates to make the new design fit. Existing code is read-only
semantic evidence. Prefer copying a needed mechanism into the new ownership
boundary over introducing a legacy dependency.

PV and TCG must both implement guest-facing capabilities. A compiling trait
implementation or a placeholder backend does not establish support. A future
PMU implementation should fit the same execution boundary, but implementing it
is outside this checkpoint.

## Interface policy

[AGENTS.md](../crates/seedbox-machine/AGENTS.md) freezes the major interfaces:
Machine, Clock, RNG and Driver, including their public supporting types.
**An interface is a contract, not necessarily a Rust trait.**

The freeze includes:

- Trait methods, associated types, bounds and default implementations.
- Public structs and their fields, constructors and inherent methods: for
  example Driver, Barrier, RunContext, MachineDefinition and backend factories.
- Public enums and their variants: for example requests, responses, actions,
  stop reasons and execution outcomes.
- Supporting configuration, identity, queue, event and snapshot boundaries.
- Documented behavior: ownership, ordering, timing, lifecycle, completion,
  failure, snapshot and caller obligations. Keeping a signature unchanged does
  not authorize changing its meaning.

**Agents must obtain explicit user authorization before changing these
contracts.** Approval to build a feature, fix an implementation or refactor code
is not approval to change an interface. Already approved changes do not need
another approval. Error variants are the explicit exception; that exception
must not be used to change a non-error contract indirectly. Private mechanical
changes that preserve the contracts can proceed normally.

Write specifications in terms that apply to every implementation. Do not put
KVM, PMU, QEMU, DynamoRIO or other backend mechanisms into generic contracts.
Explain preconditions, state ownership, what counts as completion, effects on
future behavior, error guarantees and what the caller should do for every
return case. Distinguish enqueueing, admission, execution and completion.
Keep backend explanations beside the implementation. If an implementation
cannot satisfy the interface, review the boundary rather than quietly weakening
it or leaking backend details upward.

The coordinator delegates bounded implementation work and independently reviews
it. Keep one checklist item active, and close it with relevant evidence before
opening the next interface discussion.

## Design and ownership

### Machine and injected dependencies

[Machine](../crates/seedbox-machine/src/machine.rs) exposes `run` and `snapshot`.
`RunContext` supplies worker resources, the clock, the machine's continuing RNG
stream, execution configuration, inbox and outbox. Worker threads are part of
Driver; resources may be thread-affine, while machines can move between workers.

Driver owns the simulation RNG and derives one stream per machine in deterministic
creation order. A stream follows machine identity, not worker assignment. There
is no RNG replacement/reseeding control. Machine snapshots preserve machine-owned
execution and accepted work, but exclude injected dependencies and the wider
simulation. A simulation fork must capture those owners consistently too.

The ownership test is: does an operation change the machine, or change what the
machine encounters when it runs? Clock faults and execution pricing belong to
Driver-owned dependencies. Storage is also intended to become an injected
dependency; its interface and integration remain deferred.

### Driver and ordering

[Driver](../crates/seedbox-machine/src/driver.rs) owns shared time, parallel
execution, playback and one ordered timeline. Current quanta are fixed and
anchored at construction. Intermediate barriers do not shift their endpoints.
Worker count must not change results. Global playback pause holds timeline
admission; an individual machine pause suppresses positive execution budget.

Machine requests and Driver actions share timestamp/FIFO ordering, but machines
see only their own requests. Due requests may receive zero-budget `run` calls:
these adopt elapsed time and make timers eligible without replaying instructions
from a paused interval. Additions and controls happen at barriers.

Outputs commit only after a successful execution batch, ordered by occurrence
time, machine identity and per-machine emission order. Failed batches discard
staged outputs. A later control-processing failure must preserve the already
committed output batch and confirmed results. Failure poisons execution; readable
evidence must not imply rollback or resumability.

### Lifecycle, application and oracle

[Requests and responses](../crates/seedbox-machine/src/pv.rs) carry lifecycle,
application commands, logs, outcomes and simulation-control messages. Crash,
Reboot and Shutdown are requests; pause is Driver policy. Reboot clears individual
pause and old-boot work. Future external schedules bind to the boot at admission.
Application exit does not end the kernel: Start can restart the program within
that boot. Whole-machine exit requires Reboot before positive execution resumes.
Graceful shutdown requires later guest execution and is not guaranteed to finish.

Driver construction requires an oracle definition and a possibly empty list of
other definitions. The oracle is created first. Its application exit determines
the verdict: zero passes; nonzero, confirmed forced Stop, crash or whole-machine
termination fails. Queueing Stop is not confirmation. The result is permanent,
after full-batch publication. Every machine has the same simulation-control
access and remains faultable; the oracle's special responsibility is the verdict.

### Shared control service

The concrete [control service](../crates/seedbox-machine/src/oracle/service.rs)
belongs to Driver. It interprets committed calls and returns effects, replies,
wakes and deadlines for the same timeline. There is no generic EventHandler or
second scheduler. Calls and trigger matches act at their discovery barrier, with
zero additional control latency; their original timestamps remain observations,
not permission to change another machine's past.

Admission and completion are separate. Generated effects join the existing FIFO
tail and revalidate state when dispatched. Accepted work survives caller loss.
Only Applied proves a successful mutation; unknown failure time stays unknown.
Fault IDs, application command IDs, trigger IDs and fault handles have distinct
ownership. Ordinary controls and bare triggers intentionally have different
availability preflight, now represented explicitly instead of inferred from the
presence of an RPC reply destination.

Public control-session IDs persist across crash/reboot, even when private wire
sessions restart. Retired replies are discarded without canceling accepted host
work. Restoring old snapshots requires matching external history or a separate
branch identity. Reply generation, transport completion and application reading
are different facts; the current interface does not expose exact reply delivery.

### Definitions and factories

The common [MachineDefinition](../crates/seedbox-machine/src/definition.rs)
contains a prepared filesystem, launch recipe and resources. Backend factories
own their kernel, helper, firmware and instrumentation privately. Driver sees
only definitions and a retained factory. No runtime build or implicit asset
search belongs in library construction.

PV prepares its own kernel/runtime and mandatory instrumentation; TCG uses its
own ordinary kernel. Guest assets exclude the host executable. Factories own
loaded bytes independently of their source directories. The exact definition
API still needs review before merge.

## Implemented and verified baseline

- Parallel Driver with worker-owned resources, clock/RNG injection, ordered
  lifecycle and application requests, playback, execution pricing and oracle
  completion.
- Shared guest framing/helper, binary logs, application restart and graceful
  shutdown on PV and TCG; snapshot continuation for accepted guest work.
- Automatic control dispatch for Now/Sleep, process and machine controls,
  throttle/heal and streaming triggers. Native tests cover real control RPCs
  and worker-count determinism; positive Sleep/trigger dispatch have portable
  integration coverage but still need native operation coverage.
- Backend factories, owned guest assets and common prepared applications.
  Native preparation tests cover scripts, PATH, non-root credentials,
  execute-only workloads, same-boot restart and metadata preservation. PV's
  instrumentation path passes CPU-cost, partition and mid-work snapshot checks.
- Confirmed fault settlements survive a later malformed acknowledgment and final
  oracle cutoff without early dispatch or duplicate retention. Private admission
  mode changes preserve ordinary-control versus trigger semantics.

Evidence owners, rather than a duplicate test catalogue:

| Area | Owning tests |
| --- | --- |
| Driver ordering, controls, oracle and failures | `tests/driver_*.rs`, `src/driver/*tests.rs` |
| Native control and worker determinism | `tests/driver_guest_services.rs` |
| Prepared workloads and instrumentation | `tests/prepared_application.rs`, `tests/pv_progress.rs`, `src/machine/pv/progress_tests.rs` |
| Guest services and channel | `tests/guest_services.rs`, `tests/simulation_control.rs`, `src/machine/*snapshot_tests.rs` |
| Shared control policy and observations | `src/oracle/service/tests.rs`, `src/oracle/observation/tests.rs`, `src/oracle/triggers/tests.rs` |

Paths in this table are relative to `crates/seedbox-machine`. Latest focused
checks passed: 54 Driver tests, 30 service tests, 8 trigger tests, 10 public
control/trigger tests, and four settlement-retention regressions. A fresh
`cargo check -p seedbox-machine --all-targets` also passed for this checkpoint. Native factory,
Driver, shutdown and instrumentation evidence predates this documentation-only
checkpoint; it is not a fresh native rerun. Keep private unit tests beside their
owner and public multi-component tests under `tests/`. Use focused checks for
this new family, not the whole legacy suite.

## TODO — remaining work

Item 1 is implemented and awaits the user's re-evaluation; items 3 and 6 and
the fault parts of item 4 are implemented in their approved shapes. Do not interpret
this checkpoint or the event stream as completion of the full compatibility
objective.

1. **Host event stream — implemented in the approved shape; re-evaluation
   pending.** `Driver::drain_events` replaced `Barrier::drain_responses` and
   works after failure, stop and finish. Types and the ordering, failure,
   retention and determinism contract live in rustdoc
   ([event.rs](../crates/seedbox-machine/src/driver/event.rs) and
   `Driver::drain_events`). Outputs, MachineAdded, Driver transitions with
   Host/Fault causes, fault admission and settlement, trigger arm/resolution
   and reply admission are published incrementally; the Driver's retained
   settlement history was removed. Confirmed prefixes, including failure-path
   settlements, and successful `Barrier::add` mutations survive later errors.
   Portable coverage is in `tests/driver_events.rs` plus the migrated tests.
   **TODO BEFORE MERGE: re-evaluate this event interface**, as explicitly
   requested by the user. Open points for that review: machine self-exit has
   no event (only its outputs and `Barrier::stopped`); `TriggerResult::Rejected`
   carries no reason; `FaultRecord` omits admission time (the `FaultAdmitted`
   event's `at`) and the application command ID; `FaultId` has no accessor;
   a reply to a session retired within the same boot still publishes
   `ReplyAdmitted` because only the machine sees that discard (item 2).
2. **Reply delivery evidence — requires approval.** Consider a backend-neutral
   `Response::SimulationReplyDelivered { id }`, timestamped when the entire reply
   reaches the program-readable transport with completion published. Inbox
   consumption or host FIFO acceptance is insufficient. Both backends need
   private partial-transfer accounting and snapshot support. A buffered old-session
   reply can complete later under its original identity; do not confuse retirement
   with discard. Silent disconnect/discard timing remains unobservable and needs
   an explicit decision if required. Do not claim delivery parity yet.
3. **Host fault admission — implemented in the approved shape (DECISIONS F).**
   One `Barrier::schedule(at, InputKey, Action)` replaced `schedule`,
   `schedule_request`, `Action::Wake` and `Action::SetExecution`; there are no
   raw host actions any more. Enqueue only checks shape (key syntax, `at >= now`,
   target or spawnable name, process deadline, live key) and reserves no fault,
   handle or command ID. At its FIFO turn the input is admitted through the
   service's ordinary-control path (`Service::admit_input`, sharing
   `prepare`/`install`; triggers keep trigger mode), binding the target boot then.
   Admission publishes `FaultAdmitted` with `FaultOrigin::Input { key }`; refusal
   publishes `InputRejected { key, rejection }` with no fault ID. Generated
   effects join the FIFO tail, so same-time Crash-then-Reboot admits the Crash
   and rejects the Reboot (documented and tested). Process commands use
   Driver-allocated command IDs and a per-input deadline; host Shutdown and
   Spawn are host-only service commands. `Heal(Input(key))` resolves the key's
   handle; the Driver keeps only live keys. Contract in rustdoc
   ([input.rs](../crates/seedbox-machine/src/driver/input.rs),
   `Barrier::schedule`, `Barrier::snapshot`). Internally the service now
   identifies machines by canonical private keys (`machine:N`), so unnamed
   added machines and duplicate display names are targetable by identity.
   Portable evidence: `tests/driver_inputs.rs`, `tests/driver_requests.rs`
   (process commands) plus the migrated suites. Native evidence on TCG and x86
   PV: `tests/driver_guest_services.rs` (every host action on real guests,
   settled by acknowledgment; workers 1/2/4 TCG, 1/4 PV) and
   `src/machine/driver_input_snapshot_tests.rs` (in-place machine snapshots with
   a pending input and an unacknowledged command; workers 1 and 4).
4. **Complete control compatibility — fault parts implemented; Inspect open.**
   `Barrier::pause_before_next_fault` replaced `pause_before_next_crash`: it holds
   before any effective fault effect from guest, trigger or host origin
   (including process dispatch, throttle, heal and spawn), reported as
   `StepOutcome::BeforeFault { at, fault }`. Service-name targeting resolves a
   name to the machine of that name, for host `Target::Name` and guest
   `{"service": ...}` targets alike. Inspect, and how it represents deferred
   networking, remain open; public contracts remain to be agreed.
5. **Injected clock faults.** Agree the Driver-owned dependency boundary, then
   implement step, drift and heal on both backends with correct reads and timers.
   Canonical execution/barrier time must remain coherent. Private projection
   arithmetic is not native support. Realtime versus monotonic/raw behavior,
   inverse timer deadlines, zero-budget application, epoch, reboot and restore
   policy need resolution. Current guest inputs lack the stopped timekeeper
   mechanism; the ordinary TCG kernel constraint must be addressed explicitly.
6. **Oracle-driven machine creation — implemented in the approved shape.**
   `Driver::new` accepts a separate `spawnable: Vec<MachineDefinition>` after
   `machines`; definitions remain unchanged and templating belongs to the caller.
   All names are unique across oracle, initial machines and spawnable definitions,
   validated before factories run. Any guest may call `{"op":"spawn","machine":"name"}`;
   declared inputs stay host-owned rather than travelling as filesystem payloads.
   Admission reserves an insertion-order identity at boot zero and a fault ID;
   unknown, already-admitted and same-barrier duplicate names reserve neither
   identity nor fault ID and draw no seed. At its FIFO effect turn, creation uses
   the retained factory, draws the next machine seed and registers the name.
   MachineAdded(Fault) precedes Applied settlement and reply admission; the machine
   runs from the next round. Factory failure publishes Failed, poisons Driver and
   leaves confirmed event prefixes drainable. Terminal cutoff withholds pending work.
   Portable evidence is in `src/driver/spawn_tests.rs` (1/2/4 workers, ordering,
   rejections, cutoff, equal control access, targeting, snapshots and failure).
   Native `tests/driver_guest_services.rs` verifies an oracle-only boot spawning a
   printing peer and reading the Applied reply, with identical complete events
   and barriers at workers 1 and 4 on both TCG and x86 PV.
   **TODO BEFORE MERGE: re-evaluate the MachineDefinition / spawnable representation.**
7. **Final semantic audit and native acceptance.** Map every discussed,
   non-deferred UI/API operation to code and meaningful evidence. Add native
   positive Sleep/trigger coverage and verify both backends for new capabilities.
   Cover failures, cutoff/readback, ordering, worker count and snapshot continuation.
   Record intentional approved differences, including equal control access for
   all machines and FIFO admission of generated effects. Do not connect the old
   UI/server or call compilation proof of native behavior.
8. **Finish documentation and final checkpoint.** Update rustdoc and lasting
   architecture documentation, resolve the TODOs above and remove this temporary
   handover before merge. The user requested this intermediate commit/push while
   implementation is paused; it does not replace the final completion audit.

## Explicit deferrals and limits

Networking and storage integration/faults, whole-simulation checkpoint/branch,
and implementing a PMU backend are deferred. Host readback, clock faults,
inspection are not deferred; dynamic creation is implemented as described above.

Native evidence is from Apple Silicon/HVF, x86-64 Linux/KVM and TCG. The x86
PV backend passes the existing native PV suites on an AMD KVM host, including
snapshots; Intel VMX hosts are unverified. Linux ARM KVM architectural snapshot
support remains absent/unvalidated; do not claim cross-platform parity.
PV work accounting covers instrumentation charge points, not arbitrary polling
or uninstrumented execution. The execute-only handoff covers configured startup
and same-boot restart, not arbitrary application-initiated exec. RTC epochs, raw
hardware entropy and broader CPU portability remain limitations. Dependency
snapshot consistency remains the caller's responsibility until a simulation-wide
capture boundary is designed.
