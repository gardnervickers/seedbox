# Simulation server plan

Build a local control plane for Seedbox simulations, with the CLI as its first
client. Use a Kubernetes-style resource, describe and watch interface so a TUI
or GUI can later use the same API. The first usable milestone is starting a
simulation and following its application logs and structured events.

## Current implementation status

The seven implementation steps below are implemented in one working branch.
The validation record identifies the native scenarios and component checks used
to verify the contract. Deferred hardening is separate from these capabilities;
the design requirements and completion criteria remain the review contract.

- **Local server and resources.** Separate spec, API, client, server and application
  crates provide a durable simulation catalogue, HTTP/JSON over a Unix socket,
  automatic daemon startup, request identities, worker ownership, status watches,
  lifecycle timings and explicit execution termination. Oracle exit alone determines
  the test verdict. Describe produces a retained-input specification that can be
  resubmitted without rereading changed project files or mutable image tags.
- **CLI migration and seed ranges.** Run, explore, rerun and replay submit through
  the server. Compare reads retained results without execution. Exploration freezes
  one preparation and creates fresh timelines for consecutive seeds; pending members
  survive server restart. Clients can list, describe, stop and attach with
  `get simulation ID --watch`. Disconnecting a client leaves execution owned by the
  server; foreground run interruption sends an explicit stop request.
- **Live output and events.** Guest application stdout/stderr are separate,
  machine-attributed byte streams published from committed engine observations.
  Bounded delivery reports gaps and has no durable stream backlog. Typed events cover
  preparation milestones, lifecycle and oracle changes, machine and fault activity,
  control receipts, execution termination and deletion requests. Event envelopes
  identify the simulation, omit unknown guest time and carry exact control-boundary
  positions when available. Status inspection and watch recover the current resource.
- **Interactive execution.** Pause, resume, rate and step use committed-boundary
  controls with retained acknowledgements and observed playback rates. Deliberate
  pacing waits and pauses are excluded from the active execution wall-time budget;
  preparation has a separate deadline. Inject/heal controls retain semantic fault
  history for rerun and deterministic ordering with scenario actions.
- **Checkpoints and branches.** Linux KVM and Apple Silicon HVF have native world
  capture/restore paths. Capture freezes the intervention prefix; branch creates a
  separate simulation linked to its source checkpoint. Checkpoint list/describe and
  deletion of unreferenced captures from completed simulations are exposed. Simulation
  deletion marks the ownership tree, blocks new descendants, stops active members and
  removes descendants before parents; deletion intent and its timestamp are visible.
  Shared content objects remain until explicit reclamation is implemented.

The [configuration audit](simulation-configuration-audit.md) records authoring
and ownership decisions. Compose rejects removed internal tuning controls and
`check.exit_code`; retained formats preserve inputs required by older runs.
Runtime startup uses available assets and does not invoke builders. Offline
artifact commands remain independent of the daemon.

## Validation evidence and deferred work

Earlier native probes recorded Linux KVM and Apple Silicon HVF checkpoint capture,
branch output, matching source witnesses and simulated end times, fault isolation,
and rerun after server restart. Linux probes also recorded independent fresh-run
comparison and server replay matching, plus seed-range continuation after restart
without the original Compose file. Portable and focused component checks have
been run during implementation. These are evidence for those tested revisions
and scenarios, not full cross-platform acceptance of the final working tree.

Native Linux cascading deletion removed a paused parent, its paused checkpoint
branch, a queued grandchild and their private artifacts; unrelated queued work
completed successfully afterward. The probe stopped its server and removed its
temporary state. Native Linux runs with no log subscriber, a fast subscriber,
a stalled subscriber and multiple subscribers produced matching fresh-run
witnesses and replay results. Stalled readers received explicit delivery gaps.

