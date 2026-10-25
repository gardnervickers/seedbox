# Disk, scheduler and trigger faults

These are interfaces only. Every engine refuses `disk_error`, `disk_latency`,
`preempt` and program `trigger` inputs as not implemented yet. A later
change implements them along the contracts below.

## Actions

Each is a healable [control `Action`](../crates/seedbox-engine-protocol/src/control.rs)
on one machine, drawn on its own seeded stream. It survives a guest device
reset and applies to every incarnation until healed.

| Action | Arguments | While active | Heal |
| --- | --- | --- | --- |
| `disk_error` | `op` (`read`, `write`, `flush`, `any`); `ppm`; `effect` (`eio` default, or `corrupt`); `scope` (`all` default, or `data`) | Selected requests fail with EIO before any disk effect, or succeed with one sector of seeded bytes. `data` spares the metadata ext4 lays out at format time, not metadata it allocates later; neither `corrupt` nor `data` selects flushes | Stops selecting; settles with `matched` and `affected` counts |
| `disk_latency` | `op`; `latency_ns` 1 µs–1 s; `jitter_ns` up to 1 s | Requests complete late, in submission order, with their disk effect at completion; a crash loses the pending ones | Stops delaying; queued requests still complete at their times; settles with counts |
| `preempt` | `ppm` | The guest kernel yields at that rate of syscalls and progress charges | Restores ordinary scheduling |

A disk fault's counts cover every boot it applied to and arrive on its heal's
settlement; a preempt heal has none.
## Typed triggers

The oracle's `arm_trigger` call already arms console-marker triggers that stop
an application or partition machines. A program `trigger` input states the
same rule in general and arms when due:

| Field | Meaning |
| --- | --- |
| `when` | A selector tagged `kind`: `output` (a marker, as a string or `{hex: ...}`, in a machine's committed output, as `arm_trigger` matches), `lifecycle` (`ready`, `exited`, `crashed`, `rebooted`, `paused`, `resumed`), `connection_open`, `frame`, or `block_request` |
| `nth` | The occurrence that matches, from 1 |
| `delay_ns` | Added to the matching barrier before the effect is admitted |
| `within_ns` | Occurrences at or after this never match and the trigger expires; without it the run's end cuts it off |
| `then` | Any action, admitted as an ordinary fault with its own ID and the same checks as a timed input |

A `block_request` trigger holds the matching disk request at the device, so
its only effect is an undelayed `crash` of that machine. Selectors on SDK
evidence (`event`, `assertion`, `setup_complete`) are reserved for a later
change.
Oracle-armed and program triggers share one lifecycle and 64 IDs per run, so a
program alone may arm at most 64. The lifecycle is armed, matched, then fired,
rejected, cancelled, expired or cut off. Each change of a program trigger is a
`triggerChanged` event, and the world view keeps every program trigger's
latest state (oracle-armed triggers keep their arm and resolution events); a
fired trigger names its fault, whose own status says whether the effect
applied. A consumed trigger input leaves an arm receipt in the program cursor,
beside a slot for that trigger's matching progress (its count and partial
marker match), so a checkpoint continues the same match.

## In the timeline

`seedbox.yaml` declares no faults, so neither it nor the CLI's flags change.
The new faults reach a run as a definition's `overrides` (a submission's
`program`) or one at a time through `seedbox control inject`. The oracle's
guest RPC carries none of them: its operations are unchanged. Each new action
is an ordinary override; a trigger is one too, but only in a program, since it
allocates a trigger ID rather than a fault ID:

```yaml
overrides:
  - {id: slow-fsync, atNs: 1000000000,
     action: {action: disk_latency, handle: slow, machine: db, op: flush, latency_ns: 20000000}}
  - {id: reboot-db, atNs: 0,
     action: {action: trigger, when: {kind: lifecycle, machine: db, transition: crashed},
              then: {action: reboot, machine: db}}}
```

Submission checks each entry's arguments, and that a program arms no more
triggers than the run's budget, then refuses it with the API's
`notImplemented` error (HTTP 501), naming it. A fault preview or an injected
fault is refused the same way, before it is journaled.

## Where each fault runs

| Input | Needs | aarch64 PV | x86_64 PV | x86_64 TCG |
| --- | --- | --- | --- | --- |
| `disk_error` | disk request selection | not yet | not yet | refused |
| `disk_latency` | delayed block completion | not yet | not yet | refused |
| `preempt` | the guest yield hook | not yet | refused | refused |
| `trigger` on `block_request` | held block requests | not yet | not yet | refused |
| any other `trigger` | its effect's needs | not yet | not yet | not yet |
| every other action | nothing | yes | yes | yes |

The [Driver](../crates/seedbox-driver/src/driver/faults.rs) keys each
capability by backend and architecture, and its worker refuses an input its
engine lacks, naming both: a program when it opens, before any machine boots,
and an injected fault when it is prepared. Every engine also refuses the
inputs marked "not yet" as not implemented yet, at submission and again
there.

## Engine contracts

### Disk request selection

