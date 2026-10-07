#!/usr/bin/env python3
"""Test actual Python processes and TCP responses without KVM or the fault SDK."""
import json
import importlib.util
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
from contextlib import contextmanager
from unittest import mock
import time

WORKLOAD = Path(__file__).resolve().parent / "workload" / "main.py"
spec = importlib.util.spec_from_file_location("fault_workload", WORKLOAD)
workload = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workload)


def address():
    with socket.socket() as channel:
        channel.bind(("127.0.0.1", 0))
        return f"127.0.0.1:{channel.getsockname()[1]}"


def wire(destination, command):
    endpoints = workload.resolve(destination)
    return workload.exchange(destination, command, timeout=workload.CONTROL_TIMEOUT,
                             deadline=time.monotonic() + workload.CONTROL_TIMEOUT,
                             endpoints=endpoints)


def start(mode, listen, log, *arguments):
    process = subprocess.Popen([sys.executable, "-u", str(WORKLOAD), mode,
                                "--listen", listen, *arguments], stdout=log, stderr=log)
    deadline = time.monotonic() + 3
    while True:
        assert process.poll() is None, f"{mode} exited during startup"
        try:
            host, port = listen.rsplit(":", 1)
            with socket.create_connection((host, int(port)), timeout=0.1):
                return process
        except OSError:
            if time.monotonic() > deadline:
                process.kill()
                process.wait()
                raise TimeoutError(f"{mode} readiness timeout")
            time.sleep(0.005)


def stop(process):
    if process is not None:
        process.kill()  # An actual abrupt application stop, with retained disk state.
        process.wait(timeout=3)


def check_case(root, broken, restart, batch=32):
    root.mkdir()
    server_address, control_address = address(), address()
    state = root / "kv.log"
    server_args = ["--state", str(state)] + (["--broken"] if broken else [])
    server = client = None
    with (root / "server.log").open("w") as server_log, (root / "client.log").open("w") as client_log:
        try:
            server = start("server", server_address, server_log, *server_args)
            client = start("client", control_address, client_log, "--address", server_address, "--batch", str(batch))
            assert json.loads(wire(control_address, "STATUS"))["acked"] == 0
            first = json.loads(wire(control_address, "LOAD 1"))
            assert first == {"acked": batch, "generation": 1, "total": batch}, first
            assert wire(server_address, "GET k/000000") == "VALUE 1 v000000"
            if restart:
                stop(server)
                server = None
                probe = json.loads(wire(control_address, "PROBE"))
                assert probe == {"attempts": 4, "failed": 4, "acked": 0}, probe
                server = start("server", server_address, server_log, *server_args)
                assert wire(server_address, "GET k/000000") == ("MISSING 2" if broken else "VALUE 2 v000000")
                assert json.loads(wire(control_address, "LOAD 2"))["acked"] == batch
                assert json.loads(wire(control_address, "LOAD 2"))["total"] == 3 * batch
                stop(server)
                server = None
                server = start("server", server_address, server_log, *server_args)
                assert json.loads(wire(control_address, "LOAD 3"))["total"] == 4 * batch
            else:
                for total in [2 * batch, 3 * batch, 4 * batch]:
                    assert json.loads(wire(control_address, "LOAD 1"))["total"] == total
            result = json.loads(wire(control_address, "VERIFY"))
            expected_missing = 3 * batch if broken and restart else 0
            assert result == dict(status="fail" if expected_missing else "pass", acked=4 * batch,
                                 pre_restart=batch if restart else 4 * batch,
                                 post_restart=batch if restart else 4 * batch,
                                 generation=3 if restart else 1, missing=expected_missing), result
            # A status request does not manufacture writes or alter the result.
            assert json.loads(wire(control_address, "STATUS"))["acked"] == 4 * batch
            assert json.loads(wire(control_address, "VERIFY")) == result
        finally:
            stop(client)
            stop(server)
    client_output = (root / "client.log").read_text()
    assert client_output.count("RESULT ") == 1, client_output
    assert client_output.count("MISSING_ACK ") == (3 * batch if broken and restart else 0), client_output
    server_output = (root / "server.log").read_text()
    if restart and not broken:
        assert f"generation=2 recovered={batch}" in server_output, server_output
        assert f"generation=3 recovered={3 * batch}" in server_output, server_output
    print(f"broken={broken} restart={restart} {result['status']} acknowledged={result['acked']} missing={result['missing']}")


