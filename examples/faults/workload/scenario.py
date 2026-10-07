"""Decide when to inject faults from completed client workload phases."""
import json
import time
from seedbox import World
from main import BATCH, PROBES, CONTROL_TIMEOUT, exchange, resolve


def client(address, command):
    endpoints = resolve(address)
    def request():
        return json.loads(exchange(address, command, timeout=CONTROL_TIMEOUT,
                                   deadline=time.monotonic() + CONTROL_TIMEOUT,
                                   endpoints=endpoints))

    # Wait for the client's listener without retrying submitted workload commands.
    if command != "STATUS":
        return request()
    deadline = time.monotonic() + 10
    while True:
        try:
            # LOAD and VERIFY execute finite work before their reply.
            return request()
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.02)


def run(address, no_faults=False, batch=BATCH):
    if not 1 <= batch <= BATCH:
        raise ValueError("batch must be between 1 and 32")
    def load(generation):
        result = client(address, f"LOAD {generation}")
        assert result["generation"] == generation and result["acked"] == batch, result
        return result

    def probe(label):
        result = client(address, "PROBE")
        assert result == {"attempts": PROBES, "failed": PROBES, "acked": 0}, result
        print(f"SCENARIO_PROBE phase={label} " + json.dumps(result, sort_keys=True), flush=True)

    with World.connect() as world:
        started_ns = world.now_ns()
        client(address, "STATUS")
        progress = load(1)
        if no_faults:
            for _ in range(3):
                load(1)
        elif progress["acked"] >= batch:
            world.stop("server")
            world.sleep(0.1)
            world.restart("server")
            load(2)  # The restart RPC confirms exec; the client checks readiness.
            first = world.partition(["client"], ["server"])
            world.sleep(0.1)
            overlap = world.partition(["client"], ["server"])
            assert set(world.inspect()["active_handles"]) == {first.handle, overlap.handle}
            probe("both")
            world.heal(first)
            assert world.inspect()["active_handles"] == [overlap.handle]
            probe("partial")  # The overlapping handle must still drop traffic.
            world.heal(overlap)
            assert world.inspect()["active_handles"] == []
            load(2)  # Actual ACKs demonstrate delivery after the final heal.
            world.stop("server")
            world.sleep(0.1)
            world.restart("server")
            load(3)
        result = client(address, "VERIFY")
        assert result["acked"] == 4 * batch and result["generation"] == (1 if no_faults else 3), result
        print(f"SCENARIO_TIME start_ns={started_ns} finish_ns={world.now_ns()}", flush=True)
        print("SCENARIO_RESULT " + json.dumps(result, sort_keys=True), flush=True)
        return 0 if result["status"] == "pass" else 1
