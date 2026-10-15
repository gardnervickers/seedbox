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

Some syscalls are not application work at full price. On ARM the PV clock
disables the vDSO, so `clock_gettime`, `clock_getres` and `gettimeofday`
are syscalls; they cost only the clock read they make, as x86 vDSO reads do
through RDTSC emulation. The translator also makes syscalls for itself:
suspending threads to flush rewritten code, returning from its signal
handler and waiting on futexes. Its syscall stubs put a fixed mark in a
register the syscall leaves unused (x7 on ARM, rbx on x86), and the kernel
charges a marked syscall a tenth of a syscall. x86 counts whole syscalls, so
every tenth marked syscall counts as one. A JVM makes millions of these while
it starts, about ten times its own progress calls, and at full price they
dominated simulated startup time. Both architectures build the translator
core from source to apply the mark.

Instrumentation must leave the application's registers and comparison flags
intact. On ARM, the client uses arithmetic that leaves those flags alone and a
branch that tests the sign bit directly (`TBZ`). DynamoRIO's drreg extension
manages the temporary registers, conservatively, and the charge puts the
application's value back before the next application instruction. drreg would
otherwise restore it lazily, at the next application read, and a register that
straight-line code no longer reads can still be read on a fault path: HotSpot's
implicit null checks deoptimize through handlers that save such registers, and a
budget left there corrupted the heap.

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

A flush stops all threads. JITs invalidate a range one cache line at a time,
so a translator patch joins each thread's consecutive lines into one pending
range and flushes it at that thread's next `DSB`, which architecturally
completes the invalidations, or at its next other entry to the translator. A
JVM startup otherwise stopped all threads about 19,000 times. While the
flushing thread waits for another thread's flush, it must not be redirected:
that abandoned its own flush, and before a syscall it skipped the syscall.

Flushes still race a JVM creating a process.
Translator patches keep a thread that is forking in place (redirecting it skipped
the `fork`), count a thread waiting in `vfork` as stopped (it cannot take the
suspend signal until its child execs, which may wait for the flush), and requeue
signals that arrive while a thread is suspended, since it may resume into a
blocking syscall and never reach the translator's pending-signal delivery.
A signal that interrupts a thread waiting for such a flush is delayed until the
thread re-enters the code cache: running the handler at once abandoned the wait
while the thread still looked parked, so later flushes redirected it from a
stale context (JFR sampling and `tgkill` crashed compiled Java code this way).
A thread starting a stop-all-threads lets threads already waiting for one go
first: forking also stops all threads, and a JIT thread flushing after every
code write otherwise retook the lock each time and the fork never ran.
Minimal reproducers are in [`tools/jvm-fuzz/repros`](../tools/jvm-fuzz/repros).

Code: [progress client](../guest/progress/progress.c),
[x86 syscall charge](../kernel/seedbox.c), [ARM syscall charge](../kernel-arm64/seedbox.c),
the vector context and translator syscall mark commits in the ARM DynamoRIO
fork (the `dynamorio-arm` flake input, [`nix/dynamorio.nix`](../nix/dynamorio.nix)),
and the [x86 syscall mark patch](../guest/progress/dynamorio-x86-translator-syscalls.patch).

On x86 a charge can span several scheduling boundaries. Its unpaid time is
retained with the machine and drained while the vCPU is stopped, before guest
execution resumes. Timers can become pending during that interval; checkpoints
preserve the remainder.

The TUI shows each machine's configured nominal speed by default. While
throttled it shows the effective nominal speed after backend rounding and
clamping; healing restores that machine's configured speed. The fault's
recorded input keeps the original request.

## User CPU time

Linux samples CPU time per tick: a tick that interrupts user mode is user time,
anything else is system time. Guest time advances only in the kernel, so every
tick interrupts it, and without a correction translated compute would all be
system time: `utime` stayed zero, `ITIMER_VIRTUAL` never fired, and
`getrusage` reported all work as system time. The kernel counts a tick that
interrupts a progress charge directly, not a softirq or a nested interrupt, as
user time. On ARM the mark covers the bulk charge, whose rendezvous takes the
tick. On x86 it covers the progress call's yield, after which the VMM injects
the timer interrupt. Only tick accounting reads the mark. RCU and the
scheduler still see kernel mode, and the charges are unchanged. Syscall and
translator charges remain system time.

Precise (`VIRT_CPU_ACCOUNTING_GEN`) accounting would not help: it reads the
clock at each kernel entry and exit, and the clock does not move in user mode.
It also needs a `nohz_full` CPU, and the guest's one CPU is the boot CPU.

Code: [ARM charge](../kernel-arm64/seedbox.c), [x86 hook](../kernel/seedbox.h),
the `kernel/time/timer.c` edit in both patch scripts, and the
[guest check](../guest/progress/fixtures/cputime.c).

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
handled by binary instrumentation. Two kinds of application code run uncharged:
the translator's own code, so a translator spin loop must sleep rather than
yield (its thread-exit lock once froze guest time), and `rseq` critical
sections, which run from an uninstrumented native copy. Preemption and signals
arrive only at charges, so `rseq` aborts are rare in a simulation.

Kernel code charges no guest time of its own, so a kernel loop that waits for
another task to run would never see the tick that sets `need_resched` on the
single guest CPU. Such loops yield through `cond_resched()`: on ARM, a call that
does not reschedule makes the same bounded read as `cpu_relax()`. Without it a
JVM exiting several threads hung in `shrink_dcache_parent`, retrying while the
task killing a `/proc` dentry stayed preempted. The x86 kernel lacks this hook.

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

