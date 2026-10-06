#!/usr/bin/env python3
"""Test actual Python processes and TCP responses without KVM or the fault SDK."""
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time

WORKLOAD = Path(__file__).resolve().parent / "workload" / "main.py"


def address():
    with socket.socket() as channel:
        channel.bind(("127.0.0.1", 0))
        return f"127.0.0.1:{channel.getsockname()[1]}"


def wire(destination, command):
    host, port = destination.rsplit(":", 1)
    with socket.create_connection((host, int(port)), timeout=3) as channel:
        channel.settimeout(5)
        channel.sendall(command.encode() + b"\n")
        with channel.makefile("rb") as reader:
            reply = reader.readline(8193)
            assert reply.endswith(b"\n") and len(reply) <= 8192, reply
            return reply.decode().strip()


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


def check_case(root, broken, restart):
    root.mkdir()
    server_address, control_address = address(), address()
    state = root / "kv.log"
    server_args = ["--state", str(state)] + (["--broken"] if broken else [])
    server = client = None
    with (root / "server.log").open("w") as server_log, (root / "client.log").open("w") as client_log:
        try:
            server = start("server", server_address, server_log, *server_args)
            client = start("client", control_address, client_log, "--address", server_address)
            assert json.loads(wire(control_address, "STATUS"))["acked"] == 0
            first = json.loads(wire(control_address, "LOAD 1"))
            assert first == {"acked": 32, "generation": 1, "total": 32}, first
            assert wire(server_address, "GET k/000000") == "VALUE 1 v000000"
            if restart:
                stop(server)
                server = None
                probe = json.loads(wire(control_address, "PROBE"))
                assert probe == {"attempts": 4, "failed": 4, "acked": 0}, probe
                server = start("server", server_address, server_log, *server_args)
                assert wire(server_address, "GET k/000000") == ("MISSING 2" if broken else "VALUE 2 v000000")
                assert json.loads(wire(control_address, "LOAD 2"))["acked"] == 32
                assert json.loads(wire(control_address, "LOAD 2"))["total"] == 96
                stop(server)
                server = None
                server = start("server", server_address, server_log, *server_args)
                assert json.loads(wire(control_address, "LOAD 3"))["total"] == 128
            else:
                for total in [64, 96, 128]:
                    assert json.loads(wire(control_address, "LOAD 1"))["total"] == total
            result = json.loads(wire(control_address, "VERIFY"))
            expected_missing = 96 if broken and restart else 0
            assert result == dict(status="fail" if expected_missing else "pass", acked=128,
                                 pre_restart=32 if restart else 128,
                                 post_restart=32 if restart else 128,
                                 generation=3 if restart else 1, missing=expected_missing), result
            # A status request does not manufacture writes or alter the result.
            assert json.loads(wire(control_address, "STATUS"))["acked"] == 128
            assert json.loads(wire(control_address, "VERIFY")) == result
        finally:
            stop(client)
            stop(server)
    client_output = (root / "client.log").read_text()
    assert client_output.count("RESULT ") == 1, client_output
    assert client_output.count("MISSING_ACK ") == (96 if broken and restart else 0), client_output
    server_output = (root / "server.log").read_text()
    if restart and not broken:
        assert "generation=2 recovered=32" in server_output, server_output
        assert "generation=3 recovered=96" in server_output, server_output
    print(f"broken={broken} restart={restart} {result['status']} acknowledged={result['acked']} missing={result['missing']}")


def main():
    with tempfile.TemporaryDirectory(prefix="seedbox-python-workload-") as directory:
        for broken, restart in [(False, True), (True, True), (True, False)]:
            check_case(Path(directory) / f"case-{broken}-{restart}", broken, restart)
    print("portable Python workload checks passed")


if __name__ == "__main__":
    main()