The Linux `linux` harness group passed its portable gate, native KVM/XSAVE
checks, all application fresh-run comparisons and reruns, and OCI integration.
Successful application runtime state was removed afterward; validation logs were
retained. The Mac harness passed its portable gate, native HVF checks and all
application fresh-run comparisons and reruns. Application acceptance uses a short
private state path to fit macOS Unix socket limits and removes successful runtime
state. The prior `slow-machine` failure exposed missing ARM CPU throttle support;
ARM PV ABI 3 now carries the syscall cost, and the throttle oracle and both
comparison checks pass.

A native Mac describe/resubmit probe saved the source's YAML, replaced its original
Compose file with invalid input, and submitted the YAML as a new simulation.
The retained seed, oracle outcome and witness matched the source. Cascading
deletion removed both resources, and the probe stopped its server and removed
its private state.

The current ARM ABI also passed a native pause/step/checkpoint probe: a branch
retained the captured partition while the source healed it, the paused source
remained independently controllable, and the branch reran after server restart
with matching witness, fault history, oracle exit and simulated end time. Its
server and successful runtime state were cleaned up.

A native Mac crash probe killed one owned worker while another simulation was
paused: the failed simulation became incomplete with an execution error, and
the unrelated simulation resumed and passed. Killing the private server then
terminated all its workers; restart marked the interrupted simulation explicitly
and a fresh simulation passed. The probe removed its successful runtime state.

Committed oracle exit observations now update durable status independently of
the final worker report. A component regression covers zero and nonzero oracle
codes surviving missing-report error finalization and catalogue reopen, while
other machine exits cannot determine the verdict.

The final portable gate passed after the oracle-preservation change, and the
server suite passed on both Mac and Linux. A native Mac pacing probe paused a
simulation and ran it at 1/100 speed, then compared its retained identity and
report with an independently prepared unlimited run: both matched. The paced
simulation exceeded its two-second wall-clock budget only through deliberate
waiting and completed normally. All successful probe state was removed.

Deferred hardening
includes concurrent controls during cascading deletion and interrupted cleanup;
checkpoint deletion while its source is active and cleanup recovery after
server failure; short-run stage timing; and range-dispatch fairness and request-ID
review. New component tests live beside their source; public catalogue, transport
and CLI interactions live in their owning crates' integration directories. The migrated
Linux pause/resume fixture passed through the server API, including matching
oracle outcome, witness and full-state digests against its original simulation.

Offline bundle import still produces offline retained storage; catalogue
registration is not implemented. Shared content-addressed object garbage
collection after explicit deletion is also deferred. Arbitrary backward timeline
navigation remains later work requiring restore-and-replay semantics. Remote hosts,
network authentication, warm workers, prepared-world reuse, TUIs and GUIs remain
subsequent capabilities. None replaces or narrows the requirements and acceptance
gates below.

## Agreed product behavior

- **Simulation:** one run of one seed, with its own timeline and outcome.
- **Simulations:** the collection of simulations, including concurrently running
  simulations. Do not introduce a competing job, execution or session resource.
- **Checkpoint:** a saved position within one simulation timeline.
- Simulation IDs are short and easy to type. Start with eight lowercase,
  unambiguous alphanumeric characters, checking collisions against the server's
  catalogue. These are resource identifiers, not content hashes or credentials.
- Commands needing a server connect to the local server or start one
  automatically. Closing a client does not stop its simulations.
- `describe` produces YAML containing a resubmittable specification, current
  status, and timing context. Completed simulations remain inspectable without
  retaining their running workers or VMs.
- **Logs** mean application stdout and stderr. Simulation IDs and machine names
  are positional: `seedbox logs <sim-id> <machine-name>`.
- A simulation's test verdict comes entirely from its workload oracle's exit
  code: zero means passed; nonzero means failed. Logs, internal events, other
  machine exits and replay comparisons do not determine the verdict. Without
  an oracle exit code, the test result is incomplete, never an inferred pass
  or fail. Report execution errors and interruptions separately.
- **Events** mean typed, structured internal activity. Clients never parse
  diagnostic prose to determine a simulation's state.
- Logs and events are live streams. Do not build a file collector, log archive,
  historical log endpoint or durable stream backlog. Users can pipe output into
  their own storage.
