"""Process pause, then whole-machine pause, crash and reboot of a real guest kernel."""
import json
import socket
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


def boot():
    reply = exchange("server:7000", "BOOT").split()
    assert reply[0] == "BOOT" and reply[2].startswith("marked="), reply
    return reply[1], reply[2] == "marked=true"


def state(world):
    return next(machine["state"] for machine in world.inspect()["machines"] if machine["name"] == "server")


class Session:
    """One established TCP connection that outlives the machine operations."""

    def __init__(self):
        self.channel = socket.create_connection(("server", 7000), 2)
        self.pending = bytearray()

    def send(self, command):
        self.channel.sendall(command.encode() + b"\n")

    def line(self, timeout):
        self.channel.settimeout(timeout)
        while b"\n" not in self.pending:
            data = self.channel.recv(4096)
            if not data:
                raise ConnectionError("peer closed the connection")
            self.pending.extend(data)
        line, _, rest = bytes(self.pending).partition(b"\n")
        self.pending = bytearray(rest)
        return line.decode()

    def silent(self, timeout):
        """The peer neither answers, closes (FIN) nor resets (RST)."""
        try:
            line = self.line(timeout)
        except socket.timeout:
            return True
        raise AssertionError(f"unexpected reply {line!r}")

    def close(self):
        self.channel.close()


def run(address, no_faults=False, batch=None):
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
        first_boot, marked = boot()
        assert not marked
        assert exchange("server:7000", "MARK") == "MARKED"
        assert boot() == (first_boot, True)
        rejected(world.resume, "invalid_state")
        rejected(world.reboot, "invalid_state")

        # A frozen process's kernel keeps the connection up and still accepts
        # new ones; the request hangs and completes after SIGCONT.
        frozen = Session()
        frozen.send("PING")
        assert frozen.line(2) == "READY socket_absent"
        world.pause_process("server")
        assert state(world) == "Frozen"
        rejected(world.pause_process, "invalid_state")
        frozen.send("PING")
        assert frozen.silent(0.5)
        queued = Session()
        queued.send("PING")
        assert queued.silent(0.5)
        world.resume_process("server")
        assert frozen.line(2) == "READY socket_absent"
        assert queued.line(2) == "READY socket_absent"
        frozen.close()
        queued.close()
        assert state(world) == "Running"

        held = Session()
        held.send("PING")
        assert held.line(2) == "READY socket_absent"
        world.pause("server")
        assert state(world) == "Paused"
        rejected(world.pause, "invalid_state")
        rejected(world.stop, "invalid_state")
        held.send("PING")
        assert held.silent(0.5)
        resumed = world.resume("server")
        assert held.line(2) == "READY socket_absent"
        assert world.now_ns() > resumed.effect_at_ns
        held.close()
        assert state(world) == "Running"
        assert boot() == (first_boot, True)

        lost = Session()
        lost.send("PING")
        assert lost.line(2) == "READY socket_absent"
        world.crash("server")
        assert state(world) == "Down"
        rejected(world.crash, "invalid_state")
        rejected(world.resume, "invalid_state")
        rejected(world.stop, "target_unavailable")
        lost.send("PING")
        assert lost.silent(0.5)
        lost.close()
        world.reboot("server")
        ready()
        second_boot, marked = boot()
        assert second_boot != first_boot and not marked
        assert state(world) == "Running"
    result = {"errors": errors, "hung_while_frozen": True, "held_until_resume": True, "silent_while_down": True,
              "fresh_boot": True}
    print("LIFECYCLE_CASES_RESULT " + json.dumps(result, sort_keys=True), flush=True)
    return 0
