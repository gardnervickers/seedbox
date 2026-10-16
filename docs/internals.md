# Technical details

These mechanisms depend on ordering or state that is easy to overlook.
Each section links to the code that owns it.

## Bounded virtual clock reads

Reading the host counter would make a guest polling loop's iteration count
depend on host scheduling. Kernel clock reads use a
shared PV page. Repeated reads charge 100 ns on both architectures. A read that
would pass the published bound yields to the VMM before returning that time. This
bound is the latest time the guest may reach before the VMM must handle a
pending event or scheduling boundary. The VMM processes that boundary and
publishes a new bound.

The application-facing clock paths must use the same source. On x86, the
instructions that read the CPU's timestamp counter (`RDTSC` and `RDTSCP`) are
configured to raise an exception in application code. The kernel handles that
exception by returning virtual time. ARM counter reads are redirected similarly.
Linux also maps clock helper code into applications to avoid syscalls; this is
the vDSO. The ARM clock helpers deliberately fall back to syscalls so they
cannot bypass the controlled clock. Calibration must agree with the virtual
counter frequency rather than measure the host.

A guest must never observe time beyond the boundary the VMM has permitted.

Code: [x86 clock](../kernel/seedbox.c), [ARM clock](../kernel-arm64/seedbox.c).

## Progress from translated blocks

A computation or spin loop that never reads the clock still needs to let other
guest threads and machines make progress. The DynamoRIO client counts the
instructions each thread executes. Every 50,000 instructions it issues a raw
`getpid` syscall with a fixed first argument, which marks it as a progress
call. Existing executable code, shared libraries and JIT code go through the
translator without application-specific flags or recompilation.

The count is taken per basic block: a sequence of instructions with one entry
and no internal control-flow branch. Each block is charged its length at its
entry, so a tight loop and straight-line code advance time at the same rate
per instruction. A charge can cross the budget; the remainder carries into the
next window. Translator-internal work and kernel execution are outside the
count.

The kernel recognizes the mark and charges the call the time those
instructions take on that machine's virtual CPU. Each machine and the oracle
has its own `cpu.speed` and `cpu.jitter` in `seedbox.yaml`. Speed defaults to
1 GHz and jitter to zero, so a call nominally costs 50 µs by default. The
instructions per call is a constant of the instrumentation, carried in the
launch recipe so the guest and the simulator agree on what a call stands for.
Saved definitions retain these settings on each machine.

```yaml
machines:
  worker:
    image: example/worker
    cpu: {speed: 800MHz, jitter: 5%}
oracle:
  image: example/oracle # Defaults to 1 GHz with zero jitter.
```

Equal work interleaves the same way on every seed. `cpu.jitter`, a percentage,
makes each call cost a seeded amount more or less than nominal, within that
percentage. The kernel draws it from a stream the VMM seeds per machine on the
time page. A run replays exactly, and a reboot or a time fork reseeds the
stream with the others. The client itself draws nothing.

On x86 the charge is counted in whole syscall costs, so the count-based plan
stays linear. The remainder carries to the next call, and a count the hard
stop cuts off stays owed to the next run. On ARM the charge advances the
bounded clock in bulk, with the same rendezvous at every bound as single clock
reads. A throttle fault scales the compute charge like every syscall's. Both
backends start with a fixed 1,000 ns syscall charge. There is no implicit per-machine
speed draw at boot or a time fork; only authored progress jitter consumes its
own RNG stream. Snapshots retain charges and jitter continuation, and reboot
reapplies active throttle faults.

Instrumentation must leave the application's registers and comparison flags
intact. On ARM, the client uses arithmetic that leaves those flags alone and a
branch that tests the sign bit directly (`TBZ`). DynamoRIO's drreg extension
manages the temporary registers.

Guest signals arrive only at progress calls, so when the ARM translator
synchronizes all threads, for example to flush code a JIT has rewritten, it
always finds a busy thread inside the progress call. The translator cannot
translate a thread's state there. If the thread returned into translated code,
the request would retry forever while every other thread stayed suspended. The
ARM progress call therefore leaves through the translator's dispatcher at the
charged instruction, which waits for a pending synchronization. The charge
there runs again, so the call gives it back first. The translator patch saves
each vector register of the call in its own context slot, so that exit
restores them exactly.

