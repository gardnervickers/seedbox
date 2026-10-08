"""Guest-owned check of the native ARM backend's shared RPC capabilities."""
import json

from seedbox import FaultError, World


with World.connect() as world:
    connections = world.connections("client", "server")
    assert connections == {"connections": [], "next_after": 0}, connections
    # A real selector must be discovered first. Rejection is shared admission,
    # rather than a backend declaring the operation unsupported.
    try:
        world.drop_connection({
            "from": "client", "to": "server", "src_addr": "10.0.0.1",
            "dst_addr": "10.0.0.2", "src_port": 1000, "dst_port": 2000,
            "incarnation": 1,
        })
    except FaultError as error:
        assert error.code == "invalid_state", error.response
    else:
        raise AssertionError("an undiscovered connection selector succeeded")
    trigger = world.arm_console_stop("client", b"CAPABILITY_MARKER", "client", match_timeout=1)
    assert world.inspect()["triggers"][trigger.id - 1]["status"] == "armed"
    world.cancel_trigger(trigger)
    assert world.inspect()["triggers"][trigger.id - 1]["status"] == "cancelled"
    world.now_ns()
    partition = world.partition(["client"], ["server"])
    world.heal(partition)
    print("CAPABILITIES_RESULT " + json.dumps({
        "connections": connections, "trigger_id": trigger.id,
        "first_partition": partition.handle,
    }), flush=True)
