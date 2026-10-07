"""Guest-owned synthetic console-trigger controls; no accepted-PUT claim."""
import os
import socket
import socketserver
import sys
import time
from control import World, FaultError


def exchange(address, message, timeout=5):
    host, port = address.split(":")
    with socket.create_connection((host, int(port)), timeout=timeout) as channel:
        channel.settimeout(timeout)
        channel.sendall(message.encode() + b"\n")
        with channel.makefile("rb") as reader:
            return reader.readline(128).decode().strip()


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        message = self.rfile.readline(128).decode().strip()
        if message.startswith("emit "):
            marker = message[5:].encode()
            # Separate writes exercise the UART stream, including split markers.
            for byte in marker:
                os.write(1, bytes([byte]))
            os.write(1, b"\n")
            self.wfile.write(b"EMITTED\n")
        elif message == "probe":
            try:
                self.wfile.write((exchange("source:7000", "ping", timeout=0.2) + "\n").encode())
            except (OSError, TimeoutError):
                self.wfile.write(b"BLOCKED\n")
        else:
            self.wfile.write(b"PONG\n")


def server(role):
    os.write(1, b"OLD_MARKER\n")
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("0.0.0.0", 7000), Handler) as service:
        service.serve_forever()


def scenario():
    deadline = time.monotonic() + 10
    for role in ("source", "peer"):
        while True:
            try:
                assert exchange(role + ":7000", "ping") == "PONG"
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.01)
    with World.connect() as world:
        old = world.arm_console_stop("source", b"OLD_MARKER", "source", match_timeout=1)
        world.sleep(0.01)
        assert world.inspect()["triggers"][old.id - 1]["status"] == "armed"
        world.cancel_trigger(old)
        cut = world.arm_console_partition("source", b"CUT_MARKER", ["source"], ["peer"], match_timeout=1)
        assert exchange("peer:7000", "probe") == "PONG"  # Return completed before marker.
        assert exchange("source:7000", "emit CUT_MARKER") == "EMITTED"
        world.sleep(0.01)
        # Repeated marker must not create a second effect or handle.
        assert exchange("source:7000", "emit CUT_MARKER") == "EMITTED"
        state = world.inspect()
        trigger = state["triggers"][cut.id - 1]
        assert trigger["status"] == "fired" and trigger["fault_id"] == 1
        assert trigger["effect_at_ns"] > trigger["matched_at_ns"]
        assert state["active_handles"] == [cut.partition.handle]
        try:
            world.cancel_trigger(cut)
            raise AssertionError("matched trigger cancelled")
        except FaultError:
            pass
        assert exchange("peer:7000", "probe") == "BLOCKED"
        state = world.inspect()
        assert any(c["partition_dropped"] > 0 for c in state["network_counters"].values())
        world.heal(cut.partition)
        assert exchange("peer:7000", "probe") == "PONG"
        expired = world.arm_console_stop("source", b"MISSING_EXPIRE", "source", match_timeout=0.001)
        world.sleep(0.01)
        assert world.inspect()["triggers"][expired.id - 1]["status"] == "expired"
        # A real committed stop must wait for the guest's reap acknowledgement.
        stop = world.arm_console_stop("source", b"STOP_MARKER", "source", match_timeout=1)
        # Its process may stop before replying; the control link remains usable.
        try:
            exchange("source:7000", "emit STOP_MARKER")
        except OSError:
            pass
        world.sleep(0.01)
        state = world.inspect()
        assert state["triggers"][stop.id - 1]["status"] == "fired"
        assert any(m["name"] == "source" and m["state"] == "Stopped" for m in state["machines"])
        world.restart("source")
        pending = world.arm_console_stop("peer", b"NEVER_AT_CUTOFF", "peer", match_timeout=60)
        assert world.inspect()["triggers"][pending.id - 1]["status"] == "armed"
    print("committed console trigger controls passed", flush=True)


if __name__ == "__main__":
    if sys.argv[1] == "scenario":
        scenario()
    else:
        server(sys.argv[1])