The ARM translator detects rewritten code through the application's `IC IVAU`
cache-maintenance instructions (`-no_hw_cache_consistency`). Translated reads of
`CTR_EL0` clear DIC (`-fake_ctr_dic`), so applications must issue `IC IVAU` even
when the underlying CPU would permit omitting it. DynamoRIO exposes this internal
option in release builds. Enabled `hw_cache_consistency` instead uses code-page
write protection and self-modifying-code sandboxing. In the observed Kafka
startup, that mode grew a broker's memory to about 2 GB and it was killed;
with application-driven invalidation, it used about 250 MB and kept starting.

Code: [progress client](../guest/progress/progress.c),
[x86 syscall charge](../kernel/seedbox.c), [ARM syscall charge](../kernel-arm64/seedbox.c),
[vector context patch](../guest/progress/dynamorio-cleancall-simd.patch).

On x86 a charge can span several scheduling boundaries. Its unpaid time is
retained with the machine and drained while the vCPU is stopped, before guest
execution resumes. Timers can become pending during that interval; checkpoints
preserve the remainder.

The TUI shows each machine's configured nominal speed by default. While
throttled it shows the effective nominal speed after backend rounding and
clamping; healing restores that machine's configured speed. The fault's
recorded input keeps the original request.

## Progress inside kernel polling

An x86 kernel `cpu_relax()` loop may hold a lock with interrupts disabled. It
cannot simply call the scheduler to let time advance. `PAUSE` is the CPU's hint
that code is spinning; it does not itself advance Seedbox time. Seedbox adds a
bounded PV clock read after that hint, allowing the guest CPU to stop at a time
bound without allocating, acquiring another lock, scheduling or enabling
interrupts. Interrupt delivery stays deferred until the guest permits it.

Each hook charges a 100 ns read and is inactive before PV activation.
A read that would cross the published bound yields instead. With interrupts
masked it yields only for the bound, so the VMM advances to the bound even
while an interrupt waits for delivery; otherwise the read would yield at the
same time forever.
This covers the patched kernel `cpu_relax`/`rep_nop` paths. It does not cover
arbitrary assembly `PAUSE`/`YIELD`, early decompression or the vDSO. ARM
`cpu_relax()` uses the same bounded read. Its raw exclusive-store retry loops
remain uncharged: host interruptions can cause those retries and must not
advance guest time or the software reservation epoch. Application polling is
handled by binary instrumentation.

Code: [x86 clock](../kernel/seedbox.c), [x86 patch sites](../kernel/apply.py),
[ARM clock](../kernel-arm64/seedbox.c), [ARM patch sites](../kernel-arm64/apply.py).

## Observation without guest progress

A host watchdog interrupts a long run so Seedbox can diagnose where it is stuck.
Treating that interruption as a scheduling boundary would make the watchdog
interval part of the simulation. The watchdog instead resumes the existing guest
transaction: it must not adopt guest time, publish a new plan, draw timer
randomness, deliver a guest interrupt, or advance the ARM reservation epoch
described below.

The same distinction applies to backend maintenance exits, such as the ARM
debug-register unlock bypass. An exit visible to the hypervisor is not
automatically an event that belongs in the simulated world.

Code: [x86 PV execution](../crates/seedbox-vmm/src/machine/pv.rs), [ARM machine](../crates/seedbox-arm64/src/machine.rs).

## Software exclusive reservations

ARM load-exclusive/store-exclusive instructions implement atomic updates. A host
interruption can clear the hardware reservation and cause a store to fail even
when no guest thread changed the value. Applications can observe that failure.

The translator retains a software reservation: address, value, size, and guest
epoch. A store checks the reservation and retries physical monitor loss internally.
Guest boundaries invalidate reservations by advancing the epoch; host interruptions
do not. Comparing values alone misses a value that changed and changed back.

The epoch check sits inside the physical exclusive pair. A guest interruption
after the check clears the physical monitor; the retry then sees the new epoch.
The kernel publishes the epoch on return to application mode, and the stopped
VMM advances it before guest device delivery. This assumes one guest CPU.

Faults must reconstruct the original application registers and instruction
pointer. Internal retries cannot count as application progress. Application wait
hints are rewritten so host wakeups cannot change charged loop counts.

Code: [translator patch](../guest/progress/dynamorio-exclusives.patch), [ARM kernel patches](../kernel-arm64/apply.py), [assembly fixtures](../guest/progress/fixtures/).

