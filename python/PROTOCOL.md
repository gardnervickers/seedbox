# Seedbox guest control RPC, version 1

This is the public language-neutral interface for runtime fault operations.
Python's `seedbox.World` is one binding over this interface. A scenario executes
inside a simulated guest and issues commands as it runs; no timestamp list is
compiled in advance. Replay starts the retained scenario image again and compares
the resulting requests, effects and replies.

## Controller and transport

Select one controller service in Compose:

```yaml
x-seedbox:
  version: 1
  control: {service: scenario}
  check: {service: scenario, exit_code: 0}
  # ...seed, limits, network and schedule settings...
```

The control service must also be the check service, so another application cannot
complete the run before the scenario finishes. The controller is outside the fault
model. Stop/restart and machine lifecycle operations cannot target it, and partition
groups cannot contain it. Its control transport and network links
remain available during cuts between other guests. A stopped application still
cannot answer application requests; restart confirmation means successful exec,
so the scenario must check application readiness separately. Normal controller
completion supplies the configured check outcome.

Connect to the Unix stream socket `/run/seedbox/control.sock` inside the guest.
The socket is created only in the configured controller guest, selected by the
VMM's `seedbox.control=1` kernel boot argument. It is absent from other guests.
Version 1 supports one active controller connection and one outstanding request
at a time. After receiving a complete response, the client may close its
connection and open a new session. Handles and simulator fault state survive
these graceful disconnects. Other language bindings need only a Unix socket and JSON.
The guest supervisor relays these messages through a named virtio-console port,
`seedbox`. Its transmit and receive queues use guest RAM shared with the simulator;
a device notification commits the submitted bytes. Shared memory sits at the
guest-driver/simulator boundary; user processes use socket reads and writes.
This channel is independent
of simulated Ethernet. No host socket, shared host filesystem, or host clock is
involved. The generic virtio channel is available on every prepared guest; the runtime
fault socket and operation authority belong only to the configured controller. Further message
families can use the same transport for application instrumentation.

Each request and response is one UTF-8 JSON object followed by a newline, at most
16384 bytes including the newline. Embedded newlines must be JSON-escaped.
Do not pipeline calls or reconnect and retry an uncertain operation. A disconnect
with a partial request or an outstanding RPC is an infrastructure error. There
are no automatic retries. Request IDs are positive unsigned 32-bit integers,
starting at 1 and increasing by one within each connection. A new connection
starts again at ID 1. Python callers close the old `World` and create a new one
with `World.connect()`; a closed `World` never reconnects itself.
The response echoes the version and ID. The API has no implicit retry, automatic
heal, or garbage-collection-based operation.

Version 1 request operations/arguments are strict; unknown fields are rejected.
The Python binding validates required response fields, version, IDs and simulated
timing while tolerating unrelated response detail keys. Such keys do not permit
changing required meanings or make a new request operation supported by an old
peer. Version compatibility remains a release decision; see
[current draft formats](../docs/compatibility.md). Application output and optional
user files remain opaque and are not RPC response schemas.