Both backends execute a virtio-blk request through the shared
[request](../crates/seedbox-machine/src/machine/disk/request.rs) code, so
selection belongs beside it, once. Each admitted request is offered to the
active disk faults before `request::execute`. A fault matches by `op`, then by
`scope`. `data` uses only the static ext4 layout, a pure function of the
disk's current bytes read when a request needs it: superblocks and their
backups, group descriptors and reserved GDT blocks, bitmaps, inode tables and
the journal. The host keeps no live filesystem model, so metadata allocated
later (directory, extent-tree and xattr blocks) counts as data and can be
selected. A match draws once on `disk-error/<boot>`, keyed by the fault's
seed, which the Driver derives from the machine's seed and the fault ID. A hit
with `eio` completes with IOERR without touching the store or the data
buffers. A hit with `corrupt` picks a sector of the request on the same stream
and replaces its bytes: in the data returned for a read, in the bytes stored
for a write. Heal settles with `FaultCounts` (matched, affected). The policy
and draw counters belong to the device snapshot, survive a guest device reset,
and apply again to a rebooted incarnation on a fresh stream.

A `block_request` trigger installs a hold that counts matching requests the
same way. On the `nth`, earlier requests in that notification complete; the
held one has no effect, used-ring entry or interrupt, and nothing after it is
processed. The MMIO write completes and the machine stops in a held state: it
neither runs nor receives, and its queued inputs stay unconsumed. The Driver
records the match there and admits the crash at that barrier, so the crash
time never depends on where rounds end. No checkpoint is captured while a
request is held.

### Asynchronous block completion

Today a notification completes every available request before it returns
(see [disk state](internals.md#disk-state-at-boot-end)). Under `disk_latency`
the notification instead consumes each request from the avail ring and queues
it with a due time: the machine time of the notifying write, plus the fault's
latency and a draw up to its jitter on
`disk-latency/<boot>` under the fault's seed, and never before the request
ahead of it, so completion order is submission order. A request the fault does
not select still queues behind a delayed one.

At its due time a request is decoded again from guest memory, which the guest
driver may not touch before completion, and then has its disk effect, data,
status, used entry and interrupt exactly as an immediate completion would. A
FLUSH therefore completes after the writes queued before it. The earliest due
time is a machine deadline like the guest timer: the guest stops there, an
idle guest wakes there, and a stopped machine reports it as its next event.
Host time never selects a completion. The queue is snapshot state. A crash,
reboot or guest device reset drops it: those requests were never acknowledged,
and boot-end disk policy then applies to the store as before. Heal stops
delaying new requests; queued ones complete at their times. With no active
fault and an empty queue, requests complete at notification, so existing runs
keep their bytes.

### Guest yield hook

`preempt` uses two host-owned words of PV ABI 3's shared page: word 88 holds
the rate in parts per million and word 96 the seed; zero means off. Both
kernels already call `seedbox_syscall_progress` at syscall entry, in process
context, and DynamoRIO's progress client charges through syscalls too. Only
the ARM kernel's hook yields: the x86 kernel has no yield yet, so x86_64 PV
refuses `preempt`. With a nonzero rate the hook draws one step of a splitmix64
stream kept in kernel memory, reseeded when the seed word changes, and calls
`yield()` on a hit after charging the syscall. The draw and the yield charge
no time. The stream is guest memory, so checkpoints continue it. The machine
derives each boot's seed from the fault's seed and `preempt/<boot>`, writes
both words at a stopped rendezvous, writes them again for a rebooted
incarnation, and clears them on heal.

### Where each backend changes

PV runs both architectures through the same block device and worker, so one
change implements the disk contracts for PV on aarch64 and x86_64; only the ARM
kernel's syscall hook yields, so the yield hook is aarch64 only. TCG, the
reference machine, needs QEMU changes and keeps refusing these inputs until a
later change makes them.

| Contract | PV | TCG (later) |
| --- | --- | --- |
| Request selection and counts | `process` in [block.rs](../crates/seedbox-machine-pv/src/machine/pv/worker/block.rs), policy from a machine request | `seedbox_blk_handle` in [seedbox-disk.patch](../crates/seedbox-machine-tcg/qemu/seedbox-disk.patch), policy by a QMP command and a new `abi.rs` entry |
| Held request | the same loop, plus a held stop in the worker | the same handler, stopping the vCPU as a device error does |
| Asynchronous completion | the queue in block.rs; the earliest due time folded into `BOUND` and `Stop::Blocked` in [the worker](../crates/seedbox-machine-pv/src/machine/pv/worker/mod.rs); the queue in its snapshot | a `QEMU_CLOCK_VIRTUAL` timer for the earliest due time, which icount and idle already honour; the queue in the device's vmstate |
| Yield hook | page words 88 and 96; `seedbox_syscall_progress` in the [ARM](../crates/seedbox-machine/guest/kernel/arm64/seedbox.c) kernel; the [x86](../crates/seedbox-machine/guest/kernel/x86_64/seedbox.c) kernel's has no yield yet | no PV page: it would need a register in a Seedbox QEMU device and a kernel hook that reads it |