## Interrupt delivery at an idle rendezvous

Entering ARM idle with interrupts masked and making an interrupt pending there
lets delivery slide past a later unmask instruction. The saved instruction
pointer can then differ in kernel stack data and entropy inputs, even if the
eventual application output looks identical.

The patched idle path unmasks interrupts before reaching the PV idle exit, when
the VMM has not yet asserted the interrupt for that rendezvous. It can assert
the line at the stopped boundary. The kernel restores the caller's mask on
return. Both ARM backends use this path instead of exposing KVM's handling of
`WFI`, ARM's wait-for-interrupt instruction.

Code: [ARM kernel](../kernel-arm64/seedbox.c), [deferred interrupt test](../crates/seedbox-arm64/tests/review_deferred_bound.rs).

## Canonical saved vector state

x86's `XSAVE` instruction saves extended CPU state to memory, including vector
registers. The YMM component holds the upper halves of the 256-bit AVX
registers. A component marked absent restores its architectural initial value,
which for YMM is zero. Two save buffers can therefore restore the same all-zero
YMM state while differing in the component-present bit or ignored payload bytes.
Hardware's choice can vary across host interruptions and produce a full-RAM
mismatch.

After successful kernel task-state saves, Seedbox zeroes the payload of an
absent YMM component and marks a present all-zero YMM component absent. Nonzero
values and other components remain unchanged. The guest itself writes one
representation; full-state comparison continues to inspect the actual bytes.

The live vCPU digest applies the same YMM normalization to a local copy of
`KVM_GET_XSAVE`'s standard-format buffer. Host FPU save/restore can report zero
YMM state as present or absent even when guest RAM, register values and execution
agree. Nonzero YMM values, other components and all guest RAM still participate
in full-state comparison. Reading the digest never writes the vCPU; checkpoints
continue to save and restore the raw KVM state.

This does not cover direct application XSAVE or every signal-frame save path.

Code: [normalizer](../crates/seedbox-vmm/src/xsave.rs), [KVM restore tests](../crates/seedbox-vmm/tests/xsave.rs).

## Independent random streams

One shared random-number stream would couple unrelated operations: an extra
network draw could change a later disk fault. The engine derives streams from a
seed and named domain, then draws using a stream-local counter. Checkpoint
branches also incorporate their branch identity.

Isolation between domains does not make draw order within a domain irrelevant.
Changing which operations draw, or changing their order, can still change
results. Random policy is owned by the coordinator rather than whichever worker
happens to finish first.

Code: [RNG](../crates/seedbox-engine-core/src/rng.rs), [link policy](../crates/seedbox-engine-core/src/links.rs).

## Parallel scheduling

The scheduler executes machines in rounds. The network's minimum delivery delay
bounds a round so a packet produced within it cannot affect another machine
until the round commits. Each machine receives an immutable assignment with
its wake time, pending inputs, and stop boundary.

After all workers finish, the coordinator sorts outputs by send time, machine,
and machine-local sequence before routing them. A failure prevents the round
from committing. Worker completion order must not change results. An idle
machine must advance to its assigned wake time before delivering inputs.

Code: [round contract](../crates/seedbox-engine-core/src/window/mod.rs),
[world adapter](../crates/seedbox-engine-core/src/world_windowed.rs).

## Fault ordering and replay

An external override has a stable key, simulated time, and action. Its execution
fault ID is assigned at admission; future overrides do not reserve IDs. Same-time
entries follow document order, and one entry's due effects are processed before
the next entry is admitted. Guest execution is not inserted between those entries.

Interactive admission records its position relative to already processed oracle
actions, so replay cannot move the override ahead of them. Reusing a key with
different contents is rejected. A state-dependent rejection advances the program
cursor and records the failure. An oracle exit prevents admission of the
unapplied suffix.

Removing an earlier fault does not automatically remove a later heal. The later
action is evaluated against the world when it becomes due. Recorded oracle events
remain observations; replay executes the oracle rather than injecting its events
again.

Code: [input program](../crates/seedbox-engine-core/src/input_program.rs),
[coordinator](../crates/seedbox-engine-core/src/world.rs).