- Rerun uses the original retained inputs and seed and creates a new simulation.
  Running current source changes performs fresh preparation instead.
- Deleting a simulation cascades through its owned descendants, including branches
  and seed-range members. Stop active work, remove checkpoints and private run data,
  then remove the resources. New descendants cannot be created during deletion.
- Later controls include playback rate, pause, boundary stepping, checkpoints,
  and interactive fault injection.

## Start with the configuration audit

Audit configuration before freezing the API. Existing options do not earn a
place in the new CLI or YAML merely because they exist today. Advanced YAML
must not become a dumping ground for removed flags.

Inventory CLI flags, environment variables, Compose extensions, defaults,
prepared-input fields and worker settings. For each, record the actual consumer,
whether it changes results, its demonstrated user need, and the proposed action:

| Action | Rule |
| --- | --- |
| Keep configurable | The user needs to express a different experiment or interaction. |
| Choose a default | Pick one documented behavior until a concrete use case earns configurability. |
| Discover automatically | Resolve host resources and installation paths where practical. |
| Remove | Eliminate redundant, obsolete or implementation-specific choices. |

Initial candidates to examine, rather than decisions to preserve every knob:

| Current category | Proposed treatment to evaluate |
| --- | --- |
| Machines, seed, application inputs, meaningful network/fault behavior | Simulation specification. |
| CPU selection and execution concurrency | Server resource allocation with automatic defaults. |
| Kernel, VMM, helpers, container connection and state paths | Server discovery/configuration; ordinary submissions should not specify installation plumbing. |
| Watchdog and scheduler tuning | Implementation defaults unless a demonstrated operational need remains. |
| Full-state evidence and retention switches | Choose a coherent default after measuring cost and checking reproducibility requirements. |
| Checkpoint/branch flags | Later resource controls, rather than copying paired CLI flags into the initial spec. |
| Simulated-time and wall-time limits | Separate experiment termination from operational timeout policy. |
| Formatting, log filters and attachment behavior | Client options. |
| Playback rate | Live control, not a result-changing simulation input. |

Distinguish fields retained internally for reproduction from knobs exposed to
users. Retaining a resolved value does not require accepting user overrides for
it. Expose enough effective configuration through describe to explain a run;
keep implementation details out of the authoring surface.

Audit deliverables are a migration table, the minimal authoring spec, documented
default rationale, and affected tests/docs. Resolve how deliberate pacing and
pauses interact with wall-time limits before implementing time controls.

## Resource and API contract

Use `metadata`, `spec` and `status` as the public resource shape. Compose is an
authoring input that resolves into this spec, not a second execution model.
Freeze the effective spec before guest execution. Status changes independently.

Illustrative shape; the audit determines the final fields:

```yaml
apiVersion: seedbox/v1
kind: Simulation
metadata:
  id: k7m2p9qx
spec:
  seed: 196
  inputs:
    prepared: example-retained-input-reference
status:
  phase: running
  stage: executing
  simulatedTimeNs: 1480000000
  stageTimings:
    preparing:
      elapsedWallNs: 820000000
      complete: true
    booting:
      elapsedWallNs: 560000000
      complete: true
    executing:
      elapsedWallNs: 2100000000
      complete: false
```

`seedbox describe simulation ID` emits YAML by default. Submission of that YAML
consumes `spec`, allocates a new ID and ignores server-owned metadata/status.
Reject unknown spec fields instead of silently dropping them. Distinguish an
unresolved submission still being prepared from an executable resolved spec.

Describe must carry the input references and effective choices needed to
resubmit without consulting changed files or mutable image tags. It is not a
self-contained bundle: referenced inputs must remain available, and missing
inputs must produce an actionable error. Reuse the existing retained-input and
engine contract. Do not add source-revision pins or qualification gates.

