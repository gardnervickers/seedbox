# Breakpoints

This is a plan. Nothing here is implemented yet. Breakpoints replace the
oracle's triggers (`arm_trigger`, `cancel_trigger`): a trigger welds a watch to
one fault and reports nothing back, while a breakpoint only suspends a machine
and hands the decision to the oracle.

## The model

A program in a machine hits a breakpoint by writing its name to
`/dev/seedbox/break`:

```sh
printf leader-elected > /dev/seedbox/break
```

Any language can do this, and nothing needs to be linked in. Outside Seedbox
the device is absent, so instrumented code checks for it once and skips the
write.

- If the oracle has declared the name, the write suspends the whole machine,
  every vCPU and its clock, at that instruction. The oracle and the other
  machines keep running. The write returns when the oracle resumes the hit.
- An undeclared name, or any write in a run without an oracle, returns at
  once. It costs no simulated time and no oracle round trip.
- Breakpoints are multi-shot: a declared name suspends the machine on every
  hit until the oracle undeclares it. A one-shot breakpoint is a handler that
  undeclares its name while its machine is still suspended, so the machine
  cannot slip past it.
- While a machine is suspended, the oracle may crash or reboot it, or apply
  network, clock and disk faults to it. Calls that need the guest to run
  (application `stop`, `pause_process`, `restart`) are refused until it
  resumes. A crash means the write never returns.
- When the oracle exits, every suspended machine resumes.

Names are 1 to 64 bytes of `[a-z0-9_.-]`. Writing any other name is a program
error: the write fails with `EINVAL` and nothing is suspended.

## Oracle calls

| Op | Arguments | Result |
| --- | --- | --- |
| `declare_breaks` | `names` | Replaces the declared set. A hit before the declaration passes through. |
| `wait_break` | none | Blocks until a declared hit, then returns `{break_id, machine, name, at_ns}`. |
| `resume_break` | `break_id` | The write returns and the machine runs again. |
| `break_on_block_request` | `machine`, `op`, `nth` | One-shot: holds that request unwritten and suspends the machine. The hit is named `block_request`. |

`inspect` lists each suspended machine with its `break_id` and name. A hit is
published as a `break_hit` event and its end as `break_resumed`, both with the
`break_id`.

The oracle libraries wrap these calls in `on_break(name, handler)`, in Python's
`World` and Rust's `GuestWorld`. The library declares the union of the
handlers' names, runs `wait_break` on its own connection, calls the handler
while the machine is suspended, and resumes the machine when the handler
returns, unless the handler crashed or rebooted it.

```python
def crash_new_leader(world, hit):
    world.crash(hit.machine)

world.on_break("leader-elected", crash_new_leader)
```

## Engine contract

On PV, the guest kernel adds a misc device, `/dev/seedbox/break`. Its `write`
copies the name into a reserved area of the shared page. On x86_64 it then
signals the worker through a new port, the same way `event_signal` does. On
aarch64 it stores to a new offset in the `EVENT_IPA` window. Either way, the
worker is at a stopped rendezvous. If the name is not in the declared set,
which the Driver hands the worker, the worker clears the word and the write
returns. Otherwise the worker reports `Response::Break {id, name}` at that
guest time and stays stopped in the held state that #375 introduces for block
requests. In that state the machine neither runs nor receives, and no
checkpoint is captured. `Request::ResumeBreak {id}` leaves the held state. The
declared set is Driver state, so it is checkpointed and replayed with the run.

A held block request is the same hit from a different source: the virtio-blk
loop holds the request and reports `Break` instead of `BlockRequestHeld`.

TCG refuses `declare_breaks` and `break_on_block_request` with `unsupported`
until a Seedbox QEMU device register exists, as it does for disk faults. On
TCG the guest device is absent, so writes behave as they do outside Seedbox.

Symbol breakpoints (`module!symbol`) are a later addition. They would go
through the DynamoRIO client, which already sees every block before it runs,
so they need no hypervisor debug registers. Their hits would have the same
shape.

## What is removed

Triggers are removed entirely, with no compatibility path:

- `arm_trigger`, `cancel_trigger` and `inspect().triggers`
- `oracle/triggers.rs`, `oracle/service/trigger.rs`, and the console marker
  feed in `driver/control.rs`
- `TriggerArmed`, `TriggerResolved`, `TriggerResult`, `FaultOrigin::Trigger`
  and `AdmissionMode::Trigger`
- `trigger_marker` and the `TriggerBoundary`/`TriggerFinalize` control events
- in Python: `arm_console_stop`, `arm_console_partition`, `trigger_partition`,
  `cancel_trigger` and `Trigger`
- the console-trigger acceptance workload
  (`harness/acceptance/workloads/triggers.yaml`) and its driver tests

A console marker becomes an explicit breakpoint written by the program. A
fault the oracle used to arm ahead of time, it now applies in its handler.

## Order of work

1. Write-to-file breakpoints, end to end on PV: kernel device, worker, machine
   protocol, Driver hold and declared set, the oracle calls, and `on_break` in
   both libraries.
2. Remove triggers.
3. `break_on_block_request`, built on #375's held block request.

#375 implements the `block_request` trigger on PV, and step 2 removes the code
it extends. Its device hold, held machine state and machine requests carry over
into steps 1 and 3 unchanged. Only its trigger-table and `arm_trigger`
changes are replaced.
