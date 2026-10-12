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
manages the temporary registers. Its ARM reservations are conservative and
restore scratch before the application instruction: a fault handler can observe
a register even when it is dead on the normal path.

The ARM due path captures full application state, the original charged PC and
old block charge, then leaves the clean call through the dispatcher. A fixed
thread-owned record retains the budget's raw TLS slot, quantum and finite
payment callback. After complete cache-exit processing on a fresh dispatcher
stack, the core adds a quantum before each marked syscall, repeats negative
overshoot, and refunds the old charge once before looking up current code.
There is no return edge to the old fragment and no fragment pointer in the record.

Only the finite, lock-free payment callback is non-transferable during thread
synchronization. Delayable application signals remain in the existing pending
queue until payment, refund and record release complete; synchronous private
faults retain DR's fault handling. This does not make an arbitrary blocked
callback safe. A separate context-aware predicate wait can instead be abandoned
at its complete application context: that entry must recheck the still-set
predicate before any application instruction, and completion always redirects.
The wait cannot own irreversible effects, locks or cache-generation references.
Its synchronization capability ends on normal completion and before a signal or
API redirect abandons its private stack. Otherwise a later fault could mistake
reused dispatcher storage for the saved application context.

The translator patch saves each vector register of the clean call in its own
context slot. Conservative scratch restoration, full-state capture and dispatcher
re-entry preserve exception-visible state as well as normal-path values.
Inactive SME-only SVE signal headers are preserved without expansion; these
patches do not implement active SME/streaming state or per-thread vector-length
changes.

The ARM translator detects rewritten code through the application's `IC IVAU`
cache-maintenance instructions (`-no_hw_cache_consistency`). Translated reads of
`CTR_EL0` clear DIC (`-fake_ctr_dic`), so applications must issue `IC IVAU` even
when the underlying CPU would permit omitting it. DynamoRIO exposes this internal
option in release builds. Enabled `hw_cache_consistency` instead uses code-page
write protection and self-modifying-code sandboxing. In the observed Kafka
startup, that mode grew a broker's memory to about 2 GB and it was killed;
with application-driven invalidation, it used about 250 MB and kept starting.
The normal eager IC path invalidates executable pages and clears their historical
execution bookkeeping. The experimental `-seedbox_icache_images` option is not
enabled by the normal guest launcher. It gives each emitted ARM fragment immutable,
page-separated decoded source spans, including followed and elided branches.
A backing-page index selects dependencies
of the published cache line, including executable aliases of a writable mapping.
Comparing only those depended-on bytes preserves unchanged same-line neighbors.
Position-independent architectural HINTs retain their owned raw encoding; they
remain valid when an optional feature such as BTI is absent. PC-relative operands
still require relocation.

Private anonymous keys require an observed successful mapping birth. Shared file
keys use device, inode and file offset, checked against the mapped interval.
Preexisting or unresolved mappings and private file/COW publication take the
conservative path. Mapping, permission and discard transitions revoke admission;
in-flight changes cannot authorize equality. Unknown alias-producing operations
also disable indexed admission. Logical backing identity is not a physical-frame
or host-time identifier, and equal bytes cannot restore a revoked incarnation.

Actual overlaps and unresolved ownership retain DR's full synchronization and
context protocol before deletion; the index does not provide arbitrary no-wait
retirement. Publication is not deferred to the writer's ISB: another thread can
synchronize before that ISB. The baseline eager page path retains complete
multi-page and open-top ranges. Code modified by deliberate publication bypasses,
such as proc-memory injection, is outside this cooperative testing contract.

Code: [progress client](../guest/progress/progress.c),
[x86 syscall charge](../kernel/seedbox.c), [ARM syscall charge](../kernel-arm64/seedbox.c),
[vector context patch](../guest/progress/dynamorio-cleancall-simd.patch),
[dependency index](../guest/progress/dynamorio-icache-index.patch),
[predicate capability lifetime](../guest/progress/dynamorio-predicate-wait-end.patch).

On x86 a charge can span several scheduling boundaries. Its unpaid time is
retained with the machine and drained while the vCPU is stopped, before guest
execution resumes. Timers can become pending during that interval; checkpoints
preserve the remainder.

