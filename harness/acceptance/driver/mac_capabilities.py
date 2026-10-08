"""Guest-owned check of the native ARM backend's initial RPC capability boundary."""
import json

from seedbox import Effect, FaultError, Trigger, World


def unsupported(name, call):
    try:
        call()
    except FaultError as error:
        assert error.code == "unsupported", error.response
        assert error.response["ok"] is False, error.response
        assert "result" not in error.response, error.response
        return name
    raise AssertionError(f"{name} unexpectedly succeeded")


with World.connect() as world:
    rejected = [
        unsupported("connections", lambda: world.connections("client", "server")),
        unsupported("drop_connection", lambda: world.drop_connection({
            "from": "client", "to": "server", "src_addr": "10.0.0.1",
            "dst_addr": "10.0.0.2", "src_port": 1000, "dst_port": 2000,
            "incarnation": 1,
        })),
        unsupported("arm_trigger", lambda: world.arm_console_partition(
            "client", b"CAPABILITY_MARKER", ["client"], ["server"], match_timeout=1)),
        unsupported("cancel_trigger", lambda: world.cancel_trigger(
            Trigger(1, Effect(0, 0, 0, 0)))),
    ]
    world.now_ns()
    partition = world.partition(["client"], ["server"])
    assert partition.handle == "p1", partition
    world.heal(partition)
    print("CAPABILITIES_RESULT " + json.dumps({"rejected": rejected, "first_handle": partition.handle}), flush=True)
