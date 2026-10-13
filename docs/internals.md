# Technical details

These mechanisms depend on ordering or state that is easy to overlook.
Each section links to the code that owns it.

## Reference TCG machine

`seedbox-machine` has a diskless, single-CPU RV64 reference backend using QEMU
software emulation. Its example uses `SEEDBOX_TCG_QEMU` and
`SEEDBOX_TCG_ASSETS` for the explicit patched executable and prepared backend
asset directory. Run `cargo run -p seedbox-machine --example tcg` inside the
project development environment after preparing those inputs. Factories load
owned guest assets; runtime construction does not build them.

The example runs two applications with seeded sleeps from one through four
seconds, plus an oracle that ends the run. Application outcomes are distinct
from kernel shutdown. It is separate from the production `seedbox run` backends.

The driver repeatedly calls `Machine::run`. A private QMP command limits QEMU's
native instruction budget, with one instruction mapped to one simulation
nanosecond. QEMU stops at that budget or when the CPU becomes idle; it cannot
automatically jump forward through idle time. `Blocked` reports the next virtual
timer deadline. The [driver](../crates/seedbox-machine/src/driver.rs)
tracks each machine's execution position and next timer, advancing the shared
clock only when no eligible runnable work or timer remains before it. Each round
brings active machines to its boundary, a blocked state, or exit. The machine observes the
read-only clock borrowed through its run context on its next `run`, delivering due timers before executing
more instructions. Guest shutdown returns `Exited`. The driver runs ready machines
on a persistent thread pool, holding the shared clock fixed until every call in
the batch returns. Each worker creates and retains its own host execution resources, which may be
thread-affine. Every run separately borrows those resources, the simulation clock,
and the machine's RNG through `RunContext`. The driver owns an injected global RNG
and derives an independent stream for each admitted machine in insertion order.
The machine's RNG cursor moves with it between workers; worker threads own no
simulation randomness. Failed additions consume no global RNG bytes.
Worker count does not change simulation results. Machine access, additions, and
scheduled wake/pause/resume/crash/reboot actions go through barrier callbacks.

An individually paused machine receives no positive execution budget while
other machines advance. Due requests still receive zero-budget admission runs:
elapsed time is adopted and overdue timers become eligible, without executing
guest instructions. Resuming also performs a zero-budget run at the current
time, without replaying CPU work from the paused interval. A machine with no
wakeup remains blocked. Pausing is driver state; `Machine` needs no separate
pause or resume methods.

Both reference machines expose lifecycle through queued `Request::Crash`,
`Reboot`, and `Shutdown`; the `Machine` interface consists of `run` and
`snapshot`. Enqueueing a request executes no work and does not acknowledge its
completion. The driver admits due requests in order through zero-budget `run`
calls, including while paused or exited. Lifecycle actions map into that same
request path. Reboot retains the machine identity and future queued work,
clears pause and old-boot work, and performs a fresh reset in its admission run.
Construction initializes on the first `run` using its borrowed dependencies.
Initial boot and each admitted reboot consume eight RNG bytes; repeated reboots
perform distinct resets. These calls execute no guest instructions. An exited
machine retains its exit reason until Reboot. Reboot clears earlier shutdown
requests; Shutdown after Reboot targets the new boot. Both backends carry
graceful shutdown requests through the named `seedbox-lifecycle` virtio-console
port, even while execution is paused. The guest supervisor performs cleanup and
poweroff only when guest execution resumes.

Playback controls can change at every barrier. Paused playback holds all work,
including current-time actions; limited rational rates wait on the host without
changing execution bounds. A single-quantum admission survives intermediate
barriers and completes at the next fixed quantum boundary. Status distinguishes
committed boundary positions from completed quanta. A one-shot pre-crash stop
retains the exact queued occurrence before its effect; ordinary wakeups and
crashes of exited machines do not consume the arm. Rearming resumes the held
occurrence and targets the next effective crash. Permanent stop prevents further
execution without crashing machines. The driver never sleeps or preempts an
active `Machine::run`; callers service controls and cancellation between steps.