The TUI shows each machine's configured nominal speed by default. While
throttled it shows the effective nominal speed after backend rounding and
clamping; healing restores that machine's configured speed. The fault's
recorded input keeps the original request.

## Experimental block executor proof

[`seedbox-executor`](../crates/seedbox-executor/src/lib.rs) is an opt-in,
Linux-only same-ISA experiment, not the normal application runtime. Its
transport-independent Rust core decodes bounded blocks, emits native interiors
and source-PC maps, tracks source intervals by backing identity, and owns the
signed precharge state machine. AArch64 uses an explicit small classifier;
x86-64 uses iced-x86. Unknown instructions and out-of-range relocations fail
before the block executes. Both proof subsets reject atomics and application
syscalls; there is no native escape for unsupported code.

The libc-free [`linux-proof`](../crates/seedbox-executor/examples/linux-proof.rs)
ELF uses executor-owned traps and Linux signal return for diagnostic dispatch.
Linux saves the application state before any metering helper runs. A due charge
makes a raw marked `getpid` from a stable gateway, refunds the old block and
looks up the current generation before precharging again. A native fault keeps
its block's precharge. No register is reserved. This trap-per-block mechanism
adds kernel charges and is **not** the intended linked-edge hot path, a timing
equivalence claim, or a selected production signal architecture.

The controlled fixture has one explicitly shared memfd backing, an R-X source
view and a private RW writer view. While task A is parked with an old map pin,
task B's precise protected stores append neighboring code and overwrite two
overlapping source spans. Invalidation removes lookup permission without waiting
for A; immutable maps/cache memory survive until their final continuation pin is
released. The source view is never made writable. This does not remap private
anonymous memory into shared memory or implement COW, arbitrary writers, kernel
writes, mapping lifecycle, or nested application signal handling. The deliberate
fault is reconstructed and asserted in the diagnostic handler, not delivered to
an arbitrary application's handler.

The fixture covers original-address call/return, a syscall-free backward loop,
PC-relative address formation, SIMD arithmetic and a protected-load fault after
invalidation. It checks all scalar registers (including dead scratch), flags,
SP/TLS, all ARM Q registers and FP controls, or x86 XMM registers and FP controls.
General x86 extended-state patterns, fault/restart/handler edits, loader/process
integration and snapshots remain outside this proof. Build the ELF with the
crate's `tools/build-proof.sh` inside the project Linux development shell; its
`tools/initramfs.py` wraps that ELF as PID 1. The ignored
[HVF proof test](../crates/seedbox-arm64/tests/executor_proof.rs) checks actual VMM
horizon returns between marked-call console delimiters against an unmarked
control. Native-container syscall completion alone is not PV evidence.

The separate ARM [`linked-proof`](../crates/seedbox-executor/examples/linked/mod.rs)
uses the [linked emitter](../crates/seedbox-executor/src/aarch64/linked.rs): inline
signed precharge, direct edges and fully tagged finite indirect caches branch
through revocable per-entry gates. Only a due payment or unresolved gate traps;
unknown indirect targets fail closed. Code/maps remain pinned independently of
gate permission. Source mutation revokes incoming direct and indirect access
before publication, without waiting for the parked task.

This cache belongs to one execution task. Its seven-instruction scratch spill
transiently puts X16 in TPIDR_EL0; original TLS and X16/X17 are retained in private
resident state, with a reconstruction map at each emitted instruction. TLS is
restored before application execution. A suspended internal sequence must resume
with its exact hardware TLS and PC; the diagnostic trap handler is TLS-independent.
No application stack slot or register is reserved. This is not multi-task cache
sharing, arbitrary asynchronous signal delivery, or a new kernel ABI. Deterministic
boundary traps and a separate instrumentation-fault case test that distinction.
The crate's `tools/build-linked.sh` builds this opt-in ELF. Its fixed-work modes
separate install, first-pass and warm execution; ordinary Linux wall measurements
are not PV-time or full-runtime qualification.

