"""Blocking, dependency-free client for the Seedbox v1 guest control RPC."""
from dataclasses import dataclass
from decimal import Decimal, InvalidOperation
import json
import socket
from typing import Optional

MAX_FRAME = 16_384
MAX_CLOCK_OFFSET_NS = 365 * 86_400 * 1_000_000_000
MAX_CLOCK_RATE_PPB = 100_000_000
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
class Trigger:
    id: int
    effect: Effect
    partition: Optional[Partition] = None


@dataclass(frozen=True)
class Shape:
    handle: str
    effect: Effect


@dataclass(frozen=True)
class Throttle:
    handle: str
    effect: Effect


@dataclass(frozen=True)
class ConnectionDrop:
    handle: str
    effect: Effect


@dataclass(frozen=True)
class ClockSkew:
    handle: str
    offset_ns: int
    effect: Effect


@dataclass(frozen=True)
class ClockDrift:
    handle: str
    rate_ppb: int
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

    def pause_process(self, target):
        """SIGSTOP the application's process group; its kernel keeps ACKing and accepting connections."""
        return self._call("pause_process", target=_target(target))[1]

    def resume_process(self, target):
        """SIGCONT a process group frozen by pause_process()."""
        return self._call("resume_process", target=_target(target))[1]

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

    def shape(self, from_, to, *, latency, jitter=0, loss_ppm=0, bandwidth_bps=None, queue_bound=None,
              reorder_ppm=0, reorder_delay=0, duplicate_ppm=0, burst_loss_ppm=0, burst_length=0,
              latency_spike_ppm=0, latency_spike=0, tcp_reset_ppm=0):
        """Apply an absolute directed policy; latency and jitter are nanoseconds."""
        if isinstance(from_, str) or isinstance(to, str):
            raise ValueError("shape sides must be sequences of targets")
        for name, value, maximum in [("latency", latency, 2**64 - 1), ("jitter", jitter, 2**64 - 1), ("loss_ppm", loss_ppm, 1_000_000)]:
            if type(value) is not int or not 0 <= value <= maximum:
                raise ValueError(f"{name} must be an integer in [0, {maximum}]")
        if jitter >= latency or latency + jitter > 3_600_000_000_000:
            raise ValueError("jitter must be less than latency; their sum must not exceed one hour")
        policy = {"from": [_target(t) for t in from_], "to": [_target(t) for t in to], "latency_ns": latency, "jitter_ns": jitter, "loss_ppm": loss_ppm}
        if bandwidth_bps is not None:
            if type(bandwidth_bps) is not int or not 1 <= bandwidth_bps <= 2**64 - 1:
                raise ValueError("bandwidth_bps must be a positive u64 integer")
            policy["bandwidth_bps"] = bandwidth_bps
        if queue_bound is not None:
            if bandwidth_bps is None or type(queue_bound) is not int or not 0 <= queue_bound <= 2**64 - 1:
                raise ValueError("queue_bound must be a nonnegative u64 integer and requires bandwidth_bps")
            policy["queue_bound_ns"] = queue_bound
        faults = dict(reorder_ppm=reorder_ppm, reorder_delay_ns=reorder_delay,
                      duplicate_ppm=duplicate_ppm, burst_loss_ppm=burst_loss_ppm,
                      burst_length=burst_length, latency_spike_ppm=latency_spike_ppm,
                      latency_spike_ns=latency_spike, tcp_reset_ppm=tcp_reset_ppm)
        for name, value in faults.items():
            maximum = 1_000_000 if name.endswith("_ppm") else 2**64 - 1
            if type(value) is not int or not 0 <= value <= maximum:
                raise ValueError(f"{name} must be an integer in [0, {maximum}]")
        for probability, amount in [(reorder_ppm, reorder_delay), (burst_loss_ppm, burst_length),
                                    (latency_spike_ppm, latency_spike)]:
            if probability and not amount:
                raise ValueError("active reorder, burst loss and latency spikes require a positive delay or length")
        if latency + jitter + (reorder_delay if reorder_ppm else 0) + (latency_spike if latency_spike_ppm else 0) > 3_600_000_000_000:
            raise ValueError("latency, jitter and fault delays must not exceed one hour")
        if any(value for name, value in faults.items() if name.endswith("_ppm")):
            policy["network_faults"] = faults
        result, effect = self._call("shape", **policy)
        if not isinstance(result.get("handle"), str) or not result["handle"]:
            self.close()
            raise ProtocolError("shape response lacks an opaque handle")
        return Shape(result["handle"], effect)

    def connections(self, from_, to, *, after=0):
        """Return up to 32 open directional incarnations and a next_after cursor.

        Discovery is a live admission-time page, not a frozen inventory.
        """
        if type(after) is not int or not 0 <= after <= 2**64 - 1:
            raise ValueError("after must be an unsigned 64-bit incarnation")
        return self._call("connections", **{"from": _target(from_), "to": _target(to), "after": after})[0]

    def drop_connection(self, connection):
        """Arm loss for one discovered direction/incarnation; heal reports drop evidence.

        Arming does not prove a packet was dropped. Inspect retained selection_counts
        or the heal result and require dropped > 0 in the scenario's own check.
        """
        if not isinstance(connection, dict):
            raise TypeError("connection must be an entry returned by connections")
        result, effect = self._call("drop_connection", connection=connection)
        if not isinstance(result.get("handle"), str) or not result["handle"]:
            self.close()
            raise ProtocolError("drop_connection response lacks an opaque handle")
        return ConnectionDrop(result["handle"], effect)

    def clock_skew(self, target, *, offset=None, max_offset=None):
        """Step the target machine's wall clock (CLOCK_REALTIME); monotonic time is untouched.

        Pass exactly one of a fixed nonzero signed ``offset`` or a positive
        ``max_offset``, both nanoseconds. With ``max_offset`` the simulator
        draws the offset uniformly from [-max_offset, max_offset] on the
        machine's seeded clock stream. Returns after the guest has stepped its
        clock; heal() steps it back by the same offset.
        """
        if (offset is None) == (max_offset is None):
            raise ValueError("clock_skew needs exactly one of offset or max_offset")
        value = offset if offset is not None else max_offset
        if type(value) is not int or value == 0 or abs(value) > MAX_CLOCK_OFFSET_NS or (max_offset is not None and value < 0):
            raise ValueError(f"offset must be a nonzero integer and max_offset positive, each at most {MAX_CLOCK_OFFSET_NS} ns")
        arguments = {"target": _target(target)}
        arguments["offset_ns" if offset is not None else "max_offset_ns"] = value
        result, effect = self._call("clock_skew", **arguments)
        if not isinstance(result.get("handle"), str) or not result["handle"] or type(result.get("offset_ns")) is not int:
            self.close()
            raise ProtocolError("clock_skew response lacks an opaque handle or offset")
        return ClockSkew(result["handle"], result["offset_ns"], effect)

    def clock_drift(self, target, *, ppb=None, max_ppb=None):
        """Run the target machine's clock fast (or slow if negative) by a rate error.

        Pass exactly one of a fixed nonzero signed ``ppb`` or a positive
        ``max_ppb``, in parts per billion, each at most 10%. With ``max_ppb``
        the simulator draws the rate uniformly from [-max_ppb, max_ppb] on the
        machine's seeded clock stream. Realtime and monotonic time drift
        together; CLOCK_MONOTONIC_RAW keeps the simulated rate. A machine has
        one drift at a time. Returns after the guest has set its rate;
        heal() restores the nominal rate.
        """
        if (ppb is None) == (max_ppb is None):
            raise ValueError("clock_drift needs exactly one of ppb or max_ppb")
        value = ppb if ppb is not None else max_ppb
        if type(value) is not int or value == 0 or abs(value) > MAX_CLOCK_RATE_PPB or (max_ppb is not None and value < 0):
            raise ValueError(f"ppb must be a nonzero integer and max_ppb positive, each at most {MAX_CLOCK_RATE_PPB}")
        arguments = {"target": _target(target)}
        arguments["rate_ppb" if ppb is not None else "max_rate_ppb"] = value
        result, effect = self._call("clock_drift", **arguments)
        if not isinstance(result.get("handle"), str) or not result["handle"] or type(result.get("rate_ppb")) is not int:
            self.close()
            raise ProtocolError("clock_drift response lacks an opaque handle or rate")
        return ClockDrift(result["handle"], result["rate_ppb"], effect)

    def throttle(self, target, factor):
        """Make the target's machine `factor` times slower until healed (x86 pv timebase).

        Simulated CPU work (each syscall's charge) costs `factor` times the
        machine's seeded speed; factors run from 1 to 1000 in steps of 0.001.
        """
        if isinstance(factor, bool):
            raise ValueError("throttle factor must be a number from 1 to 1000")
        try:
            permille = Decimal(str(factor)) * 1000
        except InvalidOperation as error:
            raise ValueError("invalid throttle factor") from error
        if not permille.is_finite() or permille != permille.to_integral_value() or not 1000 <= permille <= 1_000_000:
            raise ValueError("throttle factor must be from 1 to 1000 in steps of 0.001")
        result, effect = self._call("throttle", target=_target(target), factor_permille=int(permille))
        if not isinstance(result.get("handle"), str) or not result["handle"]:
            self.close()
            raise ProtocolError("throttle response lacks an opaque handle")
        return Throttle(result["handle"], effect)

    def heal(self, partition):
        """Release this handle only; prior shape layers or overlapping cuts remain."""
        if not isinstance(partition, (Partition, Shape, ConnectionDrop, ClockSkew, ClockDrift, Throttle)):
            raise TypeError("heal expects a fault handle returned by this API")
        result, effect = self._call("heal", handle=partition.handle)
        return result if isinstance(partition, ConnectionDrop) or "queue_counts" in result or "network_fault_counts" in result else effect


    def _arm_console(self, source, marker, match_timeout, fault):
        if not isinstance(marker, bytes) or not 1 <= len(marker) <= 256:
            raise ValueError("marker must contain 1..=256 bytes")
        timeout = _nanoseconds(match_timeout)
        if timeout == 0:
            raise ValueError("match timeout must be positive")
        result, effect = self._call("arm_trigger", source=_target(source),
                                    marker=list(marker), match_timeout_ns=timeout, fault=fault)
        trigger_id = result.get("trigger_id")
        if type(trigger_id) is not int or not 1 <= trigger_id <= 64:
            self.close()
            raise ProtocolError("arm response lacks a bounded trigger ID")
        partition = None
        if fault["action"] == "partition":
            handle = result.get("handle")
            if not isinstance(handle, str) or not handle:
                self.close()
                raise ProtocolError("arm response lacks a partition handle")
            partition = Partition(handle, effect)
        return Trigger(trigger_id, effect, partition)

    def arm_console_stop(self, source, marker, target, *, match_timeout):
        """Arm once on future console bytes; confirmation means armed, not stopped."""
        return self._arm_console(source, marker, match_timeout,
                                 {"action": "stop", "target": _target(target)})

    def arm_console_partition(self, source, marker, left, right, *, match_timeout):
        """Arm a future partition. Its handle can be healed after the effect."""
        if isinstance(left, str) or isinstance(right, str):
            raise ValueError("partition sides must be sequences of targets")
        return self._arm_console(source, marker, match_timeout,
                                 {"action": "partition", "left": [_target(t) for t in left],
                                  "right": [_target(t) for t in right]})

    def cancel_trigger(self, trigger):
        """Cancel at admission; a matched, fired, or expired trigger cannot cancel."""
        if not isinstance(trigger, Trigger):
            raise TypeError("cancel expects the Trigger returned by arm_console_*()")
        return self._call("cancel_trigger", trigger_id=trigger.id)[1]


# Descriptive controller name; retain World for existing scenarios.
Controller = World
