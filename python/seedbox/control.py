"""Blocking, dependency-free client for the Seedbox v1 guest control RPC."""
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation
import json
import socket

MAX_FRAME = 16_384
DEFAULT_SOCKET = "/run/seedbox/control.sock"


class ProtocolError(RuntimeError):
    """The peer disconnected or returned an invalid RPC response."""


class FaultError(RuntimeError):
    """An operation failed; the simulator's structured error is retained."""

    def __init__(self, code, message, response):
        super().__init__(f"{code}: {message}")
        self.code, self.response = code, response


@dataclass(frozen=True)
class Machine:
    """Explicit prepared-machine target; ordinary strings name services."""
    name: str


@dataclass(frozen=True)
class Effect:
    requested_at_ns: int
    admitted_at_ns: int
    effect_at_ns: int
    replied_at_ns: int


@dataclass(frozen=True)
class Partition:
    handle: str
    effect: Effect


@dataclass(frozen=True)
class Shape:
    handle: str
    effect: Effect


def _target(value):
    kind, name = ("machine", value.name) if isinstance(value, Machine) else ("service", value)
    if not isinstance(name, str) or not name:
        raise ValueError("targets must be nonempty service names or Machine objects")
    return {kind: name}


def _nanoseconds(seconds):
    if isinstance(seconds, bool):
        raise ValueError("sleep duration must be a nonnegative number of seconds")
    try:
        value = Decimal(str(seconds)) * 1_000_000_000
        if not value.is_finite() or value < 0 or value > 2**64 - 1 or value != value.to_integral_value():
            raise ValueError("sleep duration must fit whole unsigned 64-bit nanoseconds")
        return int(value)
    except InvalidOperation as error:
        raise ValueError("invalid sleep duration") from error


