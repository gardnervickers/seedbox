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
| `shape` | `from`, `to`: nonempty disjoint typed-target arrays; `latency_ns`, `jitter_ns`, `loss_ppm`: integers; optional `bandwidth_bps`, `queue_bound_ns`, `network_faults` below | `{"handle":"opaque value"}` |
| `connections` | `from`, `to`: distinct typed targets; `after`: optional unsigned incarnation cursor (default 0) | `connections`: up to 32 directional records; `next_after`: last returned incarnation (or input cursor) |
| `drop_connection` | `connection`: one exact discovery record | `{"handle":"opaque value"}` after arming selected loss |
| `heal` | `handle`: a previously returned handle | `{}` for a plain link or partition; `queue_counts` for bandwidth; `network_fault_counts` for impairments; `{"matched": N, "dropped": N}` for a connection handle |
| `inspect` | None | `services`, `machines`, `active_handles`, `network_counters`; `triggers` only after an arm |
| `arm_trigger` | `source`: typed target; `marker`: 1..=256 byte integers; `match_timeout_ns`: positive u64; `fault`: typed trigger fault below | `{"trigger_id": N}`; also `handle` for a partition |
| `cancel_trigger` | `trigger_id`: previously returned positive ID | `{}` after cancellation at admission |

Partitions cut both directions between their two groups. Each handle is
independent: a link remains cut while any applicable handle exists. Already
routed packets stay in flight; new sends crossing an active cut are dropped.
Inspect returns the retained service-to-machine mapping, application lifecycle
states (`Running`, `Stopped`, `Paused`, `Down`, or `Exited(code)`), active partition, shape and connection
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
The Python API is `world.shape(from_, to, latency=..., jitter=0, loss_ppm=0,
bandwidth_bps=None, queue_bound=None)`
with nanosecond integers (`Controller` is an alias for `World`); `world.heal()` accepts each returned handle type.
Shapes take effect at admission, and each frame retains its send-time decision.
Optional `bandwidth_bps` is a positive u64 integer in bits per second and
`queue_bound_ns` is a nonnegative u64 integer that requires finite bandwidth.
Omitting both retains unlimited policy and the historical request bytes.
Serialization uses actual Ethernet bytes and ceiling nanosecond division;
the bound includes both waiting and serialization. Tail drops reserve no queue
space, and healing preserves already accepted departures. Counters include
`delivered`, `partition_dropped`, `receiver_dropped`, `loss_dropped` and
`queue_dropped` (the last two are omitted while zero). A non-cut candidate,
including selected loss and tail drop, consumes one loss and one jitter sample.
Bandwidth adds no RNG draws.

Healing a bandwidth shape returns
`{"queue_counts":{"matched":2,"queue_dropped":1,"delayed_max_ns":24000000}}`.
Matched candidates reached that winning queue after cuts and loss; the maximum
delay includes proposed departures of tail-dropped frames. The shape and heal
fault rows retain the same evidence, including explicit zeros for an unused
shape. The Python API returns the result object for bandwidth shapes and the
existing effect timestamp for unlimited shapes.

### Dynamic network impairments

Every impairment is optional on a `shape` layer and is released with its handle.
The complete policy of the newest matching shape wins, including these fields;
overlapping handles can be healed in either order. A newer plain shape temporarily
masks an older impairment. Healing reveals the older layer, including its suspended
burst state. Already scheduled frames, copies and resets survive healing.

The wire request adds a strict `network_faults` object. Omitted fields default to
zero; probabilities are integer parts per million in `[0, 1000000]`:

| Wire field | Python `world.shape` keyword | Meaning |
| --- | --- | --- |
| `reorder_ppm` | `reorder_ppm` | Chance to hold a surviving frame for an additional seeded delay |
| `reorder_delay_ns` | `reorder_delay` | Maximum extra delay; sample uniformly from 1 through this bound |
| `duplicate_ppm` | `duplicate_ppm` | Chance to inject one extra receive-side copy one nanosecond after the original |
| `burst_loss_ppm` | `burst_loss_ppm` | Chance for a healthy direction to start a loss burst |
| `burst_length` | `burst_length` | Number of consecutive eligible candidates dropped, including the starting candidate |
| `latency_spike_ppm` | `latency_spike_ppm` | Chance to add a fixed latency spike to a surviving frame |
| `latency_spike_ns` | `latency_spike` | Extra spike delay in nanoseconds |
| `tcp_reset_ppm` | `tcp_reset_ppm` | Chance to replace an eligible TCP segment with a RST in that direction |

An active reorder, burst or spike requires a positive bound, length or delay.
Latency plus jitter plus the maximum active reorder and spike delays must fit
within one hour. Python rejects invalid local values before sending; runtime
admission and fault-plan compilation validate the same bounds.

For example, all calls below take effect during execution and return a healable
`Shape`. Delays are integer nanoseconds:

```python
reorder = world.shape(["client"], ["server"], latency=1_000_000,
                      reorder_ppm=500_000, reorder_delay=100_000_000)
counts = world.heal(reorder)["network_fault_counts"]
duplicate = world.shape(["client"], ["server"], latency=1_000_000,
                        duplicate_ppm=100_000)
world.heal(duplicate)
burst = world.shape(["client"], ["server"], latency=1_000_000,
                    burst_loss_ppm=100_000, burst_length=4)
world.heal(burst)
spike = world.shape(["client"], ["server"], latency=1_000_000,
                    latency_spike_ppm=100_000, latency_spike=50_000_000)
world.heal(spike)
reset = world.shape(["server"], ["client"], latency=1_000_000,
                    tcp_reset_ppm=1_000_000)
world.heal(reset)
```

