"""Deterministic resource state and supported Linux x86-64 syscall semantics."""

from collections import deque
from dataclasses import dataclass, field
import ipaddress
import posixpath
import struct
from typing import TYPE_CHECKING

import unicorn as uc

from .backend import MAX_IO, Process, SyscallRequest, Thread
from . import linux_errno as errno
from .errors import GuestMemoryError, SimulationError, UnsupportedGuestError

if TYPE_CHECKING:
    from .runtime import Simulation, Wait

MAX_FILE = 32 * 1024 * 1024


@dataclass(frozen=True)
class Complete:
    value: int


@dataclass(frozen=True)
class Blocked:
    kind: str
    data: dict
    deadline: int | None = None


@dataclass
class File:
    contents: bytearray
    durable: bytes


@dataclass
class FileHandle:
    path: str
    offset: int
    flags: int


@dataclass
class Socket:
    identity: int
    nonblocking: bool = False
    bound: tuple[str, int] | None = None
    peer: tuple[str, int] | None = None
    packets: deque = field(default_factory=deque)
    waiters: deque = field(default_factory=deque)


@dataclass
class Node:
    process: Process
    ip: str
    files: dict[str, File]
    descriptors: dict[int, FileHandle | Socket] = field(default_factory=dict)
    stdout: bytearray = field(default_factory=bytearray)
    stderr: bytearray = field(default_factory=bytearray)
    stdin: bytes = b""
    socket_sequence: int = 0

    def allocate(self, resource: FileHandle | Socket) -> int:
        for fd in range(3, 1024):
            if fd not in self.descriptors:
                self.descriptors[fd] = resource
                return fd
        raise SimulationError("guest descriptor limit exceeded")


