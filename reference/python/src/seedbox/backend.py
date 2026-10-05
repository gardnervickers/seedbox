"""Instruction execution and CPU contexts; no guest host-syscall passthrough."""

from dataclasses import dataclass
from io import BytesIO
import struct
from typing import Any, Callable

import capstone
from elftools.elf.elffile import ELFFile
import unicorn as uc
from unicorn import x86_const as x86

from .errors import GuestMemoryError, SimulationError
from .random import RandomStream

PAGE = 4096
STACK_BASE = 0x700000000000
STACK_SIZE = 1024 * 1024
MAX_IO = 1024 * 1024

# These need explicit models before enabling them. Never let the emulator's
# implementation of an entropy, clock, privilege, or alternate syscall path run.
UNSUPPORTED_INSTRUCTIONS = {
    "rdtsc", "rdtscp", "rdrand", "rdseed", "rdpid", "rdpmc", "cpuid",
    "rdmsr", "wrmsr", "sysenter", "sysexit", "sysret", "sysretq",
    "int", "int1", "int3", "into", "iret", "iretd", "iretq", "hlt",
    "in", "insb", "insd", "insw", "out", "outsb", "outsd", "outsw",
    "xbegin", "xend", "xabort", "xtest", "xgetbv", "xsetbv",
    "rsm", "vmcall", "vmmcall", "monitor", "mwait", "umonitor", "umwait",
    "tpause", "rdfsbase", "rdgsbase", "wrfsbase", "wrgsbase",
}


@dataclass(frozen=True)
class SyscallRequest:
    number: int
    args: tuple[int, ...]
    pc: int
    size: int


@dataclass
class ExecutionStop:
    reason: str
    steps: int
    request: SyscallRequest | None = None
    detail: str | None = None


@dataclass
class Thread:
    tid: int
    context: Any
    state: str = "runnable"
    wait_id: int | None = None
    clear_tid: int = 0
    exit_code: int | None = None