The quantum is fixed at construction, with boundaries anchored at the initial
time. Each step respects both its requested time limit and the next quantum
boundary; intermediate barriers do not shift that boundary. Both examples use
two workers and a one-millisecond quantum. Packet
routing is not implemented. Console and lifecycle output preserve their native
occurrence timestamps and cross-channel order, then publish at committed barriers.
GDB is used only by tests to inspect stopped register and RAM state.

Snapshots own their boot inputs and QEMU migration state, including RAM, CPU,
devices, virtual timers, and pending lifecycle transitions. Machine snapshots
exclude driver-owned clock/RNG, worker resources, other machines, and networking.
Full simulation capture must preserve those dependencies and driver scheduling
state consistently; that capture API remains deferred. The QEMU process and control connection are recreated
on restore. Serial output already emitted by an instance is external observation
and is not emitted again by restoring its snapshot.

Code: [TCG machine](../crates/seedbox-machine/src/machine/tcg.rs),
[driver example](../crates/seedbox-machine/examples/tcg.rs),
[QEMU patch](../crates/seedbox-machine/qemu/bounded-run.patch).

## Reference PV machine

`cargo run -p seedbox-machine --example pv` uses the same Driver and common
machine definitions for native guests of the host architecture. Set
`SEEDBOX_PV_WORKER` to this crate's standalone worker and `SEEDBOX_PV_ASSETS`
to its prepared guest asset directory (`nix build .#machinePvAssets`). The
factory owns the PV kernel, helper and mandatory DynamoRIO runtime; it prepares
the application launch without exposing those details to Driver. The host
worker executable is not bundled into the guest filesystem.

Build `seedbox-pv-worker` from this crate inside the project development shell.
On Apple Silicon, sign a disposable worker copy with
[the Hypervisor entitlement](../crates/seedbox-machine/hvf.entitlements.plist).
The worker uses ARM64 KVM on Linux, HVF on macOS, or x86-64 KVM on Linux, with
one worker process per machine. Architectural snapshot/restore requires
macOS/HVF or x86-64 KVM; Linux ARM64 KVM snapshot/restore is unsupported.
Both architectures share the PV time page, event registers and lifecycle
console; the x86 platform is described in its
[worker module](../crates/seedbox-machine/src/machine/pv/worker/x86_64.rs).
`nix build .#machinePvTestFixtures` builds the native test inputs.

Code: [PV example](../crates/seedbox-machine/examples/pv.rs),
[shared example RNG](../crates/seedbox-machine/examples/support/mod.rs).

## Machine time bases and clock faults

In `seedbox-machine` every machine runs in its own time base `G`, which the
Driver maps to simulation time at the machine boundary; machines never see the
mapping. `G` is exact in units of 1e-9 ns, the identity until a rate change,
and reanchored at each change keeping `G(T)`, so accrued drift is never
rounded away. Into a machine go `floor(G(now))`, `floor(G(upto))` and input
times; out of it, output times, positions and timer deadlines map to the
earliest simulation nanosecond whose `G` reaches them, so nothing is early.
A machine reaches barrier `T` exactly when its position reaches
`floor(G(T))`, which keeps one set of common barriers and tiles each machine's
own time without gaps. Pending timers are re-converted when the rate changes.
Drift therefore needs no backend support, and the whole machine (counters,
timers and CPU progress) runs at the drifted rate. Simulation-control payloads
are not translated: their times and durations stay simulation time. The
mapping, last machine-time position and pending machine-time timer are
Driver-owned and excluded from machine snapshots.

A realtime step is guest kernel state instead: the shared PID 1 performs
`Request::StepRealtime` with `adjtimex(ADJ_SETOFFSET)` and acknowledges it,
which moves only `CLOCK_REALTIME`, re-evaluates absolute realtime timers and
cancels cancel-on-set timerfds. Linux refuses a step that would put realtime
below `CLOCK_MONOTONIC`. Steps end with their boot; drift survives
crash and reboot. Every boot sets realtime to the epoch in
`/seedbox/clock.bin` plus the machine counter before `Ready`; the epoch comes
from the backend factory's `epoch_seconds` and the TCG board RTC derives from
it. Host `Action::Drift` and `Action::StepRealtime` reach the same effects
through the unified schedule.