Keep lifecycle, execution termination and test verdict separate. Proposed phases
are queued, preparing, running, finalizing and completed; execution errors,
cancellation, timeouts and interruption have explicit termination reasons.
Status exposes the workload oracle's exit code and the verdict derived solely
from it. An absent exit code leaves the verdict pending while running and
incomplete when execution ends. Identify the workload oracle explicitly in the
resolved specification; do not aggregate arbitrary machine exit codes. Preserve
an observed oracle verdict even if subsequent cleanup fails, reporting that
failure separately. Booting/executing are stages with timing information, not
new verdicts. Report simulated time separately from host elapsed time. Use
monotonic elapsed durations, and do not present overlapping stage durations as
an additive total. In-flight timings include the current elapsed duration.

The initial API supports submit, list, describe, watch, stop, application-log
streaming and event streaming. Watch is a snapshot plus subsequent resource
changes with revisions, so connecting cannot silently miss a status transition.
A reconnect obtains a new snapshot; it does not imply recovery of missed logs.

Use HTTP/JSON over a local Unix socket, with server-sent events for structured
streams as the initial transport proposal. Carry application bytes losslessly
inside stream envelopes; do not assume UTF-8 or newline-delimited writes. A
future browser client will need an authenticated HTTP endpoint or desktop
bridge. Remote access is outside the first milestone.

Use stable typed errors and request IDs. Retrying a submission or control request
must not create duplicate simulations or apply an action twice. Define the
request-ID lifetime and persist accepted submissions with their resource IDs.

## Server startup and lifetime

Commands that require the server first connect to its user-owned local socket.
On absence, coordinate through a startup lock, recheck, start one background
server, and wait for readiness with a bounded timeout. Concurrent clients must
not start competing servers. Handle stale sockets without deleting a live
server's endpoint. Connection denial, startup failure and an incompatible API
are distinct from an absent server; do not silently replace an existing server.

Keep `seedbox serve` for foreground use and service managers, but never require
it for ordinary local use. Help and purely offline commands need not start a
server. An explicitly configured remote endpoint must not fall back to starting
a local server.

Use the same installed Seedbox executable where practical. Starting the server
must never invoke Nix, Cargo or guest builders. Move host runtime-input resolution
to the server executable boundary; libraries receive explicit paths and clients.

The server owns the simulation catalogue, preparation, resource allocation,
isolated worker processes, outcomes and retained inputs. Clients own rendering.
Do not give clients ownership of guest process lifetimes. Host scheduling and
external affinity remain outside Seedbox; no CPU leases or binding options exist.

Build on `orchestration.rs`, `execution.rs` and `worker_process.rs`. Keep
`seedbox-artifacts` usable offline and free of server/hypervisor dependencies.
The existing log reader validates finalized artifacts; it is not a live-stream
reader. The existing host pause/resume path and shared coordinator are starting
points for boundary controls, not reasons to introduce another scheduler.

Persist enough metadata to describe completed simulations and support rerun;
this does not imply retaining their logs/events. A crashed worker fails its
simulation without terminating other simulations. On server restart, identify
and clean up owned workers safely and mark interrupted simulations explicitly.
Do not promise recovery of a running VM until checkpoint recovery is implemented.
Discard poisoned worker/world state after a failed transaction; a later request
must not reuse it as though it were a clean starting point.
Catalogue/input deletion must be explicit, with references from reruns and
checkpoints accounted for.

## Application logs and structured events

Application output needs an explicit guest-to-host path that distinguishes
stdout from stderr and attributes bytes to the machine and its incarnation.
Today `guest/compose-init.c` sends both descriptors to the console. Relabelling
that mixed console stream cannot recover the distinction. Supervisor messages
and kernel console diagnostics must not masquerade as application logs.

Publish output only after the engine commits it. Reuse the existing simulated
commit-time and machine-order concepts, adding the application stream identity.
Label timestamps as commitment times, not application-provided timestamps.
Preserve partial lines and non-text bytes. Combined-machine ordering must be
defined from committed state, not worker completion or socket arrival order.

Events have a type and a typed payload: simulation/stage changes, preparation
progress, machine lifecycle changes, faults requested/applied/rejected/healed,
control acknowledgements and errors. Later types cover boundary advances and
checkpoint operations. Include simulation ID, relevant machine identity and
simulation position when one exists. Preparation has no guest timestamp.
Human-readable messages can supplement structured fields, not replace them.

