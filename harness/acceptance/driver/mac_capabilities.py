"""Guest-owned check of the native ARM backend's shared RPC capabilities."""
import ctypes
import json
import os
import platform

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
    world.now_ns()
    partition = world.partition(["client"], ["server"])
    world.heal(partition)
    if platform.machine() == "aarch64":
        # The ARM guest kernel accepts tagged user pointers in syscalls once a
        # process opts in (PR_SET_TAGGED_ADDR_CTRL, PR_TAGGED_ADDR_ENABLE).
        libc = ctypes.CDLL(None, use_errno=True)
        assert libc.prctl(55, 1, 0, 0, 0) == 0, os.strerror(ctypes.get_errno())
    print("CAPABILITIES_RESULT " + json.dumps({
        "connections": connections, "first_partition": partition.handle,
    }), flush=True)
