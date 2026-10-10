# Seedbox determinism techniques

Seedbox runs existing applications in real Linux guests while controlling their
observable progress. The difficult cases are often small: a failed atomic store,
an interrupt landing one instruction later, or two byte representations of the
same saved register value. Each can make otherwise identical runs diverge.

This catalogue explains the mechanisms that address those cases, the assumptions
that make them work, and the implementation and checks to consult before
changing them. These are current mechanisms, not a claim of complete determinism
for every instruction, kernel path or host. Detailed contracts remain in the
linked guides.

The **guest** is the simulated Linux machine; the **host** runs Seedbox. The
**VMM** (virtual machine monitor) runs guest CPUs through Linux KVM or Apple's
Hypervisor framework. A **VM exit** returns control from a guest CPU to the VMM.
**PV**, or paravirtualization, means the guest kernel cooperates with the VMM
through shared memory and explicit requests. A **witness** is a digest of the
execution evidence used to compare runs. Assembly names below identify the
implementation; their relevant behavior is explained where they appear.

| Technique | Scope | Purpose |
| --- | --- | --- |
| [Bounded virtual clock reads](#bounded-virtual-clock-reads) | x86 and ARM | Let time polling progress without reading host time |
| [Progress from translated blocks](#progress-from-translated-blocks) | Applications on both platforms | Make computation and application spin loops advance time |
| [Progress inside kernel polling](#progress-inside-kernel-polling) | x86 kernel | Reach time boundaries while locks are held |
| [Observation without guest progress](#observation-without-guest-progress) | Both backends | Keep watchdog frequency out of execution results |
| [Software exclusive reservations](#software-exclusive-reservations) | ARM applications and guest kernel | Hide host monitor loss while preserving guest invalidation |
| [Interrupt delivery at an idle rendezvous](#interrupt-delivery-at-an-idle-rendezvous) | ARM kernel and VMM | Avoid variable interrupt landing locations |
| [Canonical saved vector state](#canonical-saved-vector-state) | x86 kernel task saves and live vCPU evidence | Give equivalent YMM states one representation |
| [Independent random streams](#independent-random-streams) | Shared engine | Keep unrelated random decisions from consuming each other's stream |
| [Reseeding after a fork](#reseeding-after-a-fork) | x86 runtime checkpoints | Give a restored guest the branch's entropy |
| [Time and replay inputs retained together](#time-and-replay-inputs-retained-together) | Public CLI | Make a run independent of today's host clock and installed engine |
| [Parallel execution within causal windows](#parallel-execution-within-causal-windows) | Shared world scheduler | Make worker completion order irrelevant |
| [Shared pages instead of repeated exits](#shared-pages-instead-of-repeated-exits) | x86 PV device paths | Reduce exits while preserving publication boundaries |
| [Dirty logging only when a fork needs it](#dirty-logging-only-when-a-fork-needs-it) | x86 KVM | Avoid checkpoint overhead on ordinary runs |
| [Reuse the digest of a verified zero page](#reuse-the-digest-of-a-verified-zero-page) | x86 terminal evidence | Reduce hashing cost without omitting memory |

## Bounded virtual clock reads

A loop polling the time must eventually finish, but the host's counter would
make its iteration count depend on host scheduling. Kernel clock reads use a
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

The invariant is that a guest never observes time beyond the boundary the VMM
has permitted. See [x86 clock code](../kernel/seedbox.c), [ARM clock
code](../kernel-arm64/seedbox.c), the [ARM kernel
patches](../kernel-arm64/apply.py), and the [horizon
regressions](../crates/seedbox-arm64/tests/world_horizon.rs).

## Progress from translated blocks

A computation or spin loop that never reads the clock still needs to let other
guest threads and machines make progress. The DynamoRIO client decrements a
per-thread budget at every translated basic-block entry and issues a raw
`getpid` syscall every 10,000 entries. The kernel charges that syscall as PV
progress. Existing executable code, shared libraries and JIT code go through the
translator without application-specific flags or recompilation.

The budget measures block entries, not individual executed instructions. A basic
block is a sequence of instructions with one entry and no internal control-flow
branch. Translator-internal work and kernel execution are outside that counter.
Instrumentation must leave the application's registers and comparison flags
intact. On ARM, the client uses arithmetic that leaves those flags alone and a
branch that tests a register directly (`CBNZ`, compare and branch if nonzero).
DynamoRIO's drreg extension manages the temporary registers.

See the [client](../guest/progress/progress.c), [progress guide](progress.md),
and [application acceptance harness](../crates/seedbox/tests/applications.rs).
The old ARM packing and retry-block exclusions were a partial mitigation and are
no longer the execution mechanism.

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

See [seedbox_cpu_relax](../kernel/seedbox.c), the [patch
sites](../kernel/apply.py), and the coverage and spinlock evidence in [the
progress guide](progress.md#x86-evidence).

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

See [x86 PV execution and watchdog
tests](../crates/seedbox-vmm/src/machine/pv.rs) and [ARM execution and epoch
tests](../crates/seedbox-arm64/src/machine.rs). Watchdog variation is a useful
negative control: results must match even when the number of host interruptions
changes.

## Software exclusive reservations

ARM can implement an atomic update with two instructions. A **load-exclusive**
(`LDXR`) reads a value and asks the CPU to remember a reservation on its memory
location. A **store-exclusive** (`STXR`) attempts a write and reports whether
that reservation still allowed it. Programs normally retry if the store fails.
The hardware keeping track of this reservation is called the exclusive monitor.

A host interruption can clear that monitor even when no guest thread has touched
the value. Even one guest CPU can therefore observe unpredictable failed stores.
An application might count those failures or take a different backoff path.
Skipping them in progress accounting cannot hide them from the application
itself.

The translator saves a software reservation containing address, value, size and
guest epoch. At the application store, it consumes that reservation and runs a
short internal loop. The **epoch** is a generation number recording guest
boundaries that invalidate a reservation:

```text
application exclusive load:
    read the guest epoch, then load the application value
    save that epoch, address, value and access size as a reservation

application exclusive store:
    take and clear the saved reservation
    if no reservation exists, or the address or size differs:
        return failure to the application
    repeat:
        read the target using a fresh physical exclusive reservation
        if the value or live epoch differs from the saved one:
            return failure to the application
        if the physical store succeeds:
            return success to the application
        otherwise retry internally
```

Clearing the saved reservation means a second application store cannot reuse it
without a new exclusive load. Physical monitor loss alone retries below
application control flow and block counting. The retry loop deliberately has no
host-failure budget: exhausting a budget would expose host interruptions again.
The memory operation has not completed while these retries are happening.

The kernel advances the epoch on return to application mode (ARM calls this
EL0). The stopped VMM advances it at exits that represent guest events, such as
PV requests, and before device delivery. Host interruptions leave it alone. The
epoch check sits **inside** the physical exclusive pair. If a guest interruption
happens after the check, clearing the physical monitor prevents the store from
succeeding; the next attempt detects the changed epoch.

The internal loop reads the epoch from a CPU system register, `TPIDRRO_EL0`,
which application-mode instructions can read but cannot write. Using a register
avoids another memory access between the physical exclusive load and store.
Ordinary application reads of that register are translated to Linux's normal
64-bit value of zero, so the epoch does not become an application input. Before
writing it, the VMM checks the CPU's processor-state register, PSTATE, to learn
whether the guest is in application or kernel mode. In kernel mode (EL1), Linux
may be using the epoch register as temporary storage, so the kernel publishes
the epoch on return instead.

This relies on one guest CPU and controlled guest invalidation boundaries.

Implementation details that must stay aligned:

- The shared PV page stores the epoch at byte 40. The kernel increments it after
  signal and scheduling work, before restoring application registers, and clears
  the physical reservation. The VMM increments it before device delivery even
  when delivery might modify memory and then fail. Ordinary plan publication and
  backend maintenance leave it unchanged.
- The translator disables the shortcut for load/store pairs within one block:
  a guest exception can interrupt that block too. It saves overlapping address
  and destination registers before the load overwrites them. An explicit
  reservation-clear instruction (`CLREX`) clears the software reservation.
- A progress syscall between the application's load and store is a real guest
  boundary and may cause a repeatable failure. The protocol does not promise
  zero application-visible retries.

Fault handling is part of the mechanism. A faulting store must reconstruct the
original application instruction pointer and full register values. That includes
stores whose success result the application discards and operations using the
register borrowed by the translator. A spill slot is memory used to save a
register temporarily; the translator's own slots must not be mistaken for the
instrumentation client's slots. The drreg lookup is bounded to its public slot
range to prevent that confusion. A discarded store status (`WZR`, ARM's
write-discarding zero register) needs a private temporary register so internal
retries can still test whether the store succeeded.

Separately, two kernel helpers for futexes (the primitive behind many application
locks) also use exclusive load/store
sequences. They retry physical failures locally rather than exposing their old
128-attempt budget through a retryable `EAGAIN` error and rescheduling. Their
fault handling, value-mismatch exits and memory-ordering barriers remain intact.
The audited translator atomics and enabled ARM kernel atomic helpers also retry
physical failures internally, outside application block counting.

See [runtime integration and acceptance](progress.md#arm-software-reservations),
[translator patch](../guest/progress/dynamorio-exclusives.patch), [kernel hook and futex
patches](../kernel-arm64/apply.py), and [exclusive regression
workload](../harness/acceptance/workloads/exclusives.yaml). The [assembly
fixtures](../guest/progress/fixtures/) cover widths, pairs, consumption,
explicit reservation clearing (`CLREX`), syscall/ABA invalidation, overlapping
operands and signal contexts. ABA means a value changed and then changed back:
comparing values alone misses it, but a guest boundary that changes the epoch
still invalidates the old reservation. [PR
244](https://github.com/gardnervickers/seedbox/pull/244) records the fresh-run,
full-state and replay evidence.

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

See `seedbox_idle` in [the ARM kernel](../kernel-arm64/seedbox.c), [deferred
interrupt regression](../crates/seedbox-arm64/tests/review_deferred_bound.rs),
and [masked-yield
regression](../crates/seedbox-arm64/tests/review_yield_masked.rs). This
addresses the idle rendezvous, not arbitrary interrupt delivery everywhere.

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
continue to save and restore the raw KVM state. The
[normalizer](../crates/seedbox-vmm/src/xsave.rs) has unit coverage for every YMM
payload bit, and the [KVM test](../crates/seedbox-vmm/tests/xsave.rs) checks actual
guest register values after restoring absent, present-zero and nonzero states.

This repair does not cover signal-frame saves, direct application XSAVE, or all
other state components. See [the kernel patch](../kernel/apply.py) and the
controls and remaining scope in [the progress guide](progress.md#saved-x86-vector-state-representation).

## Independent random streams

One shared random-number stream would couple unrelated operations: an extra
network draw could change a later disk fault. The engine derives streams from a
seed and named domain, then draws using a stream-local counter. Checkpoint
branches also incorporate their branch identity.

Isolation between domains does not make draw order within a domain irrelevant.
Changing which operations draw, or changing their order, can still change
results. Random policy is owned by the coordinator rather than whichever worker
happens to finish first.

See [Rng and its branch tests](../crates/seedbox-engine-core/src/rng.rs), [link
policy](../crates/seedbox-engine-core/src/links.rs), and [world
coordination](world-coordinator.md).

## Reseeding after a fork

Restoring a kernel snapshot restores its random-generator state too. Changing
only the VMM seed would leave the guest drawing from the parent's stream. The
x86 VMM publishes new entropy and an entropy generation in the shared page.
Kernel hooks consume a changed generation and reseed before producing random
output, including checks for batched values and a timer hook that refreshes the
vDSO random generation.

The update happens while the machine is stopped and does not itself advance time
or unpause it. This is the x86 checkpoint mechanism; ARM arbitrary-time snapshot
restoration remains unsupported.

See [kernel entropy hooks](../kernel/apply.py), `reseed_time_page` in
[Machine](../crates/seedbox-vmm/src/machine.rs), [checkpoint
tests](../crates/seedbox-vmm/src/checkpoint_tests.rs), and [the entropy
fixture](../guest/entropy-fork.init).

## Time and replay inputs retained together

Sampling today's wall clock during preparation would make two fresh runs differ
before the application does anything. By default, Seedbox derives the guest's
wall-clock epoch from the latest creation time in the prepared images, with a
minimum of 86400 seconds. Compose can set an explicit epoch. The supervisor sets
realtime before releasing the application, and replay retains that choice.

Runs also retain their engine and guest inputs. Replaying an old run therefore
does not silently execute the newly installed engine or fetch a changed image.
To compare an engine change, make independent fresh runs with matching inputs
and compare their witnesses and full state; replaying an old retained engine
alone does not test the new one.

See [image-time parsing and tests](../crates/seedbox-application/src/oci.rs), [the
supervisor](../guest/compose-init.c), and [the runtime guide](runtime.md).
Rebuilt images may intentionally change the default epoch.

## Parallel execution within causal windows

Network latency supplies a lower bound on how soon one machine can affect
another. The scheduler uses that bound to give machines immutable assignments
for a round in which newly produced messages cannot be delivered. Workers may
finish in any order. After they all stop, the coordinator sorts outputs by send
time, machine and machine sequence before routing them.

Wake-before-delivery checks prevent stale guest clocks from producing replies
before the round begins. Outputs that violate the causal bound are rejected; a
failed round is not partially committed. Per-machine ARM witnesses are combined
in machine order rather than host execution order.

This depends on the configured latency/slack contract; it is not permission to
run arbitrarily far ahead. See [the scheduler contract](windowed-scheduler.md),
[planner and executors](../crates/seedbox-engine-core/src/window/),
[reversed-completion
tests](../crates/seedbox-engine-core/tests/window_scripted.rs), and [world
boundary tests](../crates/seedbox-engine-core/src/world_windowed_tests.rs).

## Shared pages instead of repeated exits

The x86 guest can publish interrupt masks, network interrupt acknowledgement,
and supported timer arms through shared memory instead of trapping every port or
memory-mapped device access (MMIO). The VMM reconciles these fields at
deterministic stops. Timer arming can lower the guest's running clock bound so
it yields in time.

A watchdog interruption must not republish a plan while the guest is partway
through loading its fields. That would tear one logical transaction into values
from different plans. Shared memory reduces exits only because both sides obey
the publication contract; an arbitrary shared-memory update is not equivalent to
a completed device operation. Other virtio devices retain their own
interrupt-status register paths.

See [kernel fast paths](../kernel/apply.py), [PV plan handling and watchdog
regressions](../crates/seedbox-vmm/src/machine/pv.rs), and timer reconciliation
tests in [Machine](../crates/seedbox-vmm/src/machine.rs).

## Dirty logging only when a fork needs it

An ordinary x86 run need not pay KVM dirty-tracking costs when it will never
restore a snapshot. The first fork point enables logging and copies all RAM
once, establishing a complete baseline. Later restores combine KVM's guest-write
bitmap with explicit journals for VMM writes, such as a virtual network device
copying a received packet into guest RAM (emulated DMA).

KVM's bitmap alone cannot account for host writes. Those journals must retain
every relevant write, and snapshot capture requires a completed I/O boundary. A
reconstructed machine with no baseline performs a full copy before resuming
incremental tracking.

See `hold_fork_point` and restore tests in
[Machine](../crates/seedbox-vmm/src/machine.rs), [the RAM
shadow](../crates/seedbox-vmm/src/shadow.rs), and [checkpoint
tests](../crates/seedbox-vmm/src/checkpoint_tests.rs). This optimization was
added in [PR 243](https://github.com/gardnervickers/seedbox/pull/243).

## Reuse the digest of a verified zero page

Large guest RAM images contain many zero pages. Terminal per-page evidence
compares every byte of a full page against zero and reuses the zero-page digest
only when that comparison succeeds. Nonzero and short trailing pages use
ordinary hashing. The complete RAM digest and the per-page evidence format stay
unchanged.

The distinction matters: an unmarked dirty page is not proof of zero contents.
This optimization verifies the bytes rather than trusting a dirty bitmap or a
cache from an earlier run.

See `ram_page_hashes` and its byte-for-byte comparison test in
[Machine](../crates/seedbox-vmm/src/machine.rs), and [the measured
profile](windowed-scheduler-profile.md).

## Adding another technique

Record the concrete failure or cost, the mechanism, and the invariant a future
cleanup must preserve. Name the platform and assumptions, link the
implementation and the regression that would catch its removal, and state
remaining limits. Keep historical workarounds distinguishable from the current
path. Host test results should link to their evidence and must not be
generalized to untested platforms or configurations.
