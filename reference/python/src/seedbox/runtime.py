"""One seeded coordinator owns instruction order, time, and all completions."""

from dataclasses import asdict, dataclass
import hashlib
import heapq
import json
import platform
import struct

import capstone
import elftools
import unicorn
from unicorn import UC_PROT_WRITE

from . import __version__, implementation_sha256
from . import linux_errno as errno
from .backend import Process, SyscallRequest, Thread
from .config import NodeSpec, RunConfig, validate
from .errors import GuestMemoryError, SimulationError, UnsupportedGuestError
from .kernel import Blocked, Complete, File, Kernel, Node
from .random import RandomStream


def canonical(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()


@dataclass
class Wait:
    identity: int
    node: str
    tid: int
    request: SyscallRequest
    kind: str
    data: dict


class Simulation:
    def __init__(self, specs: list[NodeSpec], seed: int = 0, config: RunConfig | None = None,
                 instruction_trace: bool = False):
        self.config = config or RunConfig()
        validate(self.config, specs)
        self.seed = seed
        self.instruction_trace = instruction_trace
        self.now = 0
        self.instructions = 0
        self.trace: list[dict] = []
        self.instruction_hash = hashlib.sha256()
        self.streams: dict[str, RandomStream] = {}
        self.schedule_random = self.stream("schedule")
        self.nodes: dict[str, Node] = {}
        self.ip_names = {spec.ip: spec.name for spec in specs}
        self.waits: dict[int, Wait] = {}
        self.events: list[tuple[int, int, str, dict]] = []
        self.event_sequence = 0
        self.wait_sequence = 0
        self.tid_sequence = 0
        self.status = "ready"
        self.error: str | None = None
        self.kernel = Kernel(self)
        self.manifest = {
            "runtime": __version__, "backend": "unicorn-x86_64-v1",
            "runtime_sha256": implementation_sha256(),
            "instruction_trace": instruction_trace,
            "unicorn": unicorn.__version__, "capstone": capstone.__version__,
            "pyelftools": elftools.__version__, "python": platform.python_version(),
            "config": asdict(self.config),
            "nodes": [{"name": spec.name, "ip": spec.ip, "argv": list(spec.argv),
                       "image_sha256": hashlib.sha256(spec.image).hexdigest(),
                       "files": {path: data.hex() for path, data in sorted(spec.files.items())}}
                      for spec in specs],
        }
        for spec in specs:
            self.tid_sequence += 1
            process = Process(spec.name, spec.image, self.stream(f"entropy:{spec.name}"),
                              list(spec.argv), self.tid_sequence)
            files = {}
            for path, data in spec.files.items():
                normalized = self.kernel.path(path)
                if normalized in files:
                    raise ValueError("initial file paths normalize to the same path")
                files[normalized] = File(bytearray(data), data)
            self.nodes[spec.name] = Node(process, spec.ip, files)
            self.emit("start", node=spec.name, tid=process.pid, ip=spec.ip)

    def stream(self, domain: str) -> RandomStream:
        if domain not in self.streams:
            self.streams[domain] = RandomStream(self.seed, domain)
        return self.streams[domain]

    def emit(self, event: str, **data: object) -> None:
        if len(self.trace) >= 1_000_000:
            raise SimulationError("trace event limit exceeded")
        self.trace.append({"index": len(self.trace), "time_ns": self.now, "event": event, **data})

    def schedule(self, deadline: int, kind: str, data: dict) -> None:
        if not self.now <= deadline < 2**63:
            raise SimulationError("event deadline outside supported virtual time")
        self.event_sequence += 1
        heapq.heappush(self.events, (deadline, self.event_sequence, kind, data))

    def block(self, node: Node, thread: Thread, request: SyscallRequest, result: Blocked) -> None:
        self.wait_sequence += 1
        wait = Wait(self.wait_sequence, node.process.name, thread.tid, request, result.kind, result.data)
        self.waits[wait.identity] = wait
        thread.state, thread.wait_id = "blocked", wait.identity
        self.kernel.register_wait(wait)
        if result.deadline is not None:
            self.schedule(result.deadline, "completion", {"wait": wait.identity})
        self.emit("block", node=wait.node, tid=wait.tid, wait=wait.identity,
                  kind=wait.kind, deadline_ns=result.deadline)

    def complete(self, node: Node, thread: Thread, request: SyscallRequest, value: int) -> None:
        node.process.complete(thread, request, value)
        thread.state, thread.wait_id = "runnable", None
        self.emit("complete", node=node.process.name, tid=thread.tid, syscall=request.number, result=value)

    def wake(self, identity: int, value: int) -> bool:
        wait = self.waits.pop(identity, None)
        if wait is None:
            return False
        node = self.nodes[wait.node]
        thread = node.process.threads[wait.tid]
        if thread.state != "blocked" or thread.wait_id != identity:
            raise SimulationError("wait/thread state inconsistent")
        self.emit("wake", node=wait.node, tid=wait.tid, wait=identity, kind=wait.kind)
        self.complete(node, thread, wait.request, value)
        return True

    def exit(self, node: Node, thread: Thread, code: int, group: bool) -> None:
        targets = list(node.process.threads.values()) if group else [thread]
        for target in targets:
            if target.state == "exited":
                continue
            if target.wait_id is not None:
                self.waits.pop(target.wait_id, None)
            target.state, target.wait_id, target.exit_code = "exited", None, code & 255
            self.emit("exit", node=node.process.name, tid=target.tid, code=target.exit_code)
            if target.clear_tid:
                node.process.write(target.clear_tid, b"\0" * 4)
                self.kernel.wake_futex(node.process.name, target.clear_tid, 1)

    def clone(self, node: Node, thread: Thread, request: SyscallRequest) -> None:
        flags, stack, parent_tid, child_tid, tls, _ = request.args
        # Initial contract: true threads share VM, files, FS, and signal state.
        required = 0x50F00
        allowed = required | 0x80000 | 0x100000 | 0x200000 | 0x1000000
        if flags & required != required or flags & ~allowed:
            raise UnsupportedGuestError("unsupported clone flags; only shared-address-space threads are supported")
        if len(node.process.threads) >= 256:
            raise SimulationError("guest thread limit exceeded")
        try:
            node.process.check_memory(stack - 8, 8, UC_PROT_WRITE)
            if flags & 0x100000:
                node.process.check_memory(parent_tid, 4, UC_PROT_WRITE)
            if flags & (0x200000 | 0x1000000):
                node.process.check_memory(child_tid, 4, UC_PROT_WRITE)
        except GuestMemoryError:
            self.complete(node, thread, request, -errno.EFAULT)
            return
        self.tid_sequence += 1
        identity = self.tid_sequence
        if flags & 0x100000:
            node.process.write(parent_tid, identity.to_bytes(4, "little"))
        if flags & 0x1000000:
            node.process.write(child_tid, identity.to_bytes(4, "little"))
        self.complete(node, thread, request, identity)
        child = node.process.clone(thread, identity, stack, tls if flags & 0x80000 else None)
        if flags & 0x200000:
            child.clear_tid = child_tid
        self.emit("spawn", node=node.process.name, parent=thread.tid, tid=identity)

    def partitioned(self, source: str, destination: str, now: int) -> bool:
        return any(rule["source"] == source and rule["destination"] == destination
                   and rule["start_ns"] <= now < rule["end_ns"]
                   for rule in self.config.network.partitions)

    def send_packet(self, source_name: str, source: tuple[str, int], destination: tuple[str, int], payload: bytes) -> None:
        destination_name = self.ip_names.get(destination[0])
        self.emit("packet_send", node=source_name, source=list(source), destination=list(destination), data=payload.hex())
        if destination_name is None:
            self.emit("packet_drop", reason="outside_world", destination=list(destination))
            return
        cfg = self.config.network
        domain = "network:" + json.dumps([source_name, destination_name], separators=(",", ":"))
        rng = self.stream(domain)
        loss, duplicate = rng.below(10**6), rng.below(10**6)
        jitter = rng.below(2 * cfg.jitter_ns + 1) - cfg.jitter_ns
        if self.partitioned(source_name, destination_name, self.now) or loss < cfg.loss_ppm:
            self.emit("packet_drop", reason="partition" if self.partitioned(source_name, destination_name, self.now) else "loss",
                      destination=list(destination))
            return
        delay = max(0, cfg.latency_ns + jitter)
        packet = {"source_name": source_name, "destination_name": destination_name,
                  "source": source, "destination": destination, "payload": payload}
        self.schedule(self.now + delay, "packet", packet)
        if duplicate < cfg.duplicate_ppm:
            self.schedule(self.now + delay + 1, "packet", packet.copy())

    def deliver_due(self) -> None:
        while self.events and self.events[0][0] <= self.now:
            deadline, sequence, kind, data = heapq.heappop(self.events)
            if deadline != self.now:
                raise SimulationError("execution overran an event deadline")
            if kind == "packet":
                if self.partitioned(data["source_name"], data["destination_name"], self.now):
                    self.emit("packet_drop", reason="partition", destination=list(data["destination"]))
                else:
                    self.emit("packet_deliver", sequence=sequence, destination=list(data["destination"]), data=data["payload"].hex())
                    self.kernel.deliver(data)
            elif kind == "completion":
                wait = self.waits.get(data["wait"])
                if wait is None:
                    continue
                if wait.kind == "disk":
                    try:
                        value = self.kernel.finish_disk(wait)
                    except GuestMemoryError:
                        value = -errno.EFAULT
                elif wait.kind == "futex":
                    value = -errno.ETIMEDOUT
                elif wait.kind == "sleep":
                    value = 0
                else:
                    raise SimulationError("unexpected timed wait")
                self.wake(wait.identity, value)
            else:
                raise SimulationError("unknown event kind")

    def runnable(self) -> list[tuple[Node, Thread]]:
        return sorted(((node, thread) for node in self.nodes.values() for thread in node.process.threads.values()
                       if thread.state == "runnable"), key=lambda pair: pair[1].tid)

    def _discard_cancelled_events(self) -> None:
        # A completed futex's obsolete timeout must not advance virtual time.
        while self.events and self.events[0][2] == "completion" and self.events[0][3]["wait"] not in self.waits:
            heapq.heappop(self.events)

    def run(self) -> dict:
        if self.status != "ready":
            raise ValueError("create a fresh simulation for every run")
        self.status = "running"
        try:
            while True:
                self._discard_cancelled_events()
                self.deliver_due()
                threads = [t for n in self.nodes.values() for t in n.process.threads.values()]
                if all(t.state == "exited" for t in threads):
                    self.status = "completed" if all(t.exit_code == 0 for t in threads) else "guest_failure"
                    break
                runnable = self.runnable()
                if not runnable:
                    if not self.events:
                        self.status, self.error = "deadlock", "all live threads are blocked with no future event"
                        self.emit("deadlock", blocked=[{"node": w.node, "tid": w.tid, "kind": w.kind}
                                                      for w in self.waits.values()])
                        break
                    self.emit("advance", deadline_ns=self.events[0][0])
                    self.now = self.events[0][0]
                    continue
                remaining = self.config.max_instructions - self.instructions
                if remaining <= 0:
                    self.status, self.error = "instruction_limit", "guest instruction limit reached"
                    self.emit("instruction_limit", instructions=self.instructions)
                    break
                if self.events and self.events[0][0] - self.now < self.config.tick_ns:
                    self.now = self.events[0][0]
                    continue
                node, thread = runnable[self.schedule_random.below(len(runnable))]
                quantum = self.config.quantum_min + self.schedule_random.below(
                    self.config.quantum_max - self.config.quantum_min + 1)
                budget = min(quantum, remaining)
                if self.events:
                    budget = min(budget, (self.events[0][0] - self.now) // self.config.tick_ns)
                self.emit("dispatch", node=node.process.name, tid=thread.tid, budget=budget)
                start = self.now
                seen = 0

                def instruction(pc: int, raw: bytes) -> None:
                    nonlocal seen
                    name = node.process.name.encode()
                    self.instruction_hash.update(struct.pack("<I", len(name)) + name
                                                 + struct.pack("<QQI", thread.tid, pc, len(raw)) + raw)
                    if self.instruction_trace:
                        self.emit("instruction", node=node.process.name, tid=thread.tid, pc=pc,
                                  bytes=raw.hex(), time_ns=start + seen * self.config.tick_ns)
                    seen += 1

                stop = node.process.run(thread, budget, instruction)
                if not 0 <= stop.steps <= budget or seen != stop.steps or stop.steps == 0 and stop.reason not in ("fault", "unsupported"):
                    raise SimulationError("backend violated its instruction budget")
                self.instructions += stop.steps
                self.now += stop.steps * self.config.tick_ns
                self.emit("stop", node=node.process.name, tid=thread.tid, reason=stop.reason, instructions=stop.steps)
                if stop.reason in ("unsupported", "fault"):
                    self.status, self.error = stop.reason, stop.detail
                    break
                if stop.reason == "syscall":
                    request = stop.request
                    if request is None:
                        raise SimulationError("missing syscall request")
                    self.emit("syscall", node=node.process.name, tid=thread.tid,
                              number=request.number, args=list(request.args))
                    if request.number in (60, 231):
                        self.exit(node, thread, request.args[0], request.number == 231)
                    elif request.number == 56:
                        self.clone(node, thread, request)
                    else:
                        result = self.kernel.handle(node, thread, request)
                        if isinstance(result, Complete):
                            self.complete(node, thread, request, result.value)
                        elif isinstance(result, Blocked):
                            self.block(node, thread, request, result)
                        else:
                            raise SimulationError("kernel returned an invalid operation result")
        except SimulationError as error:
            self.status, self.error = "unsupported" if isinstance(error, UnsupportedGuestError) else "simulator_error", str(error)
        return self.report()

    def report(self) -> dict:
        return {
            "schema": 1, "seed": self.seed, "manifest": self.manifest,
            "manifest_sha256": hashlib.sha256(canonical(self.manifest)).hexdigest(),
            "status": self.status, "error": self.error, "virtual_time_ns": self.now,
            "instructions": self.instructions, "instruction_sha256": self.instruction_hash.hexdigest(),
            "trace_sha256": hashlib.sha256(canonical(self.trace)).hexdigest(),
            "nodes": {name: {
                "stdout_hex": node.stdout.hex(), "stderr_hex": node.stderr.hex(),
                "threads": {str(t.tid): {"state": t.state, "exit_code": t.exit_code}
                            for t in node.process.threads.values()},
                "files": {path: {"contents_hex": file.contents.hex(), "durable_hex": file.durable.hex()}
                          for path, file in sorted(node.files.items())},
            } for name, node in self.nodes.items()},
            "trace": self.trace,
        }
