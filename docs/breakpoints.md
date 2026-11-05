# Breakpoints

Breakpoints let a program hand control to the oracle at a point it chooses. A
breakpoint only suspends a machine; the oracle decides what to do. They will
replace the oracle's triggers (`arm_trigger`, `cancel_trigger`), which weld a
watch to one fault and report nothing back. Write-to-file breakpoints and
`break_on_block_request` work on PV today; removing triggers is still to come
(see [Order of work](#order-of-work)).

## The model

A program in a machine hits a breakpoint by writing its name to
`/dev/seedbox/break`:

```sh
printf leader-elected > /dev/seedbox/break
```

Any language can do this, and nothing has to be linked in. Outside Seedbox the
device is absent, so instrumented code checks for it once and skips the write.

- If the oracle has declared the name, the machine suspends at the stopped
  rendezvous before the `write` returns. The oracle and the other machines keep
  running. The write returns when the oracle resumes the hit.
- An undeclared name, or any write in a run without an oracle, returns at
  once without an oracle round trip. The `write` is still charged like any
  other syscall.
- Breakpoints are multi-shot: a declared name suspends the machine on every
  hit until the oracle undeclares it. Undeclaring affects only later hits. A
  hit already accepted is still delivered, so two machines can both hit a name
  the oracle meant as one-shot. The second hit is delivered and resumed like
  any other.
- To the guest, a suspension is a VM stall. Simulation time keeps running
  while the machine is suspended, and on resume the machine catches up to the
  current time, as after any stall: overdue timers fire and overdue disk
  requests complete at once. A short handler is a short stall, and a slow one
  can, for example, cost a leader its lease.
- Names are 1 to 64 bytes of `[a-z0-9_.-]`. One trailing newline is dropped,
  so `echo leader > /dev/seedbox/break` works. Any other name fails the write
  with `EINVAL`, and nothing is suspended.

## Hits

Each hit has a `break_id`, a `source` (`program` or `block_request`), its
`name`, the `machine` and its `boot`, and `at_ns`, the projected simulation
time of the hit. A hit becomes visible to the oracle after the round that
contains it commits. Hits are numbered in committed order, not in the order
workers reply, and `wait_break` returns the oldest one not yet returned. Hits
appear as `break_hit` and `break_ended` events.

A hit ends in one of these ways, recorded on its `break_ended` event:

- `resumed`;
- `crashed`, when the oracle crashed the machine;
- `run_ended`, when the oracle exits or the run finishes.

A hit is bound to its machine's boot and suspension. `resume_break` for a hit
that has already ended is refused with `invalid_state`, so it can never resume
a later boot.

When the oracle exits, the run finishes as it does today. Suspended machines
are not resumed and run no further guest work; their hits end with
`run_ended`.

## Oracle calls

| Op | Arguments | Result |
| --- | --- | --- |
| `declare_breaks` | `names` | Replaces the declared set for future hits. `block_request` is reserved for block request hits and refused. |
| `wait_break` | `timeout_ns` | Parks until a hit is pending or the timeout passes. Returns the oldest pending hit, or `{}` on timeout. |
| `resume_break` | `break_id` | The machine runs again and its `write` returns. |
| `break_on_block_request` | `target`, `request`, `nth` | One-shot: holds the target's `nth` disk request of kind `request` (`read`, `write`, `flush`, `any`) unwritten and suspends the machine there. Its hit is named `block_request` and also carries `request` and `nth`; see [faults](faults.md#breakpoint-on-a-block-request). |

`wait_break` parks in Driver state the way `sleep` does. It never blocks the
Driver thread, and its timeout counts from the commit that admitted it.
`inspect` lists each live hit under `breaks`.

The libraries wrap this in `on_break(name, handler)`, in Python's `World` and
Rust's `GuestWorld`. The library declares the union of the handlers' names and
runs a `wait_break` loop on a second connection, on a background thread. It
calls the handler with that connection's world while the machine is suspended,
and resumes the machine when the handler returns, unless the handler crashed
it. A handler that raises stops the loop and leaves its machine suspended; the
error is kept in `break_error`. The nemesis and other oracle calls keep using their
own connections at the same time.

```python
def crash_new_leader(world, hit):
    world.crash(hit.machine)

world.on_break("leader-elected", crash_new_leader)
```

### Several oracle connections

The guest oracle relay accepts up to 8 connections. Each connection is a
session, with at most one request in flight, so a connection's calls stay
sequential. Replies are correlated by session and request ID. The Driver admits
calls from different sessions in committed order, as it does for calls from
different machines.

Two schedules deadlock, and both are oracle bugs: a handler that waits for
something from its own suspended machine, and a handler for A that waits on a
B suspended behind it.

### What the oracle may do to a suspended machine

| Allowed | Refused until resume |
| --- | --- |
| `crash`; network faults that involve it (`partition`, `shape`, `drop_connection`) and their heals | Every other command targeting it, with `invalid_state`: `reboot` (crash it, then reboot it), clock faults, `stop`, `restart`, `pause_process`, `resume_process`, `pause`, `resume`, `throttle` |

## Engine contract

On PV, the guest kernel adds a misc device with fixed minor 243, which the
init creates as `/dev/seedbox/break` (mode 0222). Its `write` copies the name
to bytes 128..192 of the shared page and writes its length to event register
48, on both architectures. The worker stops at that exit, before the syscall
returns. PV is single-vCPU, so the whole machine is suspended.

The worker checks the name against the declared set, which it gets with each
run. An undeclared name continues at once. A declared one ends the run, and the
machine reports `Response::Break {name}` at that guest time, after the run's
other output. The machine stays held, reporting `Blocked` with no deadline,
until `Request::ResumeBreak`, a crash or a reboot. A held machine still takes
its inputs, such as network frames, at the current time; the guest sees them
in order when it runs again.

Suspension is its own state, separate from `pause`: a `resume_break` never
lifts a pause, and a pause can't be applied to a suspended machine.

`declare_breaks` sends the declared set to every machine, and the Driver sends
it again to each spawned machine. The machine keeps it across reboots and
captures it in its snapshot.

TCG has no device register for breakpoints, so the Driver is built without
breakpoint support there and the oracle calls return `unsupported`. The guest
device still exists on TCG, but nothing listens to it.

### Checkpoints and replay

A checkpoint captures the declared set (in the Driver and in each machine),
break ID allocation and parked `wait_break` calls. No checkpoint or fork is
taken while any machine is suspended; declarations and a parked waiter don't
prevent one. On replay, hits and resumes are regenerated by the retained oracle
and never injected as overrides.

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

1. Done: several oracle connections.
2. Done: write-to-file breakpoints, end to end on PV.
3. Done: `break_on_block_request`, with the held request's in-order
   continuation on resume.
4. Remove triggers.