Both streams use bounded buffers. No subscriber, a slow subscriber or a dropped
connection must change guest behavior or block the coordinator on client I/O.
Report gaps explicitly, including the affected stream/sequence range when known.
Reconnects recover current status, not a promised backlog. Gracefully finishing
a stream delivers its terminal indication; other clients can inspect the final
resource even if they missed that indication.

Do not write logs or events to files on behalf of clients. Audit existing console
retention separately so the migration does not accidentally remove evidence
needed by existing readers. Keep simulation input/outcome retention distinct
from optional user-owned log collection.

## CLI as the first client

Prefer positional arguments for the objects a command acts on: simulation first,
then machine where relevant. Flags select modifiers such as status watching
or output format. Logs and events follow live streams by default; omitting the
machine selects all machines.

Current command shape (the requirements below remain the plan):

```sh
seedbox run ./compose.yaml
seedbox get simulations
seedbox get simulation k7m2p9qx --watch
seedbox describe simulation k7m2p9qx
seedbox logs k7m2p9qx database
seedbox events k7m2p9qx
seedbox stop k7m2p9qx
seedbox describe simulation k7m2p9qx > simulation.yaml
seedbox run simulation.yaml
```

Run submits, attaches and waits by default; detached submission returns the
short ID. Disconnecting leaves the simulation running. Foreground Ctrl-C sends
an explicit stop request rather than relying on parent-process teardown.
The API exposes stop-requested separately from stop-completed.

Derive simulation PASS/FAIL solely from the workload oracle's exit code, keeping
the raw oracle code available in structured status. Map that verdict to the
documented CLI exit codes rather than forwarding arbitrary oracle codes as CLI
codes. Keep setup errors, incomplete execution, timeout and replay-comparison
results distinct; document precedence when an execution error accompanies an
already observed verdict without changing that verdict. Audit text-output migration
explicitly: scripts currently consume a retained run-directory status line, so
replacing it with an ID requires a documented transition. Preserve `--json` as
one JSON document; use an explicit streaming format for JSON-lines events.
Application-log rendering and structured event rendering are separate commands.

Rerun creates a new simulation linked to its original, using retained inputs,
seed and engine. Compare results when the required evidence is available. A
fresh run from the current Compose project re-prepares inputs. Neither action
overwrites the original. Catalogue retention makes rerun possible; stored log
streams are not required.

## Boundaries for later interactive controls

Keep resource revision, stream delivery sequence and simulation timeline position
distinct. They answer different questions and must not be interchangeable IDs.

Worker control belongs at committed scheduler boundaries. A command is first
accepted, then applied or rejected; an acknowledgement identifies the boundary
where it took effect. Reading or subscribing cannot advance simulated time or
consume RNG state. The host API remains separate from guest-facing scenario
control, with an explicit deterministic ordering when both act at a boundary.

Playback rate generalizes pause. Zero is paused, positive rates target simulated
seconds per wall-clock second, and unlimited runs as fast as possible. Pace by
waiting between boundaries, never by changing guest clocks or scheduler choices.
Report requested and achieved rates. Reset the pacing reference after pauses or
rate changes instead of attempting a burst of catch-up work. Step advances one
committed scheduler boundary and remains paused; it is not CPU instruction
stepping. Backend differences and control latency must be reported honestly.

Interactive faults are applied at a committed position, with machine/connection
targets validated there. Host click time is not reproducible. Retain applied
faults, parameters and positions as semantic replay input, including ordering
relative to scenario faults. This bounded control history is distinct from
persisting the observational event stream. Rerun must reproduce applied actions,
not request arrival times.

A checkpoint captures a consistent world: CPU/RAM, disks, devices, pending work,
coordinator state and RNG streams. Existing backend support is uneven; do not
advertise generic restore before it exists. Branching creates a new simulation
linked to the source checkpoint. Preserve the original timeline and prevent
sequence reuse from making output from separate branches indistinguishable.
Arbitrary backward navigation would restore a checkpoint and replay forward;
historical application logs are not implied by that feature.

