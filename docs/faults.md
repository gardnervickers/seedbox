# Disk and scheduler faults

PV machines carry out `disk_error`, `disk_latency`, `preempt` and the
oracle's `break_on_block_request` along the contracts below. The oracle applies
and heals the three faults through its RPC, and a run's timeline can apply
them too. TCG refuses all four and x86_64 PV refuses `preempt` (see
[where each fault runs](#where-each-fault-runs)).

## Actions

Each is a healable [control `Action`](../crates/seedbox-engine-protocol/src/control.rs)
on one machine, drawn on its own seeded stream. It survives a guest device
reset and applies to every incarnation until healed.

| Action | Arguments | While active | Heal |
| --- | --- | --- | --- |
| `disk_error` | `request` (`read`, `write`, `flush`, `any`); `ppm`; `effect` (`eio` default, or `corrupt`); `scope` (`all` default, or `data`) | Selected requests fail with EIO before any disk effect, or succeed with one sector of seeded bytes. `data` spares the metadata ext4 lays out at format time, not metadata it allocates later; neither `corrupt` nor `data` selects flushes | Stops selecting; settles with `matched` and `affected` counts |
| `disk_latency` | `request`; `latency_ns` 1 µs–1 s; `jitter_ns` up to 1 s | Requests complete late, in submission order, with their disk effect at completion; a crash loses the pending ones | Stops delaying; queued requests still complete at their times; settles with counts |
| `preempt` | `ppm` | The guest kernel yields at that rate of syscalls and progress charges. One per machine | Restores ordinary scheduling |

Disk faults stack: each selects on its own stream and the first hit acts.
A disk fault's counts cover every boot it applied to and arrive on its heal's
settlement (`counts` on `faultChanged` and in the world view, and in
`seedbox observe`) and in the oracle's heal reply; a preempt heal has none.

## From the oracle

The oracle's RPC ([schema](../crates/seedbox-driver/src/oracle.rs)) has an
operation for each, named after its action. It takes the action's arguments
and a `target`, as `throttle` does, and replies with a handle that `heal`
ends:

```json
{"op": "disk_error", "target": {"machine": "db"}, "request": "read", "ppm": 1000000, "scope": "data"}
{"op": "disk_latency", "target": {"machine": "db"}, "request": "flush", "latency_ns": 20000000}
{"op": "preempt", "target": {"machine": "db"}, "ppm": 100000}
{"op": "heal", "handle": "p1"}
```

`effect`, `scope` and `jitter_ns` are optional, with the action's defaults.
The action's own validator checks the arguments first (`invalid_request`).
The engine is checked next: one that lacks the fault refuses it as
`unsupported`, naming what it lacks ("preempt needs the guest yield hook,
which x86_64 PV does not provide"). Admission then follows the action's rules:
disk faults stack, and a second `preempt` on a machine is `invalid_state`.
As for every oracle fault, the target is another machine: the oracle's own
is outside the failure model. The reply comes once the machine has installed
the fault. A disk fault's heal replies with its counts,
`{"matched": ..., "affected": ...}`. The Python client has
`world.disk_error(target, request, ppm, effect=..., scope=...)`,
`world.disk_latency(target, request, latency_ns, jitter_ns=...)` and
`world.preempt(target, ppm)`, each returning a handle for `world.heal`, which
returns a disk fault's counts, as Rust's `GuestWorld::heal` does.

## Breakpoint on a block request

The oracle's `break_on_block_request` call suspends a machine at its `nth`
disk request of kind `request` (`read`, `write`, `flush` or `any`) from now
on, counted across boots. The device holds that request unwritten and the
machine is suspended there, as at a [breakpoint](breakpoints.md). The hit
arrives through `wait_break` with `source` and `name` `block_request`, and the
call's `request` and `nth`. The oracle decides what happens: crash the machine
while the request is still unwritten, or resume it and let the request
continue.

```json
{"op": "break_on_block_request", "target": {"machine": "db"},
 "request": "flush", "nth": 20}
```

It is one-shot and has no identity: the reply is empty, and the hit carries
the call's `request` and `nth`. There is no cancel. A hold that has not
reached its `nth` stays on the machine across boots, and a `wait_break` that
times out does not remove it. Holds that reach their `nth` on the same request
share one hit, which carries the earliest call's `request` and `nth`: after
`break_on_block_request(db, any, 5)` and then `(db, flush, 2)`, a FLUSH that
is both the fifth request and the second flush gives one hit with `request`
`any` and `nth` 5, and both holds are gone.

The Python client asks for it with
`world.break_on_block_request(target, request, nth)` and serves the hit with
`world.on_break("block_request", handler)`; Rust's `Control` has the same
call. The handler's hit has `source`, `request` and `nth` (`hit.source`,
`hit.request` and `hit.nth` in Python, `Hit::block_request` in Rust). No
program can declare a breakpoint named `block_request`, so a hit with that
name is always a held request, and `on_break("block_request", ...)` declares
nothing. No breakpoint watches guest property evidence or other application
state: the run is deterministic, so an oracle watches for such a condition
itself and applies the fault then.

## In the timeline

`seedbox.yaml` declares no faults, so neither it nor the CLI's flags change.
Besides the oracle's calls, the faults reach a run as a definition's
`overrides` (a submission's `program`) or one at a time through
`seedbox control inject`.

```yaml
overrides:
  - {id: slow-fsync, atNs: 1000000000,
     action: {action: disk_latency, handle: slow, machine: db, request: flush, latency_ns: 20000000}}
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
| `break_on_block_request` | breakpoints | yes | yes (type-checked only) | refused |
| every other action and trigger | nothing | yes | yes | yes |

The [Driver](../crates/seedbox-driver/src/driver/faults.rs) keys each
capability by backend and architecture. Before it launches the worker, the
run refuses a program input its engine lacks with a setup error naming the
input, the engine and what it lacks ("input race-on: preempt needs the guest
yield hook, which x86_64 PV does not provide"); the worker checks again when
it opens and refuses an injected fault when it is prepared. The oracle's
fault calls are refused with `unsupported` naming what the engine lacks, and,
like every breakpoint call, `break_on_block_request` is refused with
`unsupported` where the engine has no breakpoints. x86_64 PV shares the
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
`request`, then by `scope`. `data` uses only the static ext4 layout, which the
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

`break_on_block_request` installs a hold that counts matching requests the
same way, across boots. On the `nth`, earlier requests in that notification
are taken as usual, completing or queuing behind a delay; the held one has no effect, used-ring entry or interrupt, stays
unconsumed in the avail ring, and nothing after it is processed. It has
already drawn its latency and counts for that latency fault, even if the
machine is then crashed at the hit, and later holds have counted it. The MMIO write
completes and the machine stops held, exactly as at a breakpoint write: it
reports the hold as an output, takes inputs without running, and refuses
capture. The Driver numbers the hit at the barrier that commits that output.
On `resume_break` the request continues when the machine takes the release,
before the guest runs again, with the arrival it drew: it completes then if
its due time, counted from its original arrival, has passed, and otherwise at
that due time. The requests behind it follow, and one of them may reach
another hold before the guest runs. A crash or reboot drops it with the rest
of the boot. Requests the device took before the held one keep the due
times they drew at arrival. While the machine is suspended, one that falls
due completes the next time the machine admits an input, such as a network
frame or a fault, or at the release, and the guest sees it when it runs.

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
| Held request | the same loop, plus a held stop in the worker and `Block::resume` | the same handler, stopping the vCPU as a device error does |
| Asynchronous completion | the queue in block.rs; the earliest due time folded into `BOUND` and `Stop::Blocked` in [the worker](../crates/seedbox-machine-pv/src/machine/pv/worker/mod.rs); the queue in its snapshot | a `QEMU_CLOCK_VIRTUAL` timer for the earliest due time, which icount and idle already honour; the queue in the device's vmstate |
| Yield hook | page words 88 and 96; `seedbox_syscall_progress` in the [ARM](../crates/seedbox-machine/guest/kernel/arm64/seedbox.c) kernel; the [x86](../crates/seedbox-machine/guest/kernel/x86_64/seedbox.c) kernel still needs it | no PV page: it would need a register in a Seedbox QEMU device and a kernel hook that reads it |
