# Disk and scheduler faults

PV machines carry out `disk_error`, `disk_latency`, `preempt` and the
oracle's `block_request` trigger along the contracts below. TCG refuses all
four and x86_64 PV refuses `preempt` (see
[where each fault runs](#where-each-fault-runs)).

## Actions

Each is a healable [control `Action`](../crates/seedbox-engine-protocol/src/control.rs)
on one machine, drawn on its own seeded stream. It survives a guest device
reset and applies to every incarnation until healed.

| Action | Arguments | While active | Heal |
| --- | --- | --- | --- |
| `disk_error` | `op` (`read`, `write`, `flush`, `any`); `ppm`; `effect` (`eio` default, or `corrupt`); `scope` (`all` default, or `data`) | Selected requests fail with EIO before any disk effect, or succeed with one sector of seeded bytes. `data` spares the metadata ext4 lays out at format time, not metadata it allocates later; neither `corrupt` nor `data` selects flushes | Stops selecting; settles with `matched` and `affected` counts |
| `disk_latency` | `op`; `latency_ns` 1 µs–1 s; `jitter_ns` up to 1 s | Requests complete late, in submission order, with their disk effect at completion; a crash loses the pending ones | Stops delaying; queued requests still complete at their times; settles with counts |
| `preempt` | `ppm` | The guest kernel yields at that rate of syscalls and progress charges. One per machine | Restores ordinary scheduling |

Disk faults stack: each selects on its own stream and the first hit acts.
A disk fault's counts cover every boot it applied to and arrive on its heal's
settlement (`counts` on `faultChanged` and in the world view, and in
`seedbox observe`); a preempt heal has none.

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

Submission checks each entry's arguments and machine, and an injected
fault's arguments are checked before it is journaled.

## Where each fault runs

| Input | Needs | aarch64 PV | x86_64 PV | x86_64 TCG |
| --- | --- | --- | --- | --- |
| `disk_error` | disk request selection | yes | yes (type-checked only) | refused |
| `disk_latency` | delayed block completion | yes | yes (type-checked only) | refused |
| `preempt` | the guest yield hook | yes | refused | refused |
| `block_request` trigger | held block requests | yes | yes (type-checked only) | refused |
| every other action and trigger | nothing | yes | yes | yes |

The [Driver](../crates/seedbox-driver/src/driver/faults.rs) keys each
capability by backend and architecture. Before it launches the worker, the
run refuses a program input its engine lacks with a setup error naming the
input, the engine and what it lacks ("input race-on: preempt needs the guest
yield hook, which x86_64 PV does not provide"); the worker checks again when
it opens and refuses an injected fault when it is prepared. The oracle's
`block_request` trigger is refused with `unsupported`. x86_64 PV shares the
block device and worker with aarch64 but has only been type-checked; its
kernel has the syscall hook point but no yield yet.

## Engine contracts

### Disk request selection

Both backends execute a virtio-blk request through the shared
[request](../crates/seedbox-machine/src/machine/disk/request.rs) code, and the
shared [fault policy](../crates/seedbox-machine/src/machine/disk/fault.rs)
beside it selects. The Driver installs a disk fault in its machine as a
request carrying the handle record's identity and a seed it derives from the
machine and the fault ID, so admission draws nothing. A fault matches by
`op`, then by `scope`. `data` uses only the static ext4 layout, which the
[reader](../crates/seedbox-machine/src/machine/disk/ext4.rs) computes from the
disk's current bytes whenever a request needs it: superblocks and their
backups, group descriptors and reserved GDT blocks, bitmaps, inode tables and
the journal. Before the superblock exists nothing is data; metadata ext4
allocates later counts as data. A match draws once on that boot's
`disk-error/<boot>` stream of the fault's seed. A hit with `eio` completes
with IOERR without touching the store or the data buffers. A hit with
`corrupt` draws a sector of the request and its bytes on the same stream and
replaces it: in the data returned for a read, in the bytes stored for a
write; it counts as affected only if the request then completes. The
machine keeps its faults across device resets and boots, starts each boot's
streams afresh and adds each boot's counts.

A `block_request` trigger installs a hold that counts matching requests the
same way, across boots, until its match deadline in the machine's time. On the
`nth`, earlier requests in that notification complete; the held one has no
effect, used-ring entry or interrupt, and nothing after it is processed. The
MMIO write completes and the machine stops held: it reports the hold as an
output, neither runs nor takes inputs other than Crash, Reboot and fault
requests, and refuses capture. The Driver records the match at the barrier
that commits that output and admits the crash there. The held machine does
nothing in between, so its state does not depend on where rounds end.

### Asynchronous block completion

Without a latency fault a notification completes every available request
before it returns (see [disk state](internals.md#disk-state-at-boot-end)).
Under `disk_latency` the notification instead consumes each request from the
avail ring and queues it with a due time: the machine time of the notifying
write, plus the fault's latency and a draw up to its jitter on
`disk-latency/<boot>`, and never before the request ahead of it, so
completion order is submission order. A request the fault does not select
still queues behind a delayed one.

At its due time a request is decoded again from guest memory, which the guest
driver may not touch before completion, and then has its disk effect, data,
status, used entry and interrupt exactly as an immediate completion would. A
FLUSH therefore completes after the writes queued before it. The earliest due
time is a machine deadline like the guest timer: it bounds the published
`BOUND`, an idle guest wakes there, and a blocked machine reports it as its
next event. Requests complete at the first rendezvous at or after their time,
recorded as ordinary completions in the execution witness. Host time never
selects a completion. The queue is snapshot state of the transport. A crash,
reboot or guest device reset drops it: those requests were never
acknowledged, and boot-end disk policy then applies to the store as before.
With no fault installed and an empty queue, requests complete at
notification, so existing runs keep their bytes.

### Guest yield hook

`preempt` uses two host-owned words of PV ABI 3's shared page: word 88 holds
the rate in parts per million and word 96 the seed; zero means off. The ARM
kernel's `seedbox_syscall_progress` runs at syscall entry, in process
context, and DynamoRIO's progress client charges through syscalls too. With a
nonzero rate the hook draws one step of a splitmix64 stream kept in kernel
memory, reseeded when the seed word changes, and calls `yield()` on a hit after
charging the syscall. The draw and the yield charge no time. The stream is
guest memory, so checkpoints continue it. The worker derives each boot's seed
word from the fault's seed and the boot, writes both words when the fault is
installed at a stopped rendezvous and again for every new boot, and clears
them on heal.

### Where each backend changes

| Contract | PV | TCG (later) |
| --- | --- | --- |
| Request selection and counts | `process` and `complete` in [block.rs](../crates/seedbox-machine-pv/src/machine/pv/worker/block.rs) over the shared policy; the machine keeps faults and counts across boots | `seedbox_blk_handle` in [seedbox-disk.patch](../crates/seedbox-machine-tcg/qemu/seedbox-disk.patch), policy by a QMP command and a new `abi.rs` entry |
| Held request | the same loop, plus a held stop in the worker | the same handler, stopping the vCPU as a device error does |
| Asynchronous completion | the queue in block.rs; the earliest due time folded into `BOUND` and `Stop::Blocked` in [the worker](../crates/seedbox-machine-pv/src/machine/pv/worker/mod.rs); the queue in its snapshot | a `QEMU_CLOCK_VIRTUAL` timer for the earliest due time, which icount and idle already honour; the queue in the device's vmstate |
| Yield hook | page words 88 and 96; `seedbox_syscall_progress` in the [ARM](../crates/seedbox-machine/guest/kernel/arm64/seedbox.c) kernel; the [x86](../crates/seedbox-machine/guest/kernel/x86_64/seedbox.c) kernel still needs it | no PV page: it would need a register in a Seedbox QEMU device and a kernel hook that reads it |
