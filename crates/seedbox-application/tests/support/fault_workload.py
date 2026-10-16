"""Language-only fixture for Rust-owned tests of the actual Python guest code.

Rust owns the scenarios, TCP peers, child processes, assertions and deadlines.
This adapter only translates calls/observations and installs Rust-supplied clock
or exchange inputs; it has no test cases, expectations or process orchestration.
"""
import contextlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import time
import types
import traceback

ROOT = Path(__file__).resolve().parents[4]
spec = importlib.util.spec_from_file_location("fault_workload", ROOT / "examples/faults/workload/main.py")
workload = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workload)
real_exchange, real_time, real_load_timeout = workload.exchange, workload.time, workload.LOAD_TIMEOUT
client = store = None


def emit(value):
    print(json.dumps(value), file=sys.__stdout__, flush=True)


def state():
    return {"sequence": client.sequence, "acks": client.acks, "failed": client.failed} if client else None


for line in sys.stdin:
    request = json.loads(line)
    output, console = {}, io.StringIO()
    clock = [0.0]
    reads = iter(request["clock_reads"]) if "clock_reads" in request else None
    def monotonic():
        return next(reads) if reads is not None else clock[0]
    def exchange(address, command, **kwargs):
        emit({"event": "dispatch", "command": command, "state": state()})
        if "exchange_response" in request:
            clock[0] += request.get("advance_seconds", 0)
            return request["exchange_response"]
        return real_exchange(address, command, **kwargs)
    try:
        with contextlib.redirect_stdout(console):
            if "clock_reads" in request or "advance_seconds" in request:
                workload.time = types.SimpleNamespace(monotonic=monotonic, sleep=time.sleep)
            workload.LOAD_TIMEOUT = request.get("load_timeout", real_load_timeout)
            workload.exchange = exchange
            operation = request["op"]
            if operation == "store_open":
                if store and store.log:
                    store.log.close()
                store = workload.Store(Path(request["path"]), request.get("broken", False))
            elif operation == "store_reply":
                output["value"] = store.reply(request["command"])
            elif operation == "store_close":
                if store.log:
                    store.log.close()
            elif operation == "client_open":
                client = workload.Client(request["address"], request.get("batch", workload.BATCH))
            elif operation == "client_reply":
                output["value"] = client.reply(request["command"])
            elif operation == "client_put":
                output["value"] = client.put(request.get("expected"), workload.time.monotonic() + request["budget"])
            else:
                raise ValueError("unknown language-adapter operation")
    except Exception as error:
        output["error"] = {"type": type(error).__name__, "message": str(error), "traceback": traceback.format_exc()}
    finally:
        workload.exchange, workload.time, workload.LOAD_TIMEOUT = real_exchange, real_time, real_load_timeout
    output.update(state=state(), stdout=console.getvalue())
    if store:
        output.update(values=store.values, generation=store.generation)
    emit(output)