@contextmanager
def responder(reply):
    """A bounded real TCP peer; surface thread failures to the test caller."""
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    listener.settimeout(0.05)
    destination = f"127.0.0.1:{listener.getsockname()[1]}"
    done = threading.Event()
    requests, errors = [], []

    def run():
        try:
            while not done.is_set():
                try:
                    channel, _ = listener.accept()
                except socket.timeout:
                    continue
                with channel:
                    channel.settimeout(1)
                    with channel.makefile("rb") as reader:
                        request = reader.readline(8193).decode().strip()
                    requests.append(request)
                    response = reply(request, channel, requests)
                    if response is not None:
                        channel.sendall(response.encode() + b"\n")
        except BaseException as error:
            errors.append(error)

    worker = threading.Thread(target=run)
    worker.start()
    try:
        yield destination, requests
    finally:
        done.set()
        worker.join(timeout=2)
        listener.close()
        assert not worker.is_alive(), "test responder did not stop"
        if errors:
            raise errors[0]


def expires(action):
    try:
        action()
    except TimeoutError:
        return
    raise AssertionError("operation did not expire")


def check_retry_controls(root):
    state = root / "lost-reply.log"
    store = workload.Store(state, broken=False)
    client = None

    def lose_first(request, channel, requests):
        response = store.reply(request)  # Real flush/fsync happens before the lost reply.
        if len(requests) == 1:
            assert client.sequence == 0 and client.acks == []
            return None
        if len(requests) == 2:
            assert requests[0] == requests[1]
            assert client.sequence == 0 and client.acks == []
        return response

    try:
        with responder(lose_first) as (destination, requests):
            client = workload.Client(destination)
            assert json.loads(client.reply("LOAD 1")) == dict(generation=1, acked=32, total=32)
            assert len(requests) == 33 and requests[0] == requests[1]
            assert client.sequence == 32 and len(client.acks) == 32
            assert sum(ack[0] == "k/000000" for ack in client.acks) == 1
            assert len(store.values) == 32
    finally:
        store.log.close()
    reopened = workload.Store(state, broken=False)
    try:
        assert len(reopened.values) == 32 and reopened.values["k/000000"] == "v000000"
    finally:
        reopened.log.close()

    for invalid in ("NO 1", "OK", "OK one", "OK 2"):
        def reject_first(request, channel, requests):
            return invalid if len(requests) == 1 else "OK 1"
        with responder(reject_first) as (destination, requests):
            client = workload.Client(destination)
            try:
                client.reply("LOAD 1")
            except ValueError:
                pass
            else:
                raise AssertionError(f"invalid reply accepted: {invalid}")
            assert client.sequence == 0 and client.acks == [] and len(requests) == 1
            client.put(expected=1, deadline=time.monotonic() + 1)
            assert requests == ["PUT k/000000 v000000"] * 2
            assert client.sequence == 1 and client.acks == [("k/000000", "v000000", 1)]

    # Deterministic success timing: one valid ACK, then a reply at the exact deadline.
    clock, dispatched = [0.0], []
    client = workload.Client("127.0.0.1:1")
    def slow_success(address, command, **kwargs):
        dispatched.append(command)
        clock[0] += 5
        return "OK 1"
    with mock.patch.object(workload.time, "monotonic", side_effect=lambda: clock[0]), \
         mock.patch.object(workload, "exchange", side_effect=slow_success):
        expires(lambda: client.reply("LOAD 1"))
    assert dispatched == ["PUT k/000000 v000000", "PUT k/000001 v000001"]
    assert client.sequence == 1 and client.acks == [("k/000000", "v000000", 1)]

    # Expiry before another dispatch, even when the previous reply was successful.
    reads = iter([0.0, 0.0, 0.0, 0.0, 10.0])
    client = workload.Client("127.0.0.1:1")
    with mock.patch.object(workload.time, "monotonic", side_effect=lambda: next(reads)), \
         mock.patch.object(workload, "exchange", return_value="OK 1") as transport:
        expires(lambda: client.reply("LOAD 1"))
    assert transport.call_count == 1 and client.sequence == 1 and len(client.acks) == 1

    def unanswered(request, channel, requests):
        time.sleep(0.25)
        return None
    def trickle(request, channel, requests):
        for _ in range(30):
            try:
                channel.sendall(b"O")
            except OSError:
                return None
            time.sleep(0.01)
        return None
    for peer in (unanswered, trickle):
        with responder(peer) as (destination, requests), \
             mock.patch.object(workload, "LOAD_TIMEOUT", 0.12):
            client = workload.Client(destination)
            started = time.monotonic()
            expires(lambda: client.reply("LOAD 1"))
            elapsed = time.monotonic() - started
            assert elapsed < 1, (peer.__name__, elapsed)
            assert client.sequence == 0 and client.acks == []
            assert requests == ["PUT k/000000 v000000"], requests
    print("pending PUT, invalid reply, successful-progress and I/O deadline controls passed")


def main():
    with tempfile.TemporaryDirectory(prefix="seedbox-python-workload-") as directory:
        check_retry_controls(Path(directory))
        for broken, restart in [(False, True), (True, True), (True, False)]:
            check_case(Path(directory) / f"case-{broken}-{restart}", broken, restart)
    print("portable Python workload checks passed")


if __name__ == "__main__":
    main()