class World:
    """One sequential RPC connection inside a simulated guest.

    Calls block this thread until a confirmed response arrives. Other guests
    continue running. Use connect() or a context manager; do not share one
    connection across threads. Close between completed calls, then connect() a
    new World to start a new session with IDs beginning at one. Fault handles
    survive graceful disconnects. There are no automatic retries or implicit heals.
    """

    def __init__(self, channel):
        self._socket = channel
        self._reader = channel.makefile("rb")
        self._next_id = 1
        self._closed = False
        self.last_effect = None

    @classmethod
    def connect(cls, path=DEFAULT_SOCKET):
        """Open a new controller session; never retry an uncertain prior call."""
        channel = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            channel.connect(path)
            return cls(channel)
        except BaseException:
            channel.close()
            raise

    def close(self):
        if not self._closed:
            self._closed = True
            self._reader.close()
            self._socket.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def _call(self, op, **arguments):
        if self._closed:
            raise ProtocolError("control connection is closed")
        if self._next_id > 2**32 - 1:
            raise ProtocolError("request IDs exhausted")
        request_id = self._next_id
        request = dict(version=1, id=request_id, op=op, **arguments)
        frame = json.dumps(request, separators=(",", ":"), ensure_ascii=True, allow_nan=False).encode() + b"\n"
        if len(frame) > MAX_FRAME:
            raise ValueError("control request exceeds 16384-byte frame limit")
        self._next_id += 1
        try:
            self._socket.sendall(frame)
            line = self._reader.readline(MAX_FRAME + 1)
            if len(line) > MAX_FRAME or not line.endswith(b"\n"):
                raise ProtocolError("missing or oversized control response")
            response = json.loads(line.decode("utf-8"))
            if not isinstance(response, dict) or type(response.get("version")) is not int or response["version"] != 1:
                raise ProtocolError("unsupported control response version")
            if type(response.get("id")) is not int or response["id"] != request_id or type(response.get("ok")) is not bool:
                raise ProtocolError("control response correlation mismatch")
            times = [response.get(key) for key in ("requested_at_ns", "admitted_at_ns", "effect_at_ns", "replied_at_ns")]
            if any(type(value) is not int or not 0 <= value <= 2**64 - 1 for value in times) or times != sorted(times):
                raise ProtocolError("invalid control effect timing")
            effect = Effect(*times)
            self.last_effect = effect
            if not response["ok"]:
                error = response.get("error")
                if not isinstance(error, dict) or not isinstance(error.get("code"), str) or not isinstance(error.get("message"), str):
                    raise ProtocolError("invalid control error")
                raise FaultError(error["code"], error["message"], response)
            result = response.get("result")
            if not isinstance(result, dict):
                raise ProtocolError("invalid control result")
            return result, effect
        except (OSError, ValueError) as error:
            self.close()
            raise ProtocolError(f"control transport failed: {error}") from error
        except ProtocolError:
            self.close()
            raise

    def now_ns(self):
        result, effect = self._call("now")
        if type(result.get("now_ns")) is not int or result["now_ns"] != effect.effect_at_ns:
            self.close()
            raise ProtocolError("invalid simulated time result")
        return result["now_ns"]

    def inspect(self):
        """Inspect topology and simulator-owned state at the admission boundary."""
        return self._call("inspect")[0]

    def sleep(self, seconds):
        """Wait at least this many simulated seconds, plus RPC delivery latency."""
        return self._call("sleep", duration_ns=_nanoseconds(seconds))[1]

    def stop(self, target):
        """Return only after the application process group has been stopped."""
        return self._call("stop", target=_target(target))[1]

    def restart(self, target):
        """Return after successful exec; application readiness remains its own check."""
        return self._call("restart", target=_target(target))[1]

    def pause(self, target):
        """Freeze the target's whole machine; other machines keep running."""
        return self._call("pause", target=_target(target))[1]

    def resume(self, target):
        """Continue a paused machine; missed timer ticks fire once, late."""
        return self._call("resume", target=_target(target))[1]

    def crash(self, target):
        """Destroy the machine: RAM, sockets and queued I/O are lost; nothing is sent on its behalf."""
        return self._call("crash", target=_target(target))[1]

    def reboot(self, target):
        """Boot a crashed machine again from its image; readiness is the application's own check."""
        return self._call("reboot", target=_target(target))[1]

    def partition(self, left, right):
        """Cut traffic in both directions and return an independent handle."""
        if isinstance(left, str) or isinstance(right, str):
            raise ValueError("partition sides must be sequences of targets")
        result, effect = self._call("partition", left=[_target(t) for t in left], right=[_target(t) for t in right])
        if not isinstance(result.get("handle"), str) or not result["handle"]:
            self.close()
            raise ProtocolError("partition response lacks an opaque handle")
        return Partition(result["handle"], effect)

    def shape(self, from_, to, *, latency, jitter=0, loss_ppm=0):
        """Apply an absolute directed policy; latency and jitter are nanoseconds."""
        if isinstance(from_, str) or isinstance(to, str):
            raise ValueError("shape sides must be sequences of targets")
        for name, value, maximum in [("latency", latency, 2**64 - 1), ("jitter", jitter, 2**64 - 1), ("loss_ppm", loss_ppm, 1_000_000)]:
            if type(value) is not int or not 0 <= value <= maximum:
                raise ValueError(f"{name} must be an integer in [0, {maximum}]")
        if jitter >= latency or latency + jitter > 3_600_000_000_000:
            raise ValueError("jitter must be less than latency; their sum must not exceed one hour")
        result, effect = self._call("shape", **{"from": [_target(t) for t in from_], "to": [_target(t) for t in to], "latency_ns": latency, "jitter_ns": jitter, "loss_ppm": loss_ppm})
        if not isinstance(result.get("handle"), str) or not result["handle"]:
            self.close()
            raise ProtocolError("shape response lacks an opaque handle")
        return Shape(result["handle"], effect)

    def heal(self, partition):
        """Release this handle only; prior shape layers or overlapping cuts remain."""
        if not isinstance(partition, (Partition, Shape)):
            raise TypeError("heal expects the Partition or Shape returned by this API")
        return self._call("heal", handle=partition.handle)[1]


# Descriptive controller name; retain World for existing scenarios.
Controller = World
