//! Instruction engine and CPU contexts. Guest syscalls never reach the host.
use crate::{
    error::{Error, Result},
    random::RandomStream,
};
use goblin::elf::{
    Elf,
    header::{EM_X86_64, ET_EXEC},
    program_header::{PT_DYNAMIC, PT_INTERP, PT_LOAD},
};
use iced_x86::{Decoder, DecoderOptions, Mnemonic};
use std::collections::BTreeMap;
use unicorn_engine::{Arch, Context, Mode, Prot, RegisterX86 as Reg, Unicorn};

pub const PAGE: u64 = 4096;
pub const STACK_BASE: u64 = 0x700000000000;
pub const STACK_SIZE: u64 = 1024 * 1024;
pub const MAX_IO: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SyscallRequest {
    pub number: u64,
    pub args: [u64; 6],
    pub pc: u64,
    pub size: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Runnable,
    Blocked,
    Exited,
}
impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runnable => "runnable",
            Self::Blocked => "blocked",
            Self::Exited => "exited",
        }
    }
}
pub struct Thread {
    pub tid: u64,
    pub context: Context,
    pub state: State,
    pub wait_id: Option<u64>,
    pub clear_tid: u64,
    pub exit_code: Option<u8>,
    pub signal_mask: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Quantum,
    Syscall,
    Unsupported,
    Fault,
}
impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quantum => "quantum",
            Self::Syscall => "syscall",
            Self::Unsupported => "unsupported",
            Self::Fault => "fault",
        }
    }
}
pub struct EnteredInstruction {
    pub pc: u64,
    pub bytes: Vec<u8>,
}
pub struct ExecutionStop {
    pub reason: StopReason,
    pub steps: u64,
    pub request: Option<SyscallRequest>,
    pub detail: Option<String>,
    pub entered: Vec<EnteredInstruction>,
}
#[derive(Default)]
struct HookState {
    budget: u64,
    steps: u64,
    reason: Option<StopReason>,
    request: Option<SyscallRequest>,
    detail: Option<String>,
    entered: Vec<EnteredInstruction>,
}

pub struct Process {
    pub name: String,
    pub pid: u64,
    pub threads: BTreeMap<u64, Thread>,
    machine: Unicorn<'static, HookState>,
    brk_start: u64,
    brk_end: u64,
    mmap_next: u64,
}

fn unsupported(m: Mnemonic) -> bool {
    use Mnemonic::*;
    matches!(
        m,
        Rdtsc
            | Rdtscp
            | Rdrand
            | Rdseed
            | Rdpid
            | Rdpmc
            | Cpuid
            | Rdmsr
            | Wrmsr
            | Sysenter
            | Sysexit
            | Sysret
            | Sysretq
            | Int
            | Int1
            | Int3
            | Into
            | Iret
            | Iretd
            | Iretq
            | Hlt
            | In
            | Insb
            | Insd
            | Insw
            | Out
            | Outsb
            | Outsd
            | Outsw
            | Xbegin
            | Xend
            | Xabort
            | Xtest
            | Xgetbv
            | Xsetbv
            | Rsm
            | Vmcall
            | Vmmcall
            | Monitor
            | Mwait
            | Umonitor
            | Umwait
            | Tpause
            | Rdfsbase
            | Rdgsbase
            | Wrfsbase
            | Wrgsbase
    )
}

fn hook(machine: &mut Unicorn<HookState>, address: u64, size: u32) -> Result<()> {
    if machine.get_data().steps >= machine.get_data().budget {
        machine.emu_stop()?;
        return Ok(());
    }
    if size == 0 || size > 15 {
        return Err(Error::Memory(format!(
            "invalid instruction size at {address:#x}"
        )));
    }
    let raw = machine.mem_read_as_vec(address, size as usize)?;
    let instruction = Decoder::with_ip(64, &raw, address, DecoderOptions::NONE).decode();
    if instruction.is_invalid() {
        return Err(Error::Memory(format!(
            "cannot decode instruction at {address:#x}"
        )));
    }
    let mnemonic = instruction.mnemonic();
    let data = machine.get_data_mut();
    data.steps += 1;
    data.entered.push(EnteredInstruction {
        pc: address,
        bytes: raw,
    });
    if unsupported(mnemonic) {
        let data = machine.get_data_mut();
        data.reason = Some(StopReason::Unsupported);
        data.detail = Some(format!(
            "unsupported instruction {} at {address:#x}",
            format!("{mnemonic:?}").to_ascii_lowercase()
        ));
        machine.emu_stop()?;
    } else if mnemonic == Mnemonic::Syscall {
        let mut args = [0; 6];
        for (i, reg) in [Reg::RDI, Reg::RSI, Reg::RDX, Reg::R10, Reg::R8, Reg::R9]
            .iter()
            .enumerate()
        {
            args[i] = machine.reg_read(*reg)?;
        }
        let request = SyscallRequest {
            number: machine.reg_read(Reg::RAX)?,
            args,
            pc: address,
            size: u64::from(size),
        };
        let data = machine.get_data_mut();
        data.reason = Some(StopReason::Syscall);
        data.request = Some(request);
        machine.emu_stop()?;
    }
    Ok(())
}

