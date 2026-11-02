# Breakpoints

This is a plan. Nothing here is implemented yet. Breakpoints replace the
oracle's triggers (`arm_trigger`, `cancel_trigger`). A trigger welds a watch to
one fault and reports nothing back. A breakpoint only suspends a machine and
hands the decision to the oracle.

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
- Names are 1 to 64 bytes of `[a-z0-9_.-]`. Any other name fails the write with
  `EINVAL`, and nothing is suspended.

## Hits

Each hit has a `break_id`, a `source` (`program`, or `block_request` with its
watch ID), the `machine` and its `boot`, and `at_ns`, the projected simulation
time of the hit. A hit becomes visible to the oracle after the round that
contains it commits. Hits in one round are numbered in committed
`(at_ns, machine)` order, not in the order workers reply.

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
| `declare_breaks` | `names` | Replaces the declared set for future hits. |
| `wait_break` | `timeout_ns` | Parks until a hit is pending or the timeout passes. Returns the oldest pending hit, or `{}` on timeout. |
| `resume_break` | `break_id` | The machine runs again and its `write` returns. |
| `break_on_block_request` | `machine`, `op`, `nth` | One-shot: holds that request unwritten and suspends the machine. Returns a `watch` ID. |

`wait_break` parks in Driver state the way `sleep` does. It never blocks the
Driver thread. `inspect` lists each suspended machine with its hit.

The libraries wrap this in `on_break(name, handler)`, in Python's `World` and
Rust's `GuestWorld`. The library declares the union of the handlers' names and
runs a `wait_break` loop on its own connection. It calls the handler while the
machine is suspended, and resumes the machine when the handler returns, unless
the handler crashed it. The nemesis and other oracle calls keep using their
own connections at the same time.

```python
def crash_new_leader(world, hit):
    world.crash(hit.machine)

world.on_break("leader-elected", crash_new_leader)
```

### Several oracle connections

Today the guest oracle relay accepts one client and one request in flight
(`guest_oracle.c`), and the host decoder and bridge each keep one pending
request. Breakpoints need that lifted, so step 1 starts here. The relay accepts
up to 8 connections. Each connection is a session, which the frame header
already carries, with at most one request in flight, so a connection's calls
stay sequential. The decoder and bridge keep one pending request per session.
Replies are correlated by session and request ID, and snapshots record the
pending request of each session. The Driver admits calls from different
sessions in committed order, as it already does for calls from different
machines.

Two schedules deadlock, and both are oracle bugs: a handler that waits for
something from its own suspended machine, and a handler for A that waits on a
B suspended behind it.

### What the oracle may do to a suspended machine

| Allowed | Refused until resume |
| --- | --- |
| `crash`; network faults that involve it (`partition`, `shape`, `drop_connection`) and their heals | `reboot` (crash it, then reboot it), clock faults (they need a guest acknowledgement), `stop`, `restart`, `pause_process`, `resume_process`, `pause`, `resume`, `throttle` |

## Engine contract

On PV, the guest kernel adds a misc device, `/dev/seedbox/break`. Its `write`
copies the name into a reserved area of the shared page and signals the
worker. On aarch64 it stores to a new offset in the `EVENT_IPA` window. On
x86_64 it writes a new port, which needs an explicit decoder branch because
unknown ports are ignored today. KVM finishes the port instruction before it
stops. Both stops are the rendezvous before the syscall returns. PV is
single-vCPU, so the whole machine is suspended.

The worker checks the name against the declared set, which the Driver gives
it. An undeclared name returns at once. A declared one is reported as
`Response::Break {name}` at that guest time. The machine stays held until
`Request::ResumeBreak` or a crash.

Suspension is its own state, separate from `pause`. A `resume_break` never
lifts an independent pause, and a pause never lifts a suspension. While a
machine is suspended, the Driver sets incoming frames and inputs aside instead
of queuing them ahead of the resume. It delivers them in order after the
resume, and a crash discards them as it does today.

A held block request reuses this state. `ResumeBreak` on a held request
finishes the interrupted queue processing in order: it executes the held
request and the requests after it in that notification. It does not count the
held request again. At first, each machine has at most one outstanding block
watch, which avoids overlapping counts.

TCG refuses `declare_breaks` and `break_on_block_request` with `unsupported`
until a Seedbox QEMU device register exists, as it does for disk faults. On TCG
the guest device is absent, so writes behave as they do outside Seedbox.

### Checkpoints and replay

The Driver owns all breakpoint state; there is no machine-side mirror. A
checkpoint captures:

- the declared set;
- armed block watches and their counts;
- break and watch ID allocation;
- parked `wait_break` calls;
- pending hits and replies.

No checkpoint or fork is taken while any machine is suspended. Declarations,
an armed but unmatched watch and a parked waiter don't prevent one. Restore
reinstalls the declared set and the watches in each worker before it runs,
including in machines that later reboot or spawn. On replay, hits and resumes
are regenerated by the retained oracle and never injected as overrides.

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

1. Several oracle connections: relay, decoder, bridge and their snapshots.
2. Write-to-file breakpoints, end to end on PV: kernel device, worker,
   suspension state and set-aside inbox, the Driver's hit queue and declared
   set, the oracle calls, and `on_break` in both libraries.
3. Remove triggers.
4. `break_on_block_request`, built on #375's held block request, with the
   continuation on resume.

#375 implements the `block_request` trigger on PV, and step 3 removes the code
it extends. Its device hold and held machine state carry over into step 4.
That step adds the in-order continuation on resume and moves the hold mirror
from the machine to the Driver. Its trigger-table and `arm_trigger` changes are
replaced.