The separate [matched JIT fixture](../crates/seedbox-executor/examples/linked/jit.rs)
executes every published version and a neighboring function through the same
finite revocable gates. Append/recompile and four-byte patch rows have matched
no-publication controls, exact per-version call checks, and disjoint outer
publication/first-use/warm clocks. Its controlled driver runs natively for the
candidate but is instrumented by DR; diagnostic plans distinguish real DR
emission from fault-context reconstruction. These clocks include completion
faults and driver work, not just translator cost. The
[measurement tool](../crates/seedbox-executor/tools/measure-jit.py) bounds fresh
finite-arena batches and preserves failed samples instead of treating them as
speedups. No application/runtime selection is added.

The opt-in `linked-return-hint` example feature tests one separate hot-path
variant: a fully tagged RET-X30 prediction before the existing indirect lookup.
It compares actual original LR, preserves NZCV and scratch maps, and enters the
same revocable gate. A wrong prediction uses the full existing lookup; unknown
targets still fail closed. It neither shares scratch across blocks nor changes
charge points. The default emitter has no hint.

The opt-in `linked-region` proof carries scratch ownership between revocable
entries. Each generation has a native entry that spills X16/X17 and a carried
entry with X16 pointing to private State and both original values saved. Every
original block still loads, subtracts, commits and tests its own charge. Source
instructions that use either scratch register restore both before execution and
respill after architectural completion; implicit faults therefore retain even
scratch values dead on the successful path. PC-relative operands are relocated
after these transitions. Indirect arms compare the complete original target
without modifying NZCV or updating LR before a match. Both entry gates revoke
before publication; keeping scratch across edges does not combine source spans
or increase the generation's invalidation range. This adapts explicit entry
conventions and lazy restore/respill mechanisms inspected in DynamoRIO's
`core/arch/aarch64/emit_utils.c` and `ext/drreg/drreg.c`, not its trace charge plan.
The native writer and TLS-independent diagnostic handler never enter translated
code while an outer scratch transaction is active. General nested application
signals and shared multi-execution-task caches remain unsupported.

The `linked-raw` proof uses that carried ABI with a stable getpid-only gateway
([emitter](../crates/seedbox-executor/src/aarch64/linked/gateway.rs)). It additionally
saves X0/X8 in private State, issues the marked raw syscall, commits the quantum
and counter, refunds the captured old charge, restores X0/X8, and branches to
the original entry's revocable carried gate. It never returns to a cache-body PC.
SP, LR, TLS, NZCV and FP/SIMD are untouched; no C/Rust call or application stack
frame is needed. Maps distinguish syscall return from budget/counter/refund
commits. Unknown indirect targets and unexpected syscall failures remain fatal.
The diagnostic observer pins old maps before the real syscall, parks the writer
after payment, and resumes the exact raw continuation after invalidation. It
uses extra observational traps; those add real kernel charges and are absent
from wall timing. Neither that observer nor the native writer can reenter
translated application code. Normal raw execution permits no uncontrolled
writer or application signal; arbitrary nested/kernel continuations require
additional ownership, not permission inferred from these entry gates.

ARM LL/SC cannot simply be copied: host-only exits may lose the physical
exclusive monitor and make STXR failures host-timed. A future implementation
must retain the [guest-epoch reservation contract](#software-exclusive-reservations),
including ABA detection, per-attempt consumption and precise faults; LSE requires
separate native-semantics tests. The proof currently rejects LL/SC, CLREX and
LSE rather than claiming those obligations are implemented.

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
external overrides. It ignores only image reference provenance, descriptive
EngineDescriptor version/platform/backend/timebase fields, and HVF host release
and CPU features. Actual architecture/backend, runtime asset digests, engine launch
kind/owner-pipe/live-observation settings, clock, policies, configuration, overrides
and checkpoint state/cursor/assets still participate. Original records are not
rewritten. Compatible inputs still need actual witness/full-state/fault evidence;
missing evidence yields NOT_COMPARABLE and differing results yield MISMATCH.

Code: [archives](../crates/seedbox-artifacts/src/archive.rs),
[checkpoint projection](../crates/seedbox-artifacts/src/checkpoint.rs),
[server bundles](../crates/seedbox-server/src/execution/bundles.rs),
[comparison](../crates/seedbox-server/src/execution.rs),
[client transfers](../crates/seedbox-client/src/transfers.rs).