impl Process {
    pub fn new(
        name: &str,
        image: &[u8],
        entropy: &mut RandomStream,
        argv: &[String],
        pid: u64,
    ) -> Result<Self> {
        let machine = Unicorn::new_with_data(Arch::X86, Mode::MODE_64, HookState::default())?;
        let mut process = Self {
            name: name.into(),
            pid,
            threads: BTreeMap::new(),
            machine,
            brk_start: 0,
            brk_end: 0,
            mmap_next: 0x1000000000,
        };
        process.load(image, entropy, argv)?;
        process
            .machine
            .add_code_hook(1, 0, |machine, address, size| {
                if let Err(error) = hook(machine, address, size) {
                    let data = machine.get_data_mut();
                    data.reason = Some(StopReason::Fault);
                    data.detail = Some(error.to_string());
                    // The callback must not unwind across the native emulator boundary.
                    let _ = machine.emu_stop();
                }
            })?;
        let context = process.machine.context_init()?;
        process.threads.insert(
            pid,
            Thread {
                tid: pid,
                context,
                state: State::Runnable,
                wait_id: None,
                clear_tid: 0,
                exit_code: None,
                signal_mask: 0,
            },
        );
        Ok(process)
    }
    fn load(&mut self, image: &[u8], entropy: &mut RandomStream, argv: &[String]) -> Result<()> {
        let invalid = |s: &str| Error::InvalidInput(s.into());
        if image.len() > 32 * 1024 * 1024 {
            return Err(invalid("ELF image exceeds 32 MiB"));
        }
        let elf = Elf::parse(image).map_err(|e| invalid(&format!("invalid ELF: {e}")))?;
        if !elf.is_64
            || !elf.little_endian
            || elf.header.e_machine != EM_X86_64
            || elf.header.e_type != ET_EXEC
        {
            return Err(invalid(
                "only static, fixed-address little-endian x86-64 ELF64 executables are supported",
            ));
        }
        if elf
            .program_headers
            .iter()
            .any(|s| s.p_type == PT_INTERP || s.p_type == PT_DYNAMIC)
        {
            return Err(invalid("dynamic linking and PIE are not supported yet"));
        }
        let loads: Vec<_> = elf
            .program_headers
            .iter()
            .filter(|s| s.p_type == PT_LOAD && s.p_memsz > 0)
            .collect();
        let mut pages = BTreeMap::<u64, Prot>::new();
        let mut phdr = 0;
        for segment in &loads {
            let addr = segment.p_vaddr;
            let size = segment.p_memsz;
            if segment.p_filesz > size || size > 64 * 1024 * 1024 {
                return Err(invalid("invalid or oversized ELF segment"));
            }
            let end = addr
                .checked_add(size)
                .ok_or_else(|| invalid("ELF address overflow"))?;
            if addr < PAGE || end >= STACK_BASE {
                return Err(invalid("ELF segment outside supported address range"));
            }
            let file_end = segment
                .p_offset
                .checked_add(segment.p_filesz)
                .ok_or_else(|| invalid("ELF offset overflow"))?;
            if file_end > image.len() as u64 {
                return Err(invalid("ELF segment exceeds image bounds"));
            }
            let mut permissions = Prot::NONE;
            if segment.p_flags & 4 != 0 {
                permissions |= Prot::READ;
            }
            if segment.p_flags & 2 != 0 {
                permissions |= Prot::WRITE;
            }
            if segment.p_flags & 1 != 0 {
                permissions |= Prot::EXEC;
            }
            for page in ((addr / PAGE * PAGE)..end.div_ceil(PAGE) * PAGE).step_by(PAGE as usize) {
                *pages.entry(page).or_insert(Prot::NONE) |= permissions;
            }
            if segment.p_offset <= elf.header.e_phoff && elf.header.e_phoff < file_end {
                phdr = addr + elf.header.e_phoff - segment.p_offset;
            }
        }
        if pages.len() as u64 * PAGE > 128 * 1024 * 1024 {
            return Err(invalid("ELF mapped memory exceeds 128 MiB"));
        }
        for page in pages.keys() {
            self.machine.mem_map(*page, PAGE, Prot::ALL)?;
        }
        for segment in &loads {
            self.machine.mem_write(
                segment.p_vaddr,
                &image[segment.p_offset as usize..(segment.p_offset + segment.p_filesz) as usize],
            )?;
        }
        for (page, flags) in &pages {
            self.machine.mem_protect(*page, PAGE, *flags)?;
        }
        self.brk_start = pages
            .last_key_value()
            .ok_or_else(|| invalid("ELF has no load segments"))?
            .0
            + PAGE;
        self.brk_end = self.brk_start;
        if !pages
            .get(&(elf.entry / PAGE * PAGE))
            .is_some_and(|p| p.0 & Prot::EXEC.0 != 0)
        {
            return Err(invalid("ELF entry point is not executable"));
        }
        self.machine
            .mem_map(STACK_BASE, STACK_SIZE, Prot::READ | Prot::WRITE)?;
        self.machine.reg_write(Reg::RIP, elf.entry)?;
        self.machine.reg_write(Reg::EFLAGS, 2)?;
        if argv.is_empty()
            || argv.iter().map(|a| a.len() + 1).sum::<usize>() + 4096 > STACK_SIZE as usize
        {
            return Err(invalid("guest arguments exceed stack"));
        }
        let mut sp = STACK_BASE + STACK_SIZE;
        let mut pointers = Vec::new();
        for arg in argv.iter().rev() {
            let mut data = arg.as_bytes().to_vec();
            data.push(0);
            sp -= data.len() as u64;
            self.machine.mem_write(sp, &data)?;
            pointers.insert(0, sp);
        }
        sp -= 16;
        self.machine.mem_write(sp, &entropy.read(16))?;
        let auxv = [
            (3, phdr),
            (4, u64::from(elf.header.e_phentsize)),
            (5, u64::from(elf.header.e_phnum)),
            (6, PAGE),
            (9, elf.entry),
            (11, 1000),
            (12, 1000),
            (13, 1000),
            (14, 1000),
            (23, 0),
            (25, sp),
            (31, pointers[0]),
            (0, 0),
        ];
        let mut words = vec![pointers.len() as u64];
        words.extend(pointers);
        words.extend([0, 0]);
        for (key, value) in auxv {
            words.extend([key, value]);
        }
        sp = (sp - words.len() as u64 * 8) & !15;
        let bytes: Vec<_> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        self.machine.mem_write(sp, &bytes)?;
        self.machine.reg_write(Reg::RSP, sp)?;
        Ok(())
    }
    pub fn run(&mut self, tid: u64, budget: u64) -> Result<ExecutionStop> {
        let thread = self
            .threads
            .get_mut(&tid)
            .ok_or_else(|| Error::Invariant("missing thread".into()))?;
        if thread.state != State::Runnable || budget == 0 {
            return Err(Error::Invariant(
                "backend requires a runnable thread and positive budget".into(),
            ));
        }
        self.machine.context_restore(&thread.context)?;
        *self.machine.get_data_mut() = HookState {
            budget,
            entered: Vec::with_capacity(budget as usize),
            ..HookState::default()
        };
        let pc = self.machine.reg_read(Reg::RIP)?;
        let outcome = self.machine.emu_start(pc, 0, 0, budget as usize);
        if let Err(error) = outcome {
            if self.machine.get_data().reason.is_none() {
                let data = self.machine.get_data_mut();
                data.reason = Some(StopReason::Fault);
                data.detail = Some(format!("{error:?}"));
            }
        }
        self.machine.context_save(&mut thread.context)?;
        let data = std::mem::take(self.machine.get_data_mut());
        Ok(ExecutionStop {
            reason: data.reason.unwrap_or(StopReason::Quantum),
            steps: data.steps,
            request: data.request,
            detail: data.detail,
            entered: data.entered,
        })
    }
    pub fn complete(&mut self, tid: u64, request: &SyscallRequest, value: i64) -> Result<()> {
        let thread = self
            .threads
            .get_mut(&tid)
            .ok_or_else(|| Error::Invariant("missing thread".into()))?;
        self.machine.context_restore(&thread.context)?;
        self.machine.reg_write(Reg::RAX, value as u64)?;
        self.machine
            .reg_write(Reg::RCX, request.pc + request.size)?;
        let flags = self.machine.reg_read(Reg::EFLAGS)?;
        self.machine.reg_write(Reg::R11, flags)?;
        self.machine
            .reg_write(Reg::RIP, request.pc + request.size)?;
        self.machine.context_save(&mut thread.context)?;
        thread.state = State::Runnable;
        thread.wait_id = None;
        Ok(())
    }
    pub fn clone_thread(
        &mut self,
        parent: u64,
        tid: u64,
        stack: u64,
        tls: Option<u64>,
    ) -> Result<()> {
        let signal_mask = self.threads[&parent].signal_mask;
        self.machine
            .context_restore(&self.threads[&parent].context)?;
        self.machine.reg_write(Reg::RAX, 0)?;
        self.machine.reg_write(Reg::RSP, stack)?;
        if let Some(tls) = tls {
            self.machine.reg_write(Reg::FS_BASE, tls)?;
        }
        let context = self.machine.context_init()?;
        self.machine
            .context_restore(&self.threads[&parent].context)?;
        self.threads.insert(
            tid,
            Thread {
                tid,
                context,
                state: State::Runnable,
                wait_id: None,
                clear_tid: 0,
                exit_code: None,
                signal_mask,
            },
        );
        Ok(())
    }
    pub fn register(&self, reg: Reg) -> Result<u64> {
        Ok(self.machine.reg_read(reg)?)
    }
    pub fn set_tls(&mut self, tid: u64, base: u64) -> Result<()> {
        let thread = self
            .threads
            .get_mut(&tid)
            .ok_or_else(|| Error::Invariant("missing thread".into()))?;
        self.machine.context_restore(&thread.context)?;
        self.machine.reg_write(Reg::FS_BASE, base)?;
        self.machine.context_save(&mut thread.context)?;
        Ok(())
    }
    pub fn brk(&mut self, requested: u64) -> Result<u64> {
        if requested == 0
            || requested < self.brk_start
            || requested - self.brk_start > 128 * 1024 * 1024
        {
            return Ok(self.brk_end);
        }
        let old = self.brk_end.div_ceil(PAGE) * PAGE;
        let new = requested.div_ceil(PAGE) * PAGE;
        if new > old {
            let regions = self.machine.mem_regions()?;
            if regions.iter().any(|r| old <= r.end && r.begin < new)
                || regions.iter().map(|r| r.end + 1 - r.begin).sum::<u64>() + new - old
                    > 256 * 1024 * 1024
            {
                return Ok(self.brk_end);
            }
            self.machine
                .mem_map(old, new - old, Prot::READ | Prot::WRITE)?;
        } else if old > new {
            self.machine.mem_unmap(new, old - new)?;
        }
        self.brk_end = requested;
        Ok(requested)
    }
    pub fn mmap(&mut self, hint: u64, length: u64, protection: u64, flags: u64) -> Result<i64> {
        use crate::linux_errno as errno;
        if length == 0
            || length > 128 * 1024 * 1024
            || protection & !7 != 0
            || flags & !(0x02 | 0x10 | 0x20 | 0x100 | 0x20000) != 0
            || flags & 0x22 != 0x22
        {
            return Ok(-errno::EINVAL);
        }
        // MAP_FIXED is accepted only for an entirely unmapped range. Replacing
        // existing mappings is deliberately outside this initial ABI profile.
        let fixed = flags & 0x10 != 0;
        let start = if fixed { hint } else { self.mmap_next };
        let size = length.div_ceil(PAGE) * PAGE;
        let Some(end) = start.checked_add(size).filter(|end| *end < STACK_BASE) else {
            return Ok(-errno::ENOMEM);
        };
        if start < PAGE || start % PAGE != 0 {
            return Ok(-errno::EINVAL);
        }
        let regions = self.machine.mem_regions()?;
        if regions.iter().any(|r| start <= r.end && r.begin < end)
            || regions.iter().map(|r| r.end + 1 - r.begin).sum::<u64>() + size > 256 * 1024 * 1024
        {
            return Ok(-errno::ENOMEM);
        }
        self.machine.mem_map(start, size, Prot(protection as u32))?;
        if !fixed {
            self.mmap_next = end + PAGE;
        }
        Ok(start as i64)
    }
    pub fn munmap(&mut self, start: u64, length: u64) -> Result<i64> {
        use crate::linux_errno as errno;
        if start % PAGE != 0 || length == 0 || length > 128 * 1024 * 1024 {
            return Ok(-errno::EINVAL);
        }
        let Some(end) = start.checked_add(length.div_ceil(PAGE) * PAGE) else {
            return Ok(-errno::EINVAL);
        };
        // Linux munmap tolerates holes. Unmap only overlapping mapped pages.
        for r in self.machine.mem_regions()? {
            let begin = start.max(r.begin);
            let stop = end.min(r.end + 1);
            if stop > begin {
                self.machine.mem_unmap(begin, stop - begin)?;
            }
        }
        Ok(0)
    }
    pub fn mprotect(&mut self, start: u64, length: u64, protection: u64) -> Result<i64> {
        use crate::linux_errno as errno;
        if start % PAGE != 0 || protection & !7 != 0 || length > 128 * 1024 * 1024 {
            return Ok(-errno::EINVAL);
        }
        if length == 0 {
            return Ok(0);
        }
        let size = length.div_ceil(PAGE) * PAGE;
        // Check mapping continuity independently of its current permissions.
        let Some(end) = start.checked_add(size) else {
            return Ok(-errno::ENOMEM);
        };
        let mut cursor = start;
        let mut regions = self.machine.mem_regions()?;
        regions.sort_by_key(|r| r.begin);
        for r in regions {
            if r.begin <= cursor && cursor <= r.end {
                cursor = end.min(r.end + 1);
            }
        }
        if cursor != end {
            return Ok(-errno::ENOMEM);
        }
        self.machine
            .mem_protect(start, size, Prot(protection as u32))?;
        Ok(0)
    }
    pub fn check_memory(&self, address: u64, length: u64, permission: Prot) -> Result<()> {
        if length > MAX_IO {
            return Err(Error::Memory("invalid guest memory range".into()));
        }
        let end = address
            .checked_add(length)
            .ok_or_else(|| Error::Memory("invalid guest memory range".into()))?;
        if length == 0 {
            return Ok(());
        }
        let mut cursor = address;
        let mut regions = self.machine.mem_regions()?;
        regions.sort_by_key(|r| r.begin);
        for region in regions {
            if region.begin <= cursor && cursor <= region.end {
                if region.perms & permission.0 == 0 {
                    return Err(Error::Memory("guest memory permission denied".into()));
                }
                cursor = end.min(region.end + 1);
                if cursor == end {
                    return Ok(());
                }
            }
        }
        Err(Error::Memory("unmapped guest memory".into()))
    }
    pub fn read(&self, address: u64, length: u64) -> Result<Vec<u8>> {
        self.check_memory(address, length, Prot::READ)?;
        if length == 0 {
            return Ok(vec![]);
        }
        self.machine
            .mem_read_as_vec(address, length as usize)
            .map_err(|e| Error::Memory(format!("{e:?}")))
    }
    pub fn write(&mut self, address: u64, data: &[u8]) -> Result<()> {
        self.check_memory(address, data.len() as u64, Prot::WRITE)?;
        if !data.is_empty() {
            self.machine
                .mem_write(address, data)
                .map_err(|e| Error::Memory(format!("{e:?}")))?;
        }
        Ok(())
    }
    pub fn cstring(&self, address: u64) -> Result<String> {
        let mut bytes = Vec::new();
        for offset in 0..4096 {
            let cursor = address
                .checked_add(offset)
                .ok_or_else(|| Error::Memory("pathname address overflow".into()))?;
            let byte = self.read(cursor, 1)?[0];
            if byte == 0 {
                return String::from_utf8(bytes)
                    .map_err(|_| Error::Memory("invalid pathname encoding".into()));
            }
            bytes.push(byte);
        }
        Err(Error::Memory("unterminated pathname".into()))
    }
}
