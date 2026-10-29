# Disk and scheduler faults

PV machines hold a disk request for the oracle's `block_request` trigger. No
engine carries out `disk_error`, `disk_latency` or `preempt` yet, so every
engine refuses them, naming what it lacks. The contracts below state how PV
implements them.

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
settlement (`counts` on `faultChanged` and in the world view); a preempt heal
has none.

## Crash on a held block request

The oracle's [`arm_trigger`](../crates/seedbox-driver/src/oracle.rs) call arms
a trigger on a machine's committed output that stops an application or
partitions machines. Instead of a `marker`, it can take
`block_request: {op, nth}`: the source machine's `nth` disk request of kind
`op` after arming. The device holds that request unwritten, the trigger matches
at the commit that reports the hold, and its only fault is an undelayed crash
of the same machine:

```json
{"op": "arm_trigger", "source": {"machine": "db"},
 "block_request": {"op": "flush", "nth": 20}, "match_timeout_ns": 10000000000,
 "fault": {"action": "crash", "target": {"machine": "db"}}}
```

It shares the oracle triggers' budget (64 per run), match timeout,
cancellation and lifecycle, and appears in `inspect` with its
`block_request` in place of the marker. An expired or cancelled trigger
releases its hold. The Python client arms it with
`world.arm_block_request_crash(target, op, nth, match_timeout=...)`.
No trigger watches guest property evidence or other application state: the
run is deterministic, so an oracle watches for such a condition itself and
applies the fault then.

## In the timeline

`seedbox.yaml` declares no faults, so neither it nor the CLI's flags change.
The new faults reach a run as a definition's `overrides` (a submission's
`program`) or one at a time through `seedbox control inject`. The oracle's
guest RPC carries none of the three actions: its operations are unchanged
apart from the `block_request` trigger.

```yaml
overrides:
  - {id: slow-fsync, atNs: 1000000000,
     action: {action: disk_latency, handle: slow, machine: db, op: flush, latency_ns: 20000000}}
  - {id: fast-fsync, atNs: 2000000000, action: {action: heal, handle: slow}}
```

Submission checks each entry's arguments, then compiles its targets, which
refuses an action no engine provides as an invalid request naming what it
needs. A fault preview is refused the same way. An injected fault's arguments
are checked before it is journaled.

## Where each fault runs

| Input | Needs | aarch64 PV | x86_64 PV | x86_64 TCG |
| --- | --- | --- | --- | --- |
| `disk_error` | disk request selection | refused | refused | refused |
| `disk_latency` | delayed block completion | refused | refused | refused |
| `preempt` | the guest yield hook | refused | refused | refused |
| `block_request` trigger | held block requests | yes | yes (type-checked only) | refused |
| every other action and trigger | nothing | yes | yes | yes |

The [Driver](../crates/seedbox-driver/src/driver/faults.rs) keys each
capability by backend and architecture. Its worker refuses an input its engine
lacks, naming both ("disk_error needs disk request selection, which x86_64 TCG
does not provide"): a program when it opens, before any machine boots, and an
injected fault when it is prepared. The oracle's `block_request` trigger is
refused with `unsupported` where its engine cannot hold requests. The
implementation provides the three actions on PV too, except `preempt` on
x86_64, whose kernel has no yield yet; TCG follows later.

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