A fault profile (`--fault-profile`) is a request option, never part of a
definition. The server samples it once, after choosing the seed, into keyed
`gen-<n>` and `gen-<n>-end` program inputs, so describe, rerun and replay carry
the generated faults and never sample again. Each seed-range member samples
the profile with its own seed. Sampling uses integer arithmetic only, keeps
lifecycle and clock faults from overlapping on one machine, and never targets
the oracle. Code: [sampler](../crates/seedbox-explore/src/faults.rs).

The engine gates generated inputs by their reserved key namespace. A generated
start due before every initial application is ready, or before the end of a
quiet period, is skipped and recorded as a rejection whose reason begins with
`skipped`; its `gen-<n>-end` teardown is skipped with it. Authored inputs and
teardowns of admitted starts always run. The oracle's `quiet` control
operation extends the quiet period from its admission time, so a workload can
let its system recover before checking it; generated faults already active
still end on schedule. Code: [gate](../crates/seedbox-engine-core/src/input_program.rs).

## Property records

Guest properties travel the same committed path as application output. Every
machine's supervisor serves `/run/seedbox/sdk.sock`; each `SOCK_SEQPACKET`
connection is a session, and each message is relayed unparsed as
`SBX_PROP1 <session> <seq> <hex>`. Session 0 is the supervisor's own: lost
records and the main process's exit. The management decoder retains every
property line, whether or not anything observes the console, and the
coordinator reduces them in its fixed absorb order (time, machine, channel
order). The engine attributes each record to its machine and incarnation and
stamps it with the commit time; guest-supplied times are never used.

The reducer is pure and shared with offline reporting. It never fails: a
malformed record, a sequence gap, an exceeded bound, a reserved or conflicting
declaration all become evidence errors, and an assessment with evidence
errors is at best incomplete. A definite violation stays visible regardless.
Checkpoints carry the ledger, so a continuation keeps earlier records and
sequencing. Reports include the ledger, and the world witness hashes it, only
when a run produced records.

A flush reply means the supervisor has written every earlier record from that
connection to the management port. Records sent before the main process exits
are relayed before its exit record, but descendants still writing are not
waited for, and a machine crash discards what has not crossed the port.

Code: [reducer](../crates/seedbox-engine-protocol/src/property.rs),
[commit](../crates/seedbox-engine-core/src/properties.rs),
[supervisor](../guest/compose-init.c), [SDK](../crates/seedbox-sdk/src/lib.rs).

## Fault discovery and previews

The server owns categories, parameter schemas and effect explanations.
`POST /v1/simulations/{id}/fault-preview` accepts an `action` and returns expected
machines, directed links, effects, preconditions and observations at a world
revision and simulated time. The shape compiler selects targets without preparing
an admission, sampling random streams or assigning a fault ID. Heal resolves the
original active handle, including policies hidden beneath later fault layers.

Preview is observational: process effects still need guest acknowledgement,
connection incarnations may disappear, and lifecycle state may change before
submission. Unknown application state stays explicit. Clients must submit through
the ordinary control path for admission. The TUI discards obsolete form previews,
shows server targets on the map, and requires Enter after the current preview is
available; opening and closing the menu retain the playback pause/restore flow.

Code: [preview API](../crates/seedbox-api/src/fault_catalogue.rs),
[server effects](../crates/seedbox-server/src/fault_preview.rs),
[target compilation](../crates/seedbox-engine-core/src/faults.rs).

## Checkpoints and lifecycle

World observations expose machine and application conditions separately.
Confirmed process effects update the application condition at their effect time;
whole-machine pause preserves it. Resume restores the machine's pre-pause
condition, including startup. Crash makes the application unavailable, and reboot
waits for a new readiness observation. Application exits retain their exit code.
Persistent stop and pause effects remain active until reversed or ended by a
lifecycle transition.

A checkpoint captures the world and its input-program cursor together. Restoring
it must neither repeat consumed overrides nor lose pending ones. Capture requires
a committed boundary with no outstanding I/O cost. A requested checkpoint may
therefore need another step before it becomes available.

Machine lifecycle updates must occur at their defined commit points around
fallible backend operations. A failed crash transaction poisons the world and
terminates its workers; a retry cannot continue using partially changed state.
Worker responses are checked for identity, generation, sequence, monotonic time,
and stop bounds before their effects are published.

Restoring a snapshot restores guest random-generator state too. A branch that
changes entropy must update the guest's generator as well as the host's streams.
The x86 PV page carries an entropy generation consumed by kernel hooks.

