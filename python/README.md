# Python scenario API

Place `seedbox/` on your controller image's Python module path. The package uses
only the standard library. Select that service with
`x-seedbox.control: {service: scenario}`; Compose defines the applications, while
ordinary Python code decides when to call the simulator.

```python
from seedbox import World

with World.connect() as world:
    # Application-specific code can wait for readiness or acknowledged writes.
    wait_for_acknowledged_writes(32)

    world.stop("server")
    world.sleep(1)
    world.restart("server")
    wait_for_server_ready()

    first = world.partition(["client"], ["server"])
    overlap = world.partition(["client"], ["server"])
    world.sleep(2)
    world.heal(first)       # The overlapping cut is still active.
    world.sleep(1)
    world.heal(overlap)

    world.pause("server")   # Freeze its whole machine while others run.
    world.sleep(1)
    world.resume("server")
    world.crash("server")   # Lose RAM, sockets and queued I/O.
    world.reboot("server")  # Fresh boot from the retained image.
    wait_for_server_ready()

    check_acknowledged_values()
```

The application helpers above are illustrative; the
[complete Python example](../examples/faults/README.md) implements real service
readiness, progress and data checks. No scenario code is evaluated on the host.
The scenario runs again during replay.

Methods are blocking: the calling thread waits for the confirmed operation while
the simulated world continues. This keeps normal Python control flow useful,
including loops, conditionals and calls to application clients. A controller is
outside the fault model; keep it separate from the services being stopped or
partitioned or shaped. Closing the connection does not silently heal outstanding handles.

`stop`, `restart`, `pause`, `resume`, `crash`, `reboot`, link-handle `heal`, and `sleep` return an `Effect` containing request,
admission, effect and reply timestamps. `partition` returns a `Partition` with
its opaque handle and effect. `shape(from_, to, latency=..., jitter=0, loss_ppm=0)`
returns a `Shape` accepted by `heal`; latency and jitter are nanosecond integers.
Optional `bandwidth_bps=1_000_000, queue_bound=12_000_000` serializes frames in
each directed pair and tail-drops candidates above the send-to-departure bound.
The bound uses nanoseconds and requires finite bandwidth. Healing a bandwidth
shape returns `queue_counts` with `matched`, `queue_dropped`, and
`delayed_max_ns`; require positive drop evidence when testing congestion. Omitted
options preserve the existing unlimited policy and heal effect timestamp.
It changes only the directed pairs, with the highest live fault ID taking precedence.
A partition dominates a shape, and in-flight decisions remain fixed. `Controller`
is also exported as an alias for `World`. `now_ns()` reads simulated time; `inspect()` returns
topology, machine state, active handles and packet counters. Strings name services;
use `Machine("prepared-name")` when explicitly targeting a machine.

Whole-machine operations take effect at admission without a guest acknowledgement.
Pause freezes CPU and devices; resume continues them with missed timer ticks
firing once, late. Crash destroys RAM, sockets and queued I/O and sends nothing
on the machine's behalf. Reboot boots a new incarnation from its retained image;
the scenario must wait for application readiness. `inspect()` exposes `Paused`
and `Down` alongside process and exit states. The controller cannot be a target.

`FaultError` carries a structured operation error. `ProtocolError` indicates an
invalid or interrupted exchange. The client never retries uncertain operations.
Use one active connection in one thread at a time. After a completed call, close
that `World` and use `World.connect()` for a new session; partition and shape handles remain
valid. Restarting a running application or stopping one twice raises a recoverable
`FaultError` with code `invalid_state`. Machine operations require running→paused
for pause, paused→running for resume, running or paused→down for crash, and
down→running for reboot; other transitions raise `invalid_state` without a fault,
as do stop and restart of a paused machine.
Stop/restart and pause/resume of an exited VM raise `target_unavailable`; crash
and reboot of one are allowed subject to the machine transition rules. A
stop/restart admitted while down, or still pending when the machine crashes,
also raises `target_unavailable` at admission or at the crash time, respectively.
The scenario chooses how those errors affect its result.

Other languages can implement the same [versioned RPC protocol](PROTOCOL.md).
There are no Python-specific objects on the wire, and future declarative frontends
can be clients of this API.

Committed console triggers are available in the development engine:

```python
trigger = world.arm_console_partition("server", b"READY_TO_CUT", ["server"], ["client"], match_timeout=1)
# Scenario-owned readiness/effect checks use world.inspect().
world.heal(trigger.partition)  # After its effect; inactive handles are rejected.
```

Use `arm_console_stop()` for a future process stop and `cancel_trigger()` for
an unmatched arm. These consume only future committed plain console bytes;
confirmation means armed. See [the contract and AMD handoff](../docs/triggers.md).

`connections(from_, to, after=0)` returns a live page of up to 32 directional TCP
incarnations plus a `next_after` cursor. Pass one exact record to
`drop_connection(record)` to arm loss. `heal()` on this `ConnectionDrop` returns
`matched` and `dropped` counts; `last_effect` retains its timing. Require a positive
`dropped` count to verify injection. Tuple reuse never transfers an old handle.
See [the transport contract](../docs/runtime.md#connection-selectors) for SYN,
FIN/RST, bounded eviction and traffic that bypasses selection.