The private supervisor-to-VMM relay framing is not part of the public socket
protocol; custom bindings do not implement virtqueue or device I/O. Private frames
carry a run-wide relay ID and a connection session ID before the public JSON:
`SBX_RPC1 <relay> <session> <JSON>` and `SBX_REPLY1 <relay> <session> <JSON>`.
Both IDs are positive unsigned 32-bit integers. Relay IDs increase by one for
every submitted RPC across sessions. Session IDs increase for every accepted
connection, so empty connections may leave gaps in the recorded session IDs.
The VMM records `session_id` in the transcript, validates public IDs within that
session, and echoes both private IDs; the supervisor rejects replies for another
session even if their public ID matches. The device
uses the [virtio console specification](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#x1-2850003),
with a named non-console port and shared-memory split queues. Its bounded buffers
and notifications carry opaque byte streams; operation names belong to the RPC
layer. Application trace events and application-defined fault points are future
RPC extensions, not implemented by this version.

## Requests

Common fields are `version: 1`, `id`, and `op`. Targets are typed objects:
`{"service":"server"}` resolves through retained placement;
`{"machine":"server-vm"}` names a prepared machine explicitly.

| `op` | Additional fields | Successful `result` |
| --- | --- | --- |
| `now` | None | `{"now_ns": E}` at the operation's effect boundary |
| `sleep` | `duration_ns`: unsigned 64-bit nanoseconds | `{}` after simulated sleep |
| `stop` | `target`: a typed target | `{}` after process-group termination and reap |
| `restart` | `target`: a typed target | `{}` after successful exec of its retained launch recipe |
| `pause` | `target`: a typed target | `{}` after freezing the whole machine |
| `resume` | `target`: a typed target | `{}` after continuing a paused machine |
| `crash` | `target`: a typed target | `{}` after destroying the machine |
| `reboot` | `target`: a typed target | `{}` after booting a crashed machine from its retained image |
| `partition` | `left`, `right`: nonempty, disjoint arrays of typed targets | `{"handle":"opaque value"}` |
| `shape` | `from`, `to`: nonempty disjoint typed-target arrays; `latency_ns`, `jitter_ns`, `loss_ppm`: integers | `{"handle":"opaque value"}` |
| `heal` | `handle`: a previously returned handle | `{}` after releasing that handle |
| `inspect` | None | `services`, `machines`, `active_handles`, `network_counters` |

Partitions cut both directions between their two groups. Each handle is
independent: a link remains cut while any applicable handle exists. Already
routed packets stay in flight; new sends crossing an active cut are dropped.
Inspect returns the retained service-to-machine mapping, application lifecycle
states (`Running`, `Stopped`, `Paused`, `Down`, or `Exited(code)`), active partition and shape
handles and directed-link counters. It exposes simulator state, not application object memory.
Transient CPU halt state is not an application lifecycle state. Counters include
sends, drops and arrivals whose nominal simulated timestamps are strictly before
the admission boundary; equal-time packets follow admission. This excludes
packets that the scheduler has delivered early but whose nominal arrival is
still in the future. Partition/shape/heal evidence uses the same counter boundary.

Machine lifecycle effects occur at admission without a guest acknowledgement.
`pause` freezes the whole machine while others keep running; `resume` continues
it, with missed timer ticks firing once, late. `crash` destroys RAM, sockets and
queued I/O; nothing is sent on the machine's behalf. `reboot` starts a fresh
incarnation from the retained image. Application readiness remains the scenario's
own check. The allowed transitions are running to paused, paused to running,
running or paused to down, and down to running, respectively. An application
process that is stopped still belongs to a running machine.

For example:

```json
{"version":1,"id":1,"op":"partition","left":[{"service":"client"}],"right":[{"service":"server"}]}
```

A successful response has this shape (times are illustrative):

```json
{"version":1,"id":1,"ok":true,"result":{"handle":"p1"},"requested_at_ns":1000000000,"admitted_at_ns":1007000000,"effect_at_ns":1007000000,"replied_at_ns":1014000000}
```

The next request can release exactly that handle:

```json
{"version":1,"id":2,"op":"heal","handle":"p1"}
```

A directed shape uses a complete, absolute policy:

```json
{"version":1,"id":3,"op":"shape","from":[{"service":"client"}],"to":[{"service":"server"}],"latency_ns":20000000,"jitter_ns":2000000,"loss_ppm":50000}
```

Shapes use the same handle counter as partitions. The highest live fault ID
covering a directed pair wins; healing reveals the previous layer or baseline.
A cut always dominates. Latency must be at least the global network latency,
jitter must be less than latency, their sum must not exceed one hour, and
`loss_ppm` must be in `[0, 1000000]`. Invalid values receive `invalid_request`.
The Python API is `world.shape(from_, to, latency=..., jitter=0, loss_ppm=0)`
with nanosecond integers (`Controller` is an alias for `World`); `world.heal()` accepts either returned handle type.
Shapes take effect at admission, and each frame retains its send-time decision.
Counters include `delivered`, `partition_dropped`, `receiver_dropped`, and
`loss_dropped` (omitted while zero); even a lost candidate consumes one loss
and one jitter sample.

An operation error uses `ok: false` and `error` instead of a result:

```json
{"version":1,"id":2,"ok":false,"error":{"code":"invalid_request","message":"inactive fault handle"},"requested_at_ns":1020000000,"admitted_at_ns":1027000000,"effect_at_ns":1027000000,"replied_at_ns":1034000000}
```

A scenario can handle rejected arguments or an inactive fault handle.
`restart` starts a stopped application using its retained launch recipe. Restarting
a running application or stopping an already stopped application returns
`invalid_state`. Illegal machine lifecycle transitions also return `invalid_state`
without creating a fault plan: pause requires a running machine, resume a paused
machine, crash a machine that is not down, and reboot a down machine. Stop and
restart of a paused machine also return `invalid_state`, since its supervisor
cannot act until it resumes. Stop/restart
and pause/resume targeting a VM that has exited return `target_unavailable`;
crash and reboot are allowed for an exited machine, subject to those transitions.
Stop/restart targeting a down machine resolve as `target_unavailable` at admission,
and pending process commands resolve with that error at their machine's crash time.
A stop or restart whose target VM exits after admission but before the effect is
acknowledged also returns `target_unavailable`; the scenario can inspect that exit
and decide its own outcome. That recovery applies to process faults requested through this RPC; a
legacy static fault plan given directly to the VMM keeps the acknowledgement
deadline below. An inspection whose response would exceed the frame limit returns
`result_too_large`.
These errors are recoverable: the connection stays usable, the error is retained
in the transcript, and no operation is reported as applied. Missing or
rejected supervisor effects and invalid protocol framing/correlation are
infrastructure errors. A pending call cut off by normal world termination stays
in the retained transcript and preserves the workload outcome or TIMEOUT. A
process command fails if its five-second simulated acknowledgement deadline is
reached without an effect while its target VM is still running; this deadline is
not shortened to the world limit.

## Time and ordering

All timestamps are unsigned 64-bit simulated nanoseconds. The VMM observes a
complete request at a committed device boundary `T`. Its recorded control latency
is `L = 2 * network_latency + 2 * stop_tolerance + 1 ms`, using checked arithmetic.
It must cover scheduler lookahead and the ordinary early-delivery window.

- `requested_at_ns = T`: complete request observed.
- `admitted_at_ns = A = T + L`: all relevant guests have reached the exact
  admission boundary; the operation can start without changing another guest's past.
- `effect_at_ns = E`: partition/heal and pause/resume/crash/reboot take effect at
  admission; stop/restart waits for the supervisor's correlated confirmation;
  sleep wakes at `A + duration_ns`.
- `replied_at_ns = R = E + L`: response delivery is scheduled strictly at this
  time, never through the ordinary early packet-delivery window.

The caller receives the response after normal guest scheduling resumes, so its
next instruction may execute later than `R`. A requested sleep includes the two
RPC delivery intervals; it is a lower bound on elapsed simulated time. Admission
and replies never derive their timestamp from host wall time or a performance
margin. Calls whose timing extends beyond the run limit remain pending and are
recorded as `cut_off` at termination. Process commands wait for the target
supervisor's readiness signal. If readiness arrives after admission, dispatch is
scheduled one control-latency interval after that signal; the original deadline
still applies.

Request order, handles, effects, replies and packet counters are retained in the
execution evidence. Performance settings such as undo margin must not change this
history. The controller's Python source, its binding, and its image are retained
with the run so replay executes the same inputs.