## Implementation sequence

1. **Configuration audit and contract.** Produce the option migration table and
   minimal spec. Define resource IDs, lifecycle, errors, describe/resubmit,
   stream envelopes, control acknowledgements and retention boundaries. Reconcile
   active exploration/fork work before choosing implementation bases.
2. **Server and first CLI path.** Implement catalogue, submit/list/describe/stop,
   worker ownership and automatic startup. Route CLI run through the server.
   Persist accepted submissions and final results; test restart/interruption.
3. **Application logs and events.** Separate guest stdout/stderr, add committed
   publication, bounded stream delivery, machine filtering and status watch.
   Ship the first usable milestone: run, describe, follow application output,
   follow structured events, disconnect and attach from another client.
4. **Complete the CLI migration and rerun.** Move remaining applicable commands
   behind the same client interface; keep offline artifact operations offline.
   Add describe/resubmit and rerun acceptance, script migration docs and examples.
   Update README, runtime/CLI docs, generated project docs and AGENTS boundaries.
5. **Playback and stepping.** Integrate boundary acknowledgements, pause/rate/
   resume/step, observed rate and a defined wall-time timeout policy. Prove
   invariance across pacing rates and client behavior.
6. **Interactive faults.** Add inject/heal controls, deterministic ordering with
   guest scenario control and retained semantic control history. Test replay of
   a simulation that was changed interactively.
7. **Checkpoints and branching.** Expose real backend checkpoint capabilities,
   consistent capture/restore and parent links. Add timeline navigation only
   once its restore/replay semantics are validated.

Start with public API types and a transport client separated from engine and
artifact crates. Reuse application orchestration for the server implementation.
Choose its package placement without creating a cycle: a server crate depending
on the application library cannot also be a dependency of that library's CLI.
If extraction requires it, move the binary/CLI into a thin package while retaining
one installed `seedbox` executable and its private worker dispatch. Avoid a broad
crate reshuffle before the first vertical slice demonstrates the boundary.

Warm workers, prepared-world reuse, remote hosts, authentication for network
access, TUIs and GUIs are subsequent capabilities. They must use the same API;
they are not prerequisites for application logs and control-plane separation.

## Validation and completion criteria

Unit tests live beside the components they exercise. Integration tests live in
the owning crate's `tests/` and cover public interactions; give each scenario one
owner rather than duplicating it across layers. Compile hardware tests in the
portable gate, then execute relevant Linux KVM and Mac HVF suites.

The initial milestone is complete when:

- Two simultaneous CLI invocations start only one server, and startup errors,
  stale sockets and existing-server incompatibility have clear behavior.
- One client starts a simulation and another can describe, watch and stop it.
- Short IDs remain unique under concurrent submission and retried requests.
- Describe round-trips into a new simulation with the same resolved experiment,
  without incorporating old status or silently using changed inputs.
- Application stdout/stderr are distinguishable and machine-attributed, including
  partial UTF-8, arbitrary bytes, simultaneous output and machine restarts.
- Events are typed, and status recovery works even after missed event delivery.
- Oracle exit zero passes and nonzero fails regardless of application log text,
  internal events, other machine exits or replay-comparison results. Missing
  oracle exits produce incomplete results on termination; cleanup failures do
  not overwrite an observed verdict. CLI mapping preserves the raw oracle code
  in structured status and keeps execution errors separate.
- No/fast/slow/multiple subscribers produce equal fresh-run witnesses and replay
  results. Buffer overflow is visible and cannot stall simulation execution.
- Worker/server crashes, stop during preparation, and stop during execution leave
  correct outcomes and release owned resources without harming other simulations.
- No new automatic log/event files or durable streaming archive are introduced.
- Runtime startup uses available assets and never invokes build tooling.

Later gates add pacing invariance, exact boundary-step behavior, replay of applied
interactive faults, and checkpoint branch isolation. Do not use successful replay
alone as evidence of independent fresh-run determinism.