Code: [driver translation](../crates/seedbox-machine/src/driver.rs),
[mapping](../crates/seedbox-machine/src/clock/projection.rs),
[guest step](../crates/seedbox-machine/src/pv/guest_service.c),
[boot realtime](../crates/seedbox-machine/src/pv/guest_service_init.c).

## Bounded virtual clock reads

Reading the host counter would make a guest polling loop's iteration count
depend on host scheduling. Kernel clock reads use a
shared PV page. Repeated reads charge a fixed increment, and a read that would
pass the published bound yields to the VMM before returning that time. This
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

Code: [x86 clock](../kernel/seedbox.c), [ARM clock](../kernel-arm64/seedbox.c),
[seedbox-machine x86 clock](../crates/seedbox-machine/guest/kernel/x86_64/seedbox.c).

## Progress from translated blocks

A computation or spin loop that never reads the clock still needs to let other
guest threads and machines make progress. The DynamoRIO client decrements a
per-thread budget at every translated basic-block entry and issues a raw
`getpid` syscall when the configured budget expires (10,000 entries by default).
The kernel charges that syscall as PV progress. Existing executable code, shared libraries and JIT code go through the
translator without application-specific flags or recompilation.

The budget measures block entries, not individual executed instructions. A basic
block is a sequence of instructions with one entry and no internal control-flow
branch. Translator-internal work and kernel execution are outside that counter.
Instrumentation must leave the application's registers and comparison flags
intact. On ARM, the client uses arithmetic that leaves those flags alone and a
branch that tests a register directly (`CBNZ`, compare and branch if nonzero).
DynamoRIO's drreg extension manages the temporary registers.

On x86-64 the same client replaces `RDRAND` and `RDSEED`. CPUID hides both, but
AMD hardware cannot trap them, so an application that executes them anyway
would read host entropy. Each replacement draws 8 bytes from the guest
kernel's `getrandom`, which is seeded from the machine's RNG stream, writes the
destination with the instruction's width and reports success (CF set; OF, SF,
ZF, AF and PF clear). The draw is an ordinary charged syscall. Uninstrumented
guest code is not covered: the patched kernel reaches these instructions only
behind CPUID checks and boots with `random.trust_cpu=off`, and the supervisor
and translator do not use them.

Code: [progress client](../guest/progress/progress.c),
[seedbox-machine progress client](../crates/seedbox-machine/guest/progress/progress.c).

## Progress inside kernel polling

An x86 kernel `cpu_relax()` loop may hold a lock with interrupts disabled. It
cannot simply call the scheduler to let time advance. `PAUSE` is the CPU's hint
that code is spinning; it does not itself advance Seedbox time. Seedbox adds a
bounded PV clock read after that hint, allowing the guest CPU to stop at a time
bound without allocating, acquiring another lock, scheduling or enabling
interrupts. Interrupt delivery stays deferred until the guest permits it.

Each hook charges a 100 ns read and is inactive before PV activation.
This covers the patched kernel `cpu_relax`/`rep_nop` paths. It does not cover
arbitrary assembly `PAUSE`, early decompression, the vDSO, or every ARM kernel
polling loop. Application polling is handled by binary instrumentation.
The `seedbox-machine` x86 kernel patches the same sites; its shared PV ABI 3
read, like every ABI 3 clock read on both architectures, charges 1 ns instead.

Code: [kernel clock](../kernel/seedbox.c), [patch sites](../kernel/apply.py),
[seedbox-machine patch sites](../crates/seedbox-machine/guest/kernel/x86_64/apply.py).

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
The shared device models have backend-specific validation profiles; sharing
code does not make those profiles identical.

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
