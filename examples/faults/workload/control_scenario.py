"""Recoverable lifecycle errors and connection reuse inside the world."""
import json
import time
from main import exchange
from seedbox import FaultError, World


def ready():
    deadline = time.monotonic() + 10
    while True:
        try:
            assert exchange("server:7000", "PING") == "READY socket_absent"
            return
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.02)


def run(address, no_faults=False):
    errors = []
    def rejected(operation, code):
        try:
            operation("server")
        except FaultError as error:
            assert error.code == code, error
            errors.append(code)
        else:
            raise AssertionError("invalid lifecycle command succeeded")

    with World.connect() as world:
        ready()
        world.now_ns()
        rejected(world.restart, "invalid_state")
        world.stop("server")
        rejected(world.stop, "invalid_state")
    with World.connect() as world:
        world.restart("server")
        ready()
        try:
            exchange("server:7000", "EXIT")
        except OSError:
            pass
        else:
            raise AssertionError("target did not exit")
        for _ in range(100):
            snapshot = world.inspect()
            state = snapshot["processes"]["server"]
            if state.startswith("Exited"):
                break
            world.sleep(0.01)
        else:
            raise AssertionError("target VM did not report its exit")
        shared = len(set(snapshot["services"].values())) == 1
        rejected(world.stop, "invalid_state" if shared else "target_unavailable")
        if shared:
            world.restart("server")
            ready()
        else:
            rejected(world.restart, "target_unavailable")
        world.now_ns()
    print("CONTROL_CASES_RESULT " + json.dumps({"errors": errors, "target_exit": 17, "reconnected": True}, sort_keys=True), flush=True)
    # The scenario classifies the actual application exit as a workload failure.
    return 1
