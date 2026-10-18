"""Language-only fixture for Rust-owned tests of the actual Python guest code.

Rust owns the scenarios, TCP peers, child processes, assertions and deadlines.
This adapter only translates calls/observations and installs Rust-supplied clock
or exchange/socket inputs; it has no test cases, expectations or process orchestration.
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
real_socket = workload.socket
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
    steps = iter(request["io_steps"]) if "io_steps" in request else None
    io_trace = []

    class ScriptSocket:
        """Execute Rust-supplied I/O inputs at the socket boundary only."""
        def __enter__(self):
            return self

        def __exit__(self, *_):
            io_trace.append({"op": "close", "at": clock[0]})

        def settimeout(self, timeout):
            io_trace.append({"op": "settimeout", "timeout": timeout, "at": clock[0]})

        def perform(self, operation, **details):
            step = next(steps, None)
            if step is None or step["op"] != operation:
                raise ValueError(f"socket input does not provide {operation}: {step}")
            clock[0] += step.get("advance_seconds", 0)
            io_trace.append({"op": operation, "at": clock[0], **details})
            if step.get("error") == "timeout":
                raise TimeoutError("scripted socket timeout")
            return step

        def connect(self, endpoint):
            self.perform("connect", endpoint=endpoint)

        def sendall(self, data):
            self.perform("sendall", bytes=data.decode())

        def recv(self, limit):
            step = self.perform("recv", limit=limit)
            return step.get("bytes", "").encode()

    def sleep(seconds):
        clock[0] += seconds

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
            if "clock_reads" in request or "advance_seconds" in request or steps is not None:
                workload.time = types.SimpleNamespace(monotonic=monotonic, sleep=sleep if steps is not None else time.sleep)
            if steps is not None:
                workload.socket = types.SimpleNamespace(
                    socket=lambda *_: ScriptSocket(), getaddrinfo=real_socket.getaddrinfo,
                    SOCK_STREAM=real_socket.SOCK_STREAM)
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
        workload.socket = real_socket
    if steps is not None:
        output.update(io=io_trace, clock=clock[0])
    output.update(state=state(), stdout=console.getvalue())
    if store:
        output.update(values=store.values, generation=store.generation)
    emit(output)
