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
complete the run before the scenario finishes. The controller is outside the fault model. Stop/restart cannot target it, and
partition groups cannot contain it. Its control transport and network links
remain available during cuts between other guests. A stopped application still
cannot answer application requests; restart confirmation means successful exec,
so the scenario must check application readiness separately. Normal controller
completion supplies the configured check outcome.

Connect to the Unix stream socket `/run/seedbox/control.sock` inside the guest.
Version 1 supports one controller connection for the run and one outstanding
request at a time. Other language bindings need only a Unix socket and JSON.
The guest supervisor relays these messages through a named virtio-console port,
`seedbox`. Its transmit and receive queues use guest RAM shared with the simulator;
a device notification commits the submitted bytes. This channel is independent
of simulated Ethernet. No host socket, shared host filesystem, or host clock is
involved. The transport is available on every prepared guest; runtime fault
operations are authorized only for the configured controller. Further message
families can use the same transport for application instrumentation.

Each request and response is one UTF-8 JSON object followed by a newline, at most
16384 bytes including the newline. Embedded newlines must be JSON-escaped.
Do not pipeline calls or reconnect and retry an uncertain operation. Request
IDs are positive unsigned 32-bit integers, starting at 1 and increasing by one.
The response echoes the version and ID. The API has no implicit retry, automatic
heal, or garbage-collection-based operation.

The private supervisor-to-VMM relay framing is not part of the public socket
protocol; custom bindings do not implement virtqueue or device I/O. The device
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
| `partition` | `left`, `right`: nonempty, disjoint arrays of typed targets | `{"handle":"opaque value"}` |
| `heal` | `handle`: a previously returned handle | `{}` after releasing that handle |
| `inspect` | None | `services`, `machines`, `active_handles`, `network_counters` |

Partitions cut both directions between their two groups. Each handle is
independent: a link remains cut while any applicable handle exists. Already
routed packets stay in flight; new sends crossing an active cut are dropped.
Inspect returns the retained service-to-machine mapping, machine execution
states, active partition handles and directed-link counters, all sampled at its
admission boundary. It exposes simulator state, not application object memory.

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

An operation error uses `ok: false` and `error` instead of a result:

```json
{"version":1,"id":2,"ok":false,"error":{"code":"invalid_request","message":"inactive partition handle"},"requested_at_ns":1020000000,"admitted_at_ns":1027000000,"effect_at_ns":1027000000,"replied_at_ns":1034000000}
```

A scenario can handle rejected arguments or an inactive partition handle. The error is
retained in the transcript and no operation is reported as applied. Missing or
rejected supervisor effects and invalid protocol framing/correlation are
infrastructure errors. A pending call cut off by normal world termination stays
in the retained transcript and preserves the workload outcome or TIMEOUT. A
process command fails if its five-second simulated acknowledgement deadline is
reached without an effect; this deadline is not shortened to the world limit.

## Time and ordering

All timestamps are unsigned 64-bit simulated nanoseconds. The VMM observes a
complete request at a committed device boundary `T`. Its recorded control latency
is `L = 2 * network_latency + 2 * stop_tolerance + 1 ms`, using checked arithmetic.
It must cover scheduler lookahead and the ordinary early-delivery window.

- `requested_at_ns = T`: complete request observed.
- `admitted_at_ns = A = T + L`: all relevant guests have reached the exact
  admission boundary; the operation can start without changing another guest's past.
- `effect_at_ns = E`: a partition/heal takes effect at admission; stop/restart
  waits for the supervisor's correlated confirmation; sleep wakes at `A + duration_ns`.
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