Code: [world](../crates/seedbox-engine-core/src/world.rs),
[ARM checkpoint adapter](../crates/seedbox-arm64/src/application/world/checkpoint.rs),
[x86 entropy hooks](../kernel/apply.py).

## Device completion and disk state

Virtio devices mutate guest RAM as well as host state. Payload, status, and
used-ring writes must retain their order, and the x86 dirty journal must include
every write. A malformed later request does not undo earlier completed requests.
Interrupt and queue-cursor timing on error therefore affects observable state.
Both adapters use the same modern descriptor and register contracts. Block
headers may span readable descriptors; the last writable byte is status. A
flush ignores extra data, and transfers with invalid lengths return IOERR.
Console data waits for the multiport open handshake, with a 64 KiB limit per
chain rather than per notification. Network offloads that were not advertised
are rejected, received frames are limited to 64 KiB, and an undersized receive
chain retains the frame without writing a truncated payload.

A `disk_error` fault fails matching read, write or flush requests with IOERR
before any disk effect: a failed write changes nothing, a failed flush makes
nothing durable, and a failed read writes no data. Each fault draws on its own
`disk-error/<machine>/<incarnation>/<id>` stream, which checkpoints carry with
the device; it survives a guest device reset and is reapplied to a rebooted
incarnation. The heal row records how many requests it matched and failed. A
supervisor that cannot read, format or mount a failing retained disk exits 125,
like a container runtime, rather than reporting an infrastructure failure.

Each completed request publishes its interrupt state before the next request
is validated. Earlier payload, status, used-ring writes and backlog removals
survive a later failure; the failing request is not completed. MMIO rejects
invalid selectors, sizes and ready values, protects ready queues from edits,
and preserves cursors on repeated ready writes. Driver readiness requires
VERSION_1 and a subset of advertised features; the control console also requires
MULTIPORT because its protocol uses a named port and the open handshake.

Adapter identities still select retained accounting and x86 network
notification optimizations (EVENT_IDX, notification suppression and deferred
TX). Those negotiated extensions can suppress an interrupt; they do not
change request validation or completed-prefix mutation rules.

The disk model separates stable and volatile writes. Flush and FUA affect what
is stable; a crash applies the configured survival policy to volatile state.
Restarting an application process is different from crashing its machine and disk.

Code: [block](../crates/seedbox-engine-core/src/virtio_block.rs),
[console](../crates/seedbox-engine-core/src/virtio_console.rs),
[network](../crates/seedbox-engine-core/src/virtio_network.rs),
[disk](../crates/seedbox-engine-core/src/disk.rs).

## Connection-scoped faults

Connection selection tracks observed IPv4 TCP sends, including packets later
dropped. It is not the guest kernel's TCP state. An opening SYN establishes an
incarnation; tuple reuse can create a new one. A fault binds to that incarnation
and cannot silently attach to its replacement. Already routed frames retain
their previous delivery decisions.

Unsupported traffic, including IPv6 and fragmented IPv4, bypasses connection
selection and still follows ordinary link policy. Arming a fault does not prove
it dropped traffic: check its dropped-packet count. Healing one handle does not
remove overlapping faults owned by other handles.

Code: [classifier](../crates/seedbox-engine-core/src/connections.rs),
[guest API](../python/seedbox/).

## Retained inputs

A resolved definition retains the initial wall-clock epoch. The default derives
from image creation times, with a minimum of 86400 seconds. Sampling the host
clock for each run would make otherwise identical fresh runs differ.
The guest supervisor initializes wall time to that epoch plus elapsed world time
on every boot. It reads the kernel-emulated PV counter because a rebooted
kernel's monotonic clock begins again at boot. An explicit clock fault may
change wall time independently.

Kernels, engines, prepared boot archives, and checkpoint binaries are identified
by content digest in the persistent server cache. Metadata, logs, and execution
scratch data belong to the server session. YAML references the binary assets;
it does not contain them. Missing assets cannot be replaced by pulling the current
value of an image tag without changing the experiment.

Code: [definition types](../crates/seedbox-spec/src/definition.rs),
[image preparation](../crates/seedbox-application/src/oci.rs),
[server](../crates/seedbox-server/src/).

## Development checks

Run from the checkout root:

```sh
nix develop
bash harness/check.sh             # Portable tests.
bash harness/check.sh mac         # Apple Silicon hardware and applications.
bash harness/check.sh linux       # Linux hardware and applications.
bash harness/check.sh --help      # Individual suites and requirements.
```