Cuts and selected connection loss dominate all new faults. Ordinary loss/jitter
draws occur next, in their historical order, even when jitter or loss is zero.
Survivors reach burst loss, then bandwidth admission and the base FIFO arrival.
The burst stream draws only outside an existing burst; cuts, selected loss,
ordinary loss and masked layers do not advance a burst. Burst drops reserve
neither departure nor arrival clocks. Admitted frames sample reorder, spike,
eligible reset, then duplication from independent named streams. Reorder uses
a second draw for its delay only when selected. Inactive faults draw nothing.
The five domains are `link-reorder/FROM/TO`, `link-duplicate/FROM/TO`,
`link-burst-loss/FROM/TO`, `link-latency-spike/FROM/TO` and
`link-tcp-reset/FROM/TO`. Checkpoint branches use the existing identity-derived
domain prefix. Snapshots retain stream counters and burst state.

Extra delays sit outside the base FIFO clock, so later frames can overtake a
delayed frame, even across healing. Copies are receive-side injection, with no
second serialization charge or legacy RNG draw. Both copies undergo normal
receiver lifecycle/backlog admission and contribute to delivered/drop counters.
All scheduled deadlines respect the global lookahead; no fault uses host time.

Reset supports untagged unicast Ethernet carrying unfragmented IPv4 TCP with
valid header lengths and nonzero ports. Only ACK-bearing, non-SYN/non-RST
segments are eligible. It constructs minimal IPv4/TCP RST+ACK headers with
the same addresses, ports, sequence and acknowledgement and correct checksums;
options and payload are removed. The reset replaces that direction's frame at
its deterministic simulated delivery deadline. Unsupported traffic is unchanged
and consumes no reset draw. The guest TCP stack still validates the sequence
against its receive window; reset evidence means a RST was scheduled, and the
workload must check the socket effect. A reset does not prevent reconnection.

Healing returns `network_fault_counts` with `matched`, `reordered`, `duplicated`,
`burst_dropped`, `spiked`, `tcp_reset` and `delayed_max_ns`. `matched` counts
candidates after cuts, selected loss and ordinary loss; subsequent burst/queue
drops are included. The remaining counters record selected injections before
receiver admission; `reordered` does not prove receipt order changed.
`delayed_max_ns` is the largest combined extra delay. Fault rows retain the same
counts, including zeros for unused layers. Python returns the result dictionary
for these shapes, including both count objects when bandwidth is also enabled.
The guest-owned `harness/acceptance/workloads/network-faults.yaml` checks receipt
overtaking, actual copies and consecutive losses, a latency spike, a socket reset,
and successful exchanges after each heal. Run it and replay the retained run to
require `MATCH`.

Connection discovery and selection take effect at the exact admission barrier,
including frames sent before it and excluding equal-time sends. Discovery records
have exactly `from`, `to` (prepared-machine names), `src_addr`, `dst_addr`
(canonical IPv4 strings), `src_port`, `dst_port` (nonzero u16), and `incarnation`
(nonzero u64). The record selects only that direction. Discovery resolves typed
service targets and excludes the controller. It is read-only; paginate using
`after=next_after` until an empty page. Pages are live views, ordered by increasing
incarnation, not a frozen inventory. Long machine names can still produce
`result_too_large`; no result is silently truncated beyond the stated page size.

`drop_connection` shares the link fault handle counter. A missing, closed, evicted
or replaced incarnation at admission returns `invalid_state` without a fault.
An accepted handle means **armed**, not that loss occurred. `heal` returns matched
and actually dropped candidate counts; retained selector fault rows carry the
same `selection_counts`, including explicit zeros. Scenarios asserting an injected
fault must require `dropped > 0`. `matched` includes packets masked by a partition;
`dropped` excludes them. Overlapping connection handles each count their matched
drops, and healing one leaves others active. A stale active handle never follows
a reused tuple; it remains healable with its accumulated evidence.

See [connection classification](../docs/runtime.md#connection-selectors) for TCP
lifecycle, bounded eviction, unsupported traffic and send-time semantics.

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

## Committed console trigger extension (development engine)

`fault` is either `{"action":"stop","target":{"service":"server"}}` or
`{"action":"partition","left":[{"service":"client"}],"right":[{"service":"server"}]}`.
Arguments and nested faults reject unknown fields. Sources may include the
controller; fault targets may not. Stop and partition are the only trigger
actions in this initial extension. No network/control-event selectors are
exposed. Arm replies confirm admission, not firing. Cancellation rejects any
trigger no longer armed. A partition handle can be healed after its effect.

A run admits at most 64 triggers total. Each marker uses bounded streaming
matching of future COM1/debug-port transmitted bytes, including split writes.
Matching is once per trigger in `[admission, admission + match_timeout_ns)`.
The final byte supplies the committed match timestamp; its effect is scheduled
at that time plus the fixed control latency. Same-time effects follow arm ID.
Existing in-flight retention and five-second process effect deadlines apply.
Expiry is retained without a fault; cutoff retains unmatched prefixes or pending
effects. `inspect.triggers` and terminal `vmm.json.triggers` contain the same
canonical matcher/effect records, with cutoff added at termination. The world
witness includes those records only when triggers have been used.

The full [time/commitment contract and AMD controls](../docs/triggers.md) define
tie order, snapshots, discarded candidates and charged-I/O boundaries. Plain
console bytes do not establish an accepted PUT or any application commit.