class Process:
    def __init__(
        self, name: str, image: bytes, entropy: RandomStream, argv: list[str],
        pid: int,
    ):
        self.name = name
        self.pid = pid
        self.machine = uc.Uc(uc.UC_ARCH_X86, uc.UC_MODE_64)
        self.decoder = capstone.Cs(capstone.CS_ARCH_X86, capstone.CS_MODE_64)
        self.threads: dict[int, Thread] = {}
        self._stop: ExecutionStop | None = None
        self._steps = 0
        self._budget = 0
        self._instruction: Callable[[int, bytes], None] = lambda pc, code: None
        self._load(image, entropy, argv)
        self.machine.hook_add(uc.UC_HOOK_CODE, self._hook)
        self.threads[pid] = Thread(pid, self.machine.context_save())

    def _load(self, image: bytes, entropy: RandomStream, argv: list[str]) -> None:
        if len(image) > 32 * 1024 * 1024:
            raise SimulationError("ELF image exceeds 32 MiB")
        try:
            elf = ELFFile(BytesIO(image))
            if elf.elfclass != 64 or not elf.little_endian:
                raise SimulationError("only little-endian ELF64 is supported")
            if elf["e_machine"] != "EM_X86_64" or elf["e_type"] != "ET_EXEC":
                raise SimulationError("only static, fixed-address x86-64 ELF executables are supported")
            segments = list(elf.iter_segments())
            if any(s["p_type"] in {"PT_INTERP", "PT_DYNAMIC"} for s in segments):
                raise SimulationError("dynamic linking and PIE are not supported yet")
            loads = [s for s in segments if s["p_type"] == "PT_LOAD" and s["p_memsz"]]
            pages: dict[int, int] = {}
            phdr = 0
            for segment in loads:
                addr, size, filesz = segment["p_vaddr"], segment["p_memsz"], segment["p_filesz"]
                if filesz > size or size > 64 * 1024 * 1024:
                    raise SimulationError("invalid or oversized ELF segment")
                if addr < PAGE or addr + size >= STACK_BASE:
                    raise SimulationError("ELF segment outside supported address range")
                if segment["p_offset"] + filesz > len(image):
                    raise SimulationError("ELF segment exceeds image bounds")
                flags = segment["p_flags"]
                permissions = ((uc.UC_PROT_READ if flags & 4 else 0)
                               | (uc.UC_PROT_WRITE if flags & 2 else 0)
                               | (uc.UC_PROT_EXEC if flags & 1 else 0))
                for page in range(addr // PAGE * PAGE, (addr + size + PAGE - 1) // PAGE * PAGE, PAGE):
                    pages[page] = pages.get(page, 0) | permissions
                phoff = elf["e_phoff"]
                if segment["p_offset"] <= phoff < segment["p_offset"] + filesz:
                    phdr = addr + phoff - segment["p_offset"]
            if len(pages) * PAGE > 128 * 1024 * 1024:
                raise SimulationError("ELF mapped memory exceeds 128 MiB")
            for page in sorted(pages):
                self.machine.mem_map(page, PAGE, uc.UC_PROT_ALL)
            for segment in loads:
                self.machine.mem_write(segment["p_vaddr"], segment.data())
            for page, permissions in sorted(pages.items()):
                self.machine.mem_protect(page, PAGE, permissions)
            entry = elf["e_entry"]
            if not pages.get(entry // PAGE * PAGE, 0) & uc.UC_PROT_EXEC:
                raise SimulationError("ELF entry point is not executable")
            self.machine.mem_map(STACK_BASE, STACK_SIZE, uc.UC_PROT_READ | uc.UC_PROT_WRITE)
            self.machine.reg_write(x86.UC_X86_REG_RIP, entry)
            self.machine.reg_write(x86.UC_X86_REG_EFLAGS, 2)
            sp = STACK_BASE + STACK_SIZE
            pointers = []
            for arg in reversed(argv):
                data = arg.encode() + b"\0"
                if len(data) > 16384:
                    raise SimulationError("guest argument too long")
                sp -= len(data)
                self.machine.mem_write(sp, data)
                pointers.insert(0, sp)
            sp -= 16
            self.machine.mem_write(sp, entropy.read(16))
            random_pointer = sp
            auxv = [
                (3, phdr), (4, elf["e_phentsize"]), (5, elf["e_phnum"]),
                (6, PAGE), (9, entry), (11, 1000), (12, 1000), (13, 1000),
                (14, 1000), (23, 0), (25, random_pointer), (31, pointers[0]), (0, 0),
            ]
            words = [len(pointers), *pointers, 0, 0, *[word for pair in auxv for word in pair]]
            sp = (sp - len(words) * 8) & ~15
            self.machine.mem_write(sp, struct.pack("<" + "Q" * len(words), *words))
            self.machine.reg_write(x86.UC_X86_REG_RSP, sp)
        except SimulationError:
            raise
        except Exception as error:
            raise SimulationError(f"invalid ELF: {error}") from error

    def _hook(self, machine: uc.Uc, address: int, size: int, _data: Any) -> None:
        if self._steps >= self._budget:
            machine.emu_stop()
            return
        raw = bytes(machine.mem_read(address, size))
        decoded = next(self.decoder.disasm_lite(raw, address, count=1), None)
        if decoded is None:
            self._stop = ExecutionStop("fault", self._steps, detail=f"cannot decode instruction at {address:#x}")
            machine.emu_stop()
            return
        self._steps += 1
        self._instruction(address, raw)
        mnemonic = decoded[2]
        # REP prefixes are included in Capstone's mnemonic for string I/O.
        instruction = mnemonic.split()[-1]
        if instruction in UNSUPPORTED_INSTRUCTIONS:
            self._stop = ExecutionStop("unsupported", self._steps, detail=f"unsupported instruction {mnemonic} at {address:#x}")
            machine.emu_stop()
        elif instruction == "syscall":
            regs = [x86.UC_X86_REG_RDI, x86.UC_X86_REG_RSI, x86.UC_X86_REG_RDX,
                    x86.UC_X86_REG_R10, x86.UC_X86_REG_R8, x86.UC_X86_REG_R9]
            request = SyscallRequest(machine.reg_read(x86.UC_X86_REG_RAX),
                                     tuple(machine.reg_read(reg) for reg in regs), address, size)
            self._stop = ExecutionStop("syscall", self._steps, request=request)
            machine.emu_stop()

    def run(self, thread: Thread, budget: int, instruction: Callable[[int, bytes], None]) -> ExecutionStop:
        if budget <= 0 or thread.state != "runnable":
            raise SimulationError("backend requires a runnable thread and positive budget")
        self.machine.context_restore(thread.context)
        self._stop, self._steps, self._budget = None, 0, budget
        self._instruction = instruction
        try:
            self.machine.emu_start(self.machine.reg_read(x86.UC_X86_REG_RIP), 0, timeout=0, count=budget)
        except uc.UcError as error:
            if self._stop is None:
                self._stop = ExecutionStop("fault", self._steps, detail=str(error))
        thread.context = self.machine.context_save()
        self._instruction = lambda pc, code: None
        return self._stop or ExecutionStop("quantum", self._steps)

    def complete(self, thread: Thread, request: SyscallRequest, value: int) -> None:
        self.machine.context_restore(thread.context)
        self.machine.reg_write(x86.UC_X86_REG_RAX, value & (2**64 - 1))
        self.machine.reg_write(x86.UC_X86_REG_RCX, request.pc + request.size)
        self.machine.reg_write(x86.UC_X86_REG_R11, self.machine.reg_read(x86.UC_X86_REG_EFLAGS))
        self.machine.reg_write(x86.UC_X86_REG_RIP, request.pc + request.size)
        thread.context = self.machine.context_save()

    def clone(self, parent: Thread, tid: int, stack: int, tls: int | None) -> Thread:
        self.machine.context_restore(parent.context)
        self.machine.reg_write(x86.UC_X86_REG_RAX, 0)
        self.machine.reg_write(x86.UC_X86_REG_RSP, stack)
        if tls is not None:
            self.machine.reg_write(x86.UC_X86_REG_FS_BASE, tls)
        child = Thread(tid, self.machine.context_save())
        self.threads[tid] = child
        self.machine.context_restore(parent.context)
        return child

    def check_memory(self, address: int, length: int, permission: int) -> None:
        if length < 0 or length > MAX_IO or address + length > 2**64:
            raise GuestMemoryError("invalid guest memory range")
        end, cursor = address + length, address
        for low, high, flags in self.machine.mem_regions():
            if low <= cursor <= high:
                if not flags & permission:
                    raise GuestMemoryError("guest memory permission denied")
                cursor = min(end, high + 1)
                if cursor == end:
                    return
        if length:
            raise GuestMemoryError("unmapped guest memory")

    def read(self, address: int, length: int) -> bytes:
        self.check_memory(address, length, uc.UC_PROT_READ)
        return bytes(self.machine.mem_read(address, length)) if length else b""

    def write(self, address: int, data: bytes) -> None:
        self.check_memory(address, len(data), uc.UC_PROT_WRITE)
        if data:
            self.machine.mem_write(address, data)

    def cstring(self, address: int) -> str:
        data = bytearray()
        for offset in range(4096):
            char = self.read(address + offset, 1)
            if char == b"\0":
                try:
                    return data.decode("utf-8")
                except UnicodeDecodeError as error:
                    raise GuestMemoryError("invalid pathname encoding") from error
            data.extend(char)
        raise GuestMemoryError("unterminated pathname")