Portable checks run before hardware suites and compile hardware test features.
The runner reports missing host requirements before building. Reports are written
under `target/check-reports/`; they are local output, not documentation.
The VMM and ARM backends have separate Cargo workspaces and lockfiles; use the
root Cargo aliases when building them directly. ARM guest fixtures read
`SEEDBOX_ARM64_KERNEL` and `SEEDBOX_ARM64_INITRD`; `harness/check.py` defines
their checkout defaults and the other suite requirements.

Unit tests live beside their component under `src/`. Integration tests live in
the owning crate's `tests/` and exercise public boundaries. Give each scenario
one owner. Check determinism with independent fresh runs as well as replay;
use identical resolved inputs, including the initial clock.

Code: [test runner](../harness/check.py), [Cargo aliases](../.cargo/config.toml).

## Portable simulation archives

The server captures a completed resolved resource under the same evidence lock
used by deletion and comparison. Export spools a binary tar before streaming it;
the client spools downloads and atomically links the completed file into a pinned
parent directory without replacing files or symlinks. Import spools the upload,
validates a strict manifest and digest closure in an isolated store, then publishes
a fresh completed catalogue resource without admitting execution. No archive name
or original attempt path is followed as a host path. The sole format is
`seedbox-simulation-v1`; obsolete formats have no compatibility reader.

Export staging uses a pinned private directory; ownership, permissions and absence
of extended ACLs must be verified before payload creation. Cleanup unlinks only a
successfully created exclusive payload. Automatic directory removal is limited to
current-user-owned output parents with no group/other write bits and proven absent
ACLs, excluding same-UID/root mutation from the threat boundary. An identity check
avoids already-substituted names; it is not an atomic stat/remove guarantee.
Shared or unprovable parents retain an empty staging directory after payload cleanup
and emit a lowercase stderr warning for manual inspection/removal. Rejected or
unopenable staging directories never receive destructive cleanup. Unsupported ACL
inspection is conservative; inherited staging ACLs are rejected, never modified.

The HTTP/client wire bound defaults to 64 GiB, including tar headers, padding and
end blocks. Manifest metadata is bounded (16 MiB); large assets and diagnostic
payloads use bounded buffers and disk spools. Transfers are not resumable or
idempotent. After a full upload, disconnect/cancellation cannot guarantee that
import was not accepted; clients never automatically retry it. Failed validation
publishes no resource, but verified cache objects may remain after publication
failure. This is not cache pruning.

Logs and events preserve their independent aggregate/per-machine sequences,
source unavailability and delivery gaps. Restoration uses the shared 64 MiB log
and 16 MiB event budgets, so importing can evict older observations. Frames are
not retained. Raw diagnostic names, digests and omission reasons come from the
retained record; file access uses only exact allowlisted keys, verifies the digest
while spooling under the evidence lock, then streams the spool. `diagnostics ID NAME`
prints raw bytes; its JSON document contains the descriptor and lossless hex bytes.
It is distinct from time-filtered application `logs ID`.

Checkpoint definitions include captured binary state independently of the source.
Projection validates the frozen intervention history against the captured consumed
input prefix, including admissions and rejections, before merging external
overrides. Only the pending suffix can change: the cursor, consumed prefix,
assigned fault IDs and captured world state cannot. Effects scheduled for later
by already-admitted inputs remain consumed; oracle calls are not reinjected.
Unavailable captures remain explicit receipts, not fabricated checkpoints.

Server comparison uses complete resolved execution definitions plus ordered
external overrides. It ignores only image reference provenance and HVF host
release and CPU features; resolved definitions do not mention the engine. Actual
architecture/backend, runtime asset digests, clock, policies, configuration,
overrides and checkpoint state/cursor/assets still participate. Original records are not
rewritten. Compatible inputs still need actual witness/full-state/fault evidence;
missing evidence yields NOT_COMPARABLE and differing results yield MISMATCH.

Code: [archives](../crates/seedbox-artifacts/src/archive.rs),
[checkpoint projection](../crates/seedbox-artifacts/src/checkpoint.rs),
[server bundles](../crates/seedbox-server/src/execution/bundles.rs),
[comparison](../crates/seedbox-server/src/execution.rs),
[client transfers](../crates/seedbox-client/src/transfers.rs).