class Kernel:
    def __init__(self, simulation: "Simulation"):
        self.sim = simulation
        self.bindings: dict[tuple[str, int], tuple[str, Socket]] = {}
        self.futexes: dict[tuple[str, int], deque[int]] = {}

    def handle(self, node: Node, thread: Thread, req: SyscallRequest) -> Complete | Blocked:
        try:
            return self._handle(node, thread, req)
        except GuestMemoryError:
            return Complete(-errno.EFAULT)

    def _handle(self, node: Node, thread: Thread, req: SyscallRequest) -> Complete | Blocked:
        n, a, process = req.number, req.args, node.process
        if n in (39, 186):
            return Complete(process.pid if n == 39 else thread.tid)
        if n == 24:  # sched_yield: a controlled stop already hands control back.
            return Complete(0)
        if n == 218:  # set_tid_address
            if a[0]:
                process.check_memory(a[0], 4, uc.UC_PROT_WRITE)
            thread.clear_tid = a[0]
            return Complete(thread.tid)
        if n == 228:  # clock_gettime
            if a[0] not in (0, 1):
                return Complete(-errno.EINVAL)
            now = self.sim.now + (self.sim.config.epoch_ns if a[0] == 0 else 0)
            process.write(a[1], struct.pack("<qq", *divmod(now, 10**9)))
            return Complete(0)
        if n == 318:  # getrandom
            if a[2] & ~3 or a[1] > MAX_IO:
                return Complete(-errno.EINVAL)
            process.check_memory(a[0], a[1], uc.UC_PROT_WRITE)
            data = self.sim.stream(f"entropy:{process.name}").read(a[1])
            process.write(a[0], data)
            return Complete(len(data))
        if n == 35:  # nanosleep
            seconds, ns = struct.unpack("<qq", process.read(a[0], 16))
            if seconds < 0 or not 0 <= ns < 10**9 or seconds > 10**8:
                return Complete(-errno.EINVAL)
            if a[1]:
                process.check_memory(a[1], 16, uc.UC_PROT_WRITE)
            return Blocked("sleep", {}, self.sim.now + seconds * 10**9 + ns)
        if n == 202:
            return self.futex(node, req)
        if n == 257:  # openat
            if a[0] != 2**64 - 100:
                return Complete(-errno.EBADF)
            path = self.path(process.cstring(a[1]))
            flags = a[2]
            if flags & ~(3 | 0x40 | 0x80 | 0x200 | 0x400 | 0x80000):
                raise UnsupportedGuestError("unsupported openat flags")
            if flags & 3 == 3:
                return Complete(-errno.EINVAL)
            if flags & 0x200 and flags & 3 == 0:
                return Complete(-errno.EACCES)
            if path in node.files and flags & 0x40 and flags & 0x80:
                return Complete(-errno.EEXIST)
            if path not in node.files:
                if not flags & 0x40:
                    return Complete(-errno.ENOENT)
                node.files[path] = File(bytearray(), b"")
            if flags & 0x200:
                node.files[path].contents.clear()
            return Complete(node.allocate(FileHandle(path, 0, flags)))
        if n == 3:  # close
            resource = node.descriptors.pop(a[0], None)
            if resource is None:
                return Complete(-errno.EBADF)
            if isinstance(resource, Socket):
                if resource.bound:
                    self.bindings.pop(resource.bound, None)
                for wait in list(resource.waiters):
                    self.sim.wake(wait, -errno.EBADF)
            return Complete(0)
        if n == 8:  # lseek
            handle = node.descriptors.get(a[0])
            if not isinstance(handle, FileHandle):
                return Complete(-errno.ESPIPE if isinstance(handle, Socket) else -errno.EBADF)
            offset = signed(a[1])
            if a[2] not in (0, 1, 2):
                return Complete(-errno.EINVAL)
            base = (0, handle.offset, len(node.files[handle.path].contents))[a[2]]
            if base + offset < 0 or base + offset > MAX_FILE:
                return Complete(-errno.EINVAL)
            handle.offset = base + offset
            return Complete(handle.offset)
        if n in (0, 1, 74):
            return self.file_operation(node, req)
        if n == 41:  # socket: IPv4 datagram only.
            if a[0] != 2 or a[1] & ~0x80800 != 2 or a[2] not in (0, 17):
                raise UnsupportedGuestError("only IPv4 UDP sockets are supported")
            node.socket_sequence += 1
            return Complete(node.allocate(Socket(node.socket_sequence, bool(a[1] & 0x800))))
        if n in (42, 49):
            if a[2] < 16:
                return Complete(-errno.EINVAL)
            resource = node.descriptors.get(a[0])
            if not isinstance(resource, Socket):
                return Complete(-errno.ENOTSOCK)
            target = self.sockaddr(process, a[1], a[2])
            if n == 42:
                resource.peer = target
                self.bind(node, resource, (node.ip, 0)) if resource.bound is None else None
                return Complete(0)
            return Complete(self.bind(node, resource, target))
        if n == 44:
            return self.send(node, req)
        if n == 45:
            return self.receive(node, req)
        raise UnsupportedGuestError(f"unsupported syscall {n}")

    @staticmethod
    def path(path: str) -> str:
        return posixpath.normpath("/" + path.lstrip("/"))

    def file_operation(self, node: Node, req: SyscallRequest) -> Complete | Blocked:
        n, a, process = req.number, req.args, node.process
        fd = a[0]
        if n == 0 and fd == 0:
            data = node.stdin[:min(a[2], MAX_IO)]
            process.write(a[1], data)
            node.stdin = node.stdin[len(data):]
            return Complete(len(data))
        if n == 1 and fd in (1, 2):
            data = process.read(a[1], a[2])
            target = node.stdout if fd == 1 else node.stderr
            if len(target) + len(data) > MAX_FILE:
                raise SimulationError("guest output limit exceeded")
            target.extend(data)
            self.sim.emit("output", node=process.name, fd=fd, data=data.hex())
            return Complete(len(data))
        handle = node.descriptors.get(fd)
        if isinstance(handle, Socket):
            if n == 0:
                return self.receive(node, req, read=True)
            if n == 1:
                return self.send(node, req, write=True)
            return Complete(-errno.EINVAL)
        if not isinstance(handle, FileHandle):
            return Complete(-errno.EBADF)
        if n == 0 and handle.flags & 3 == 1 or n == 1 and handle.flags & 3 == 0:
            return Complete(-errno.EBADF)
        if n in (0, 1) and a[2] == 0:
            return Complete(0)
        data = {"op": n, "handle": handle}
        if n == 0:
            process.check_memory(a[1], a[2], uc.UC_PROT_WRITE)
            data.update(address=a[1], length=a[2])
        elif n == 1:
            data["bytes"] = process.read(a[1], a[2])
        return Blocked("disk", data, self.sim.now + self.sim.config.disk.latency_ns)

    def finish_disk(self, wait: "Wait") -> int:
        node = self.sim.nodes[wait.node]
        data, handle = wait.data, wait.data["handle"]
        if self.sim.stream(f"disk:{wait.node}").below(10**6) < self.sim.config.disk.error_ppm:
            self.sim.emit("disk_fault", node=wait.node, op=data["op"])
            return -errno.EIO
        file = node.files[handle.path]
        if data["op"] == 0:
            value = bytes(file.contents[handle.offset:handle.offset + data["length"]])
            node.process.write(data["address"], value)
            handle.offset += len(value)
            return len(value)
        if data["op"] == 1:
            offset = len(file.contents) if handle.flags & 0x400 else handle.offset
            payload = data["bytes"]
            end = offset + len(payload)
            if end > MAX_FILE:
                return -errno.EFBIG
            if end > len(file.contents):
                file.contents.extend(b"\0" * (end - len(file.contents)))
            file.contents[offset:end] = payload
            handle.offset = end
            return len(payload)
        file.durable = bytes(file.contents)
        return 0

    def futex(self, node: Node, req: SyscallRequest) -> Complete | Blocked:
        address, operation, value, timeout, _second, _value3 = req.args
        if address % 4:
            return Complete(-errno.EINVAL)
        if operation not in (0, 1, 128, 129):
            raise UnsupportedGuestError("unsupported futex operation")
        current = int.from_bytes(node.process.read(address, 4), "little")
        if operation & 127 == 1:
            return Complete(self.wake_futex(node.process.name, address, value))
        if current != value & 0xFFFFFFFF:
            return Complete(-errno.EAGAIN)
        deadline = None
        if timeout:
            seconds, ns = struct.unpack("<qq", node.process.read(timeout, 16))
            if seconds < 0 or not 0 <= ns < 10**9 or seconds > 10**8:
                return Complete(-errno.EINVAL)
            deadline = self.sim.now + seconds * 10**9 + ns
        return Blocked("futex", {"address": address}, deadline)

    def wake_futex(self, name: str, address: int, maximum: int) -> int:
        queue = self.futexes.get((name, address), deque())
        count = 0
        while queue and count < maximum:
            if self.sim.wake(queue.popleft(), 0):
                count += 1
        return count

    def register_wait(self, wait: "Wait") -> None:
        if wait.kind == "futex":
            self.futexes.setdefault((wait.node, wait.data["address"]), deque()).append(wait.identity)
        elif wait.kind == "socket":
            wait.data["socket"].waiters.append(wait.identity)

    @staticmethod
    def sockaddr(process: Process, address: int, length: int) -> tuple[str, int]:
        if length < 16:
            raise GuestMemoryError("short sockaddr")
        value = process.read(address, 16)
        if int.from_bytes(value[:2], "little") != 2:
            raise UnsupportedGuestError("only IPv4 sockaddr is supported")
        return str(ipaddress.IPv4Address(value[4:8])), int.from_bytes(value[2:4], "big")

    def bind(self, node: Node, socket: Socket, target: tuple[str, int]) -> int:
        address, port = target
        if socket.bound:
            return -errno.EINVAL
        if address not in (node.ip, "0.0.0.0"):
            return -errno.EADDRNOTAVAIL
        if not port:
            port = next((p for p in range(49152, 65536) if (node.ip, p) not in self.bindings), 0)
            if not port:
                return -errno.EADDRINUSE
        key = (node.ip, port)
        if key in self.bindings:
            return -errno.EADDRINUSE
        self.bindings[key] = node.process.name, socket
        socket.bound = key
        return 0

    def send(self, node: Node, req: SyscallRequest, write: bool = False) -> Complete:
        a, process = req.args, node.process
        socket = node.descriptors.get(a[0])
        if not isinstance(socket, Socket):
            return Complete(-errno.ENOTSOCK)
        if not write and a[3] & ~0x40:
            raise UnsupportedGuestError("unsupported sendto flags")
        if a[2] > 65507:
            return Complete(-errno.EMSGSIZE)
        if not write and a[4] and a[5] < 16:
            return Complete(-errno.EINVAL)
        payload = process.read(a[1], a[2])
        destination = self.sockaddr(process, a[4], a[5]) if not write and a[4] else socket.peer
        if destination is None:
            return Complete(-errno.EDESTADDRREQ)
        if socket.bound is None:
            result = self.bind(node, socket, (node.ip, 0))
            if result:
                return Complete(result)
        self.sim.send_packet(process.name, socket.bound, destination, payload)
        return Complete(len(payload))

    def receive(self, node: Node, req: SyscallRequest, read: bool = False) -> Complete | Blocked:
        a, process = req.args, node.process
        socket = node.descriptors.get(a[0])
        if not isinstance(socket, Socket):
            return Complete(-errno.ENOTSOCK)
        if not read and a[3] & ~0x40:
            raise UnsupportedGuestError("unsupported recvfrom flags")
        process.check_memory(a[1], a[2], uc.UC_PROT_WRITE)
        if not read and a[4]:
            length = int.from_bytes(process.read(a[5], 4), "little")
            process.check_memory(a[4], min(length, 16), uc.UC_PROT_WRITE)
            process.check_memory(a[5], 4, uc.UC_PROT_WRITE)
        data = {"socket": socket, "address": a[1], "length": a[2],
                "peer": a[4] if not read else 0, "peer_length": a[5] if not read else 0}
        if socket.packets:
            return Complete(self.consume_packet(node, data, socket.packets.popleft()))
        if socket.nonblocking or not read and a[3] & 0x40:
            return Complete(-errno.EAGAIN)
        return Blocked("socket", data)

    @staticmethod
    def consume_packet(node: Node, data: dict, packet: tuple) -> int:
        source, payload = packet
        value = payload[:data["length"]]
        node.process.write(data["address"], value)
        if data["peer"]:
            old_length = int.from_bytes(node.process.read(data["peer_length"], 4), "little")
            sockaddr = struct.pack("<H", 2) + source[1].to_bytes(2, "big") + ipaddress.IPv4Address(source[0]).packed + b"\0" * 8
            node.process.write(data["peer"], sockaddr[:old_length])
            node.process.write(data["peer_length"], (16).to_bytes(4, "little"))
        return len(value)

    def deliver(self, packet: dict) -> None:
        binding = self.bindings.get(packet["destination"])
        if not binding:
            self.sim.emit("packet_drop", reason="unbound", destination=list(packet["destination"]))
            return
        name, socket = binding
        node = self.sim.nodes[name]
        if all(t.state == "exited" for t in node.process.threads.values()):
            self.sim.emit("packet_drop", reason="node_exited", destination=list(packet["destination"]))
            return
        item = packet["source"], packet["payload"]
        while socket.waiters:
            identity = socket.waiters.popleft()
            wait = self.sim.waits.get(identity)
            if wait is not None:
                try:
                    result = self.consume_packet(node, wait.data, item)
                except GuestMemoryError:
                    result = -errno.EFAULT
                self.sim.wake(identity, result)
                return
        if len(socket.packets) >= 1024:
            self.sim.emit("packet_drop", reason="receive_buffer_full", destination=list(packet["destination"]))
        else:
            socket.packets.append(item)


def signed(value: int) -> int:
    return value if value < 2**63 else value - 2**64
