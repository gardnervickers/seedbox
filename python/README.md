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
partitioned. Closing the connection does not silently heal outstanding cuts.

`stop`, `restart`, `heal`, and `sleep` return an `Effect` containing request,
admission, effect and reply timestamps. `partition` returns a `Partition` with
its opaque handle and effect. `now_ns()` reads simulated time; `inspect()` returns
topology, machine and per-service process state, active handles and packet
counters. Strings name services; `Machine("prepared-name")` names the only
service on a prepared machine. Services sharing the controller's kernel remain
valid targets.

`FaultError` carries a structured operation error. `ProtocolError` indicates an
invalid or interrupted exchange. The client never retries uncertain operations.
Use one active connection in one thread at a time. After a completed call, close
that `World` and use `World.connect()` for a new session; partition handles remain
valid. Restarting a running application or stopping one twice raises a recoverable
`FaultError` with code `invalid_state`; an exited VM returns `target_unavailable`.
The scenario chooses how those errors affect its result.

Other languages can implement the same [versioned RPC protocol](PROTOCOL.md).
There are no Python-specific objects on the wire, and future declarative frontends
can be clients of this API.