## Out-of-memory kills

A process the guest kernel kills for lack of memory exits with SIGKILL, which
looks like any other `137`. The guest supervisor therefore reports each kill on
its management transport as an `SBX_OOM1` frame, before it reports the exit.
`/proc/vmstat` `oom_kill` counts kills. Linux reports a victim's PID, name and
memory only in its log, so the supervisor polls `/dev/kmsg` from its start and
parses the kernel's own `Killed process` record; it accepts only facility 0,
which userspace cannot write. If the log ring overwrote a counted kill's
record, that kill's frame carries no details. Both sources are guest state, so
a replay reports the same kills at the same simulated times.

The shared decoder turns each frame into an observation. The server publishes an
`outOfMemoryKill` event, keeps the last 64 kills per machine in the world, and
lists them in `diagnostics`. Like other events, they are not compared evidence.
A kill that does not end the application, such as of a child process, is
reported when it happens.

Code: [guest parser](../guest/oom-report.h), [frame](../crates/seedbox-engine-protocol/src/out_of_memory.rs), [server world](../crates/seedbox-server/src/world.rs).

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

Code: the exclusives commit in the ARM DynamoRIO fork, [ARM kernel patches](../kernel-arm64/apply.py), [assembly fixtures](../guest/progress/fixtures/).

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

The server sizes the pool (`seedbox serve --machine-workers`). By default one
worker runs every machine on the coordinating thread: concurrent seeds use host
cores better. A larger pool helps a run whose machines could use spare cores, as
when memory limits how many simulations run at once. An explicit count
is recorded among a run's performance parameters, and the engine report's
scheduler evidence states the count used. Neither is part of identity, witness or
comparison, so a replay may use a different count and must still match.

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

Process faults signal the application's process group, as job control does.
`stop` kills the group and reaps its descendants, including those adopted by
PID 1, before it is confirmed; `pause_process` stops the group with SIGSTOP. A
descendant that calls `setsid()` has left the group: it keeps running through
both, keeps its listening ports across `restart` (the new incarnation cannot
bind them until it exits), and ends only by exiting or with a machine crash.
Daemonizing applications must close inherited listeners, or the survivor also
holds the application's own port. When the application's main process exits,
the supervisor relays the output already queued and reports the exit at once;
it does not wait for descendants that inherited stdout or stderr, and nothing
they write later is logged.

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
[supervisor](../guest/compose-init.c),
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
The default policy drops every unflushed sector; `simulator.disk.torn_writes`
in a resolved definition keeps each one with probability one half. Neither
keeps the guest's unwritten page cache.
Restarting an application process is different from crashing its machine and disk.

A machine whose service mounts named volumes gets one disk. The supervisor
formats it as ext4 on first boot and bind-mounts one directory per volume, so
volumes share its space. Its size is the sum of the resolved volumes'
`sizeBytes` (each 1 GiB unless authored), at most 64 GiB, and is part of the
world document. The disk is sparse: it stores only written sectors, in the
worker's memory. Host memory, crash transfers, checkpoints and disk digests
scale with written sectors, not the declared size.

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

## Owned cache objects

Checkpoint capture, bundle import and edited launch records create *owned* cache
objects; an object the cache already holds stays an ordinary input, and an
ordinary put of an owned object's bytes makes it one. The session's `owned/`
ledger holds an empty marker per owned object, made durable before the object is
named. One store lock orders naming, adoption and removal, so an object an
ordinary put returned is never reclaimed.

After `delete` and checkpoint deletion, the server removes every owned object
that no remaining resource names: submitted and resolved specs, undeleted
imported checkpoints, retained input files, and checkpoints exported from a
source that still exists (a fetched branch definition stays submittable). Roots
over-approximate: any digest-shaped string counts. Reclamation holds the evidence
lock, which imports, exports and deletes share, and then the reference lock for
writing. Checkpoint export and the resolved-input commit in execution hold it for
reading from their owned puts until the export registry or the resource and
retained inputs name the objects. A client that submits a definition after every
resource naming its objects was deleted gets a missing-asset error.

Graceful shutdown reclaims once more; objects still referenced then stay as
ordinary cache content for saved definitions. Startup removes the objects listed
in an abandoned session's ledger, because no resource survives a crash. Objects
from earlier versions or lost markers are never pruned automatically.

Code: [content store](../crates/seedbox-artifacts/src/store.rs),
[reclamation](../crates/seedbox-server/src/execution/content.rs),
[session ledger](../crates/seedbox/src/cli/daemon.rs).

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

The application suites also run `crates/seedbox/tests/determinism.rs`: one
definition built from `harness/fixtures/determinism` must keep its witness,
full state and fault history under slowed, paused and observed playback, beside
a concurrently controlled simulation, with checkpoints captured, and with fault
previews issued while paused. Its branches must match each other, and edited
inputs must reproduce output, events and faults before their first changed time.
A keyed fault whose HTTP client disconnects must be admitted once. Runs exported
from that server are imported into a second one with a fresh state directory,
where fresh runs of the same projects (one image pinned by registry digest) must
match them and an imported checkpoint of a deleted source must branch.

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
publishes no resource; the binaries it owned are reclaimed with the next deletion
or at shutdown.

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
