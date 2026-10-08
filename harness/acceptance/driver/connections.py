"""Guest-owned transport fixture; explicit cooperative workload phase commands.

These gates control when echo data is sent. They do not model HTTP acceptance.
The simulator classifies and drops ordinary TCP traffic between client/server.
"""
import json
import socket
import struct
import sys
import threading
import time


def connect(host, port, local_port=None):
    deadline = time.monotonic() + 10
    while True:
        s = socket.socket()
        s.settimeout(2)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        if local_port is not None:
            s.bind(("0.0.0.0", local_port))
        try:
            s.connect((host, port))
            return s
        except OSError:
            s.close()
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.05)


def listener(port):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("0.0.0.0", port))
    s.listen()
    return s


def echo(channel):
    with channel:
        try:
            while data := channel.recv(4096):
                channel.sendall(data)
        except ConnectionResetError:
            pass


def exchange(channel, data):
    channel.sendall(data)
    received = b""
    while len(received) < len(data):
        chunk = channel.recv(len(data) - len(received))
        if not chunk:
            raise RuntimeError("unexpected EOF")
        received += chunk
    if received != data:
        raise RuntimeError("echo mismatch")


def client():
    with listener(9000) as acceptor, acceptor.accept()[0] as channel:
        reader = channel.makefile("rb")
        flows = []
        for line in reader:
            op = json.loads(line)["op"]
            if op == "open":
                flows = [connect("server", 8000, port) for port in (4000, 4001)]
                for s in flows:
                    exchange(s, b"ready")
                result = {"ready": True}
            elif op == "exercise":
                flows[0].sendall(b"selected")
                exchange(flows[1], b"healthy-during-loss")
                flows[0].settimeout(0.5)
                try:
                    flows[0].recv(1)
                except socket.timeout:
                    result = {"selected_blocked": True, "healthy_completed": True}
                else:
                    raise RuntimeError("selected return traffic was not blocked")
            elif op == "reuse":
                # Abortive close avoids TIME_WAIT; only the process socket changes.
                flows[0].setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
                flows[0].close()
                flows[0] = connect("server", 8000, 4000)
                exchange(flows[0], b"reused-tuple")
                exchange(flows[1], b"still-healthy")
                result = {"reused_completed": True, "healthy_completed": True}
            elif op == "done":
                for s in flows:
                    s.close()
                return
            else:
                raise RuntimeError(f"unknown phase {op}")
            channel.sendall(json.dumps(result).encode() + b"\n")


def scenario():
    from seedbox import FaultError, World
    with connect("client", 9000) as channel, World.connect() as world:
        channel.settimeout(15)  # Includes bounded workload readiness and reuse.
        reader = channel.makefile("rb")

        def phase(op):
            channel.sendall(json.dumps({"op": op}).encode() + b"\n")
            return json.loads(reader.readline())

        assert phase("open") == {"ready": True}
        flows = world.connections("server", "client")["connections"]
        selected, healthy = [next(c for c in flows if c["dst_port"] == port) for port in (4000, 4001)]
        assert selected["incarnation"] != healthy["incarnation"]
        handle = world.drop_connection(selected)
        assert phase("exercise") == {"selected_blocked": True, "healthy_completed": True}
        # Old selector remains armed while the exact tuple opens a new incarnation.
        assert phase("reuse") == {"reused_completed": True, "healthy_completed": True}
        replacement = next(c for c in world.connections("server", "client")["connections"] if c["dst_port"] == 4000)
        assert replacement["incarnation"] != selected["incarnation"]
        try:
            world.drop_connection(selected)
        except FaultError as error:
            assert error.code == "invalid_state"
        else:
            raise RuntimeError("stale incarnation was admitted")
        evidence = world.heal(handle)
        assert evidence["matched"] >= evidence["dropped"] > 0, evidence
        assert not world.inspect()["active_handles"]
        channel.sendall(b'{"op":"done"}\n')
        print(json.dumps({"selected_blocked": True, "healthy_completed": True,
                          "tuple_reuse_completed": True, "selection_counts": evidence}, sort_keys=True))


def main():
    role = sys.argv[1]
    if role == "server":
        with listener(8000) as acceptor:
            while True:
                channel, _ = acceptor.accept()
                threading.Thread(target=echo, args=(channel,), daemon=True).start()
    elif role == "client":
        client()
    elif role == "scenario":
        scenario()
    else:
        raise RuntimeError(f"unknown role {role}")


if __name__ == "__main__":
    main()
