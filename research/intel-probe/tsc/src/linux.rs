use crate::{GUEST, ORG, fixture_table, hash, timestamp, word};
use kvm_bindings::{KVM_MAX_CPUID_ENTRIES, kvm_regs, kvm_segment, kvm_userspace_memory_region};
use kvm_ioctls::{Cap, Kvm, VcpuExit, VcpuFd, VmFd};
use serde_json::{Value, json};
use std::{
    fmt::Display,
    marker::PhantomData,
    rc::Rc,
    thread::{self, ThreadId},
};

type Result<T> = std::result::Result<T, String>;
const SIZE: usize = 0x40000;
const PORT: u16 = 0x505;
fn checked<T, E: Display>(r: std::result::Result<T, E>, context: &str) -> Result<T> {
    r.map_err(|e| format!("{context}: {e}"))
}
fn require(c: bool, message: impl Into<String>) -> Result<()> {
    if c { Ok(()) } else { Err(message.into()) }
}
#[derive(Clone)]
struct Owner {
    thread: ThreadId,
    tid: libc::pid_t,
    _not_send: PhantomData<Rc<()>>,
}
impl Owner {
    fn new() -> Self {
        Self {
            thread: thread::current().id(),
            tid: unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t },
            _not_send: PhantomData,
        }
    }
    fn check(&self) {
        assert_eq!(self.thread, thread::current().id());
        assert_eq!(self.tid, unsafe {
            libc::syscall(libc::SYS_gettid) as libc::pid_t
        });
    }
}
struct Memory {
    pointer: *mut u8,
    owner: Owner,
}
impl Memory {
    fn new(owner: &Owner) -> Result<Self> {
        owner.check();
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        require(
            p != libc::MAP_FAILED,
            format!("mmap: {}", std::io::Error::last_os_error()),
        )?;
        Ok(Self {
            pointer: p.cast(),
            owner: owner.clone(),
        })
    }
    fn bytes(&self) -> &[u8] {
        self.owner.check();
        unsafe { std::slice::from_raw_parts(self.pointer, SIZE) }
    }
    fn write(&mut self, at: usize, bytes: &[u8]) {
        self.owner.check();
        assert!(at + bytes.len() <= SIZE);
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.pointer.add(at), bytes.len()) };
    }
    fn put(&mut self, at: usize, v: u64) {
        self.write(at, &v.to_le_bytes());
    }
    fn get(&self, at: usize) -> u64 {
        word(self.bytes(), at)
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        self.owner.check();
        assert_eq!(unsafe { libc::munmap(self.pointer.cast(), SIZE) }, 0);
    }
}
#[derive(Clone, Copy)]
enum Variant {
    Positive,
    TsdOff,
    Unrelated,
}
struct Machine {
    vcpu: VcpuFd,
    _vm: VmFd,
    memory: Memory,
    owner: Owner,
}
fn segment(selector: u16, code: bool, user: bool) -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: u32::MAX,
        selector,
        type_: if code { 11 } else { 3 },
        present: 1,
        dpl: if user { 3 } else { 0 },
        db: if code { 0 } else { 1 },
        s: 1,
        l: u8::from(code),
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}
fn gate(handler: u64, dpl: u8) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[..2].copy_from_slice(&(handler as u16).to_le_bytes());
    bytes[2..4].copy_from_slice(&8u16.to_le_bytes());
    bytes[5] = 0x8e | (dpl << 5);
    bytes[6..8].copy_from_slice(&((handler >> 16) as u16).to_le_bytes());
    bytes[8..12].copy_from_slice(&((handler >> 32) as u32).to_le_bytes());
    bytes
}
impl Machine {
    fn new(kvm: &Kvm, owner: &Owner, variant: Variant) -> Result<Self> {
        owner.check();
        let mut memory = Memory::new(owner)?;
        memory.write(ORG as usize, GUEST);
        memory.put(0x1000, 0x2007);
        memory.put(0x2000, 0x3007);
        memory.put(0x3000, 0x87);
        // Flat code/data plus 64-bit TSS. All pages user-mapped: trusted bare
        // fixture, no kernel/user memory-protection or security claim.
        for (i, v) in [
            0,
            0x00af9b000000ffff,
            0x00cf93000000ffff,
            0x00affb000000ffff,
            0x00cff3000000ffff,
            0x0000890060000067,
            0,
        ]
        .into_iter()
        .enumerate()
        {
            memory.put(0x5000 + i * 8, v);
        }
        memory.put(0x6004, 0x1f000); // unaligned TSS.RSP0
        memory.write(0x6066, &104u16.to_le_bytes()); // no IO bitmap within limit
        memory.write(0x7000 + 13 * 16, &gate(0x9000, 0));
        memory.write(0x7000 + 128 * 16, &gate(0xa000, 3));
        if matches!(variant, Variant::Unrelated) {
            memory.put(0x40f0, 1);
        }
        let vm = checked(kvm.create_vm(), "create VM")?;
        unsafe {
            checked(
                vm.set_user_memory_region(kvm_userspace_memory_region {
                    slot: 0,
                    flags: 0,
                    guest_phys_addr: 0,
                    memory_size: SIZE as u64,
                    userspace_addr: memory.pointer as u64,
                }),
                "register memory",
            )?
        };
        let vcpu = checked(vm.create_vcpu(0), "create vCPU")?;
        let cpuid = checked(
            kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES),
            "supported CPUID",
        )?;
        require(
            cpuid
                .as_slice()
                .iter()
                .any(|e| e.function == 0x80000001 && e.edx & (1 << 27) != 0),
            "RDTSCP not supported by host/KVM; probe is inconclusive",
        )?;
        checked(vcpu.set_cpuid2(&cpuid), "set CPUID")?;
        let mut s = checked(vcpu.get_sregs(), "get sregs")?;
        s.cs = segment(8, true, false);
        s.ds = segment(0x10, false, false);
        s.es = s.ds;
        s.ss = s.ds;
        s.fs = s.ds;
        s.gs = s.ds;
        s.gdt.base = 0x5000;
        s.gdt.limit = 55;
        s.idt.base = 0x7000;
        s.idt.limit = 4095;
        s.tr = kvm_segment {
            base: 0x6000,
            limit: 103,
            selector: 0x28,
            type_: 9,
            present: 1,
            dpl: 0,
            db: 0,
            s: 0,
            l: 0,
            g: 0,
            avl: 0,
            unusable: 0,
            padding: 0,
        };
        s.cr0 = 0x80010033; // PG,WP,NE,ET,MP,PE
        s.cr3 = 0x1000;
        s.cr4 = (1 << 5)
            | if matches!(variant, Variant::TsdOff) {
                0
            } else {
                1 << 2
            };
        s.efer = (1 << 8) | (1 << 10); // LME,LMA
        checked(vcpu.set_sregs(&s), "set long-mode sregs")?;
        checked(
            vcpu.set_regs(&kvm_regs {
                rip: ORG + 0x80,
                rsp: 0x1f000,
                rflags: 2,
                ..Default::default()
            }),
            "set regs",
        )?;
        Ok(Self {
            vcpu,
            _vm: vm,
            memory,
            owner: owner.clone(),
        })
    }
    fn exit(&mut self) -> Result<u32> {
        self.owner.check();
        self.vcpu.set_kvm_immediate_exit(0);
        loop {
            match self.vcpu.run() {
                Ok(VcpuExit::IoOut(port, data)) => {
                    require(
                        port == PORT && data.len() == 4,
                        format!("unexpected IO port={port:#x} bytes={}", data.len()),
                    )?;
                    let tag = u32::from_le_bytes(data.try_into().unwrap());
                    self.vcpu.set_kvm_immediate_exit(1);
                    let complete = self.vcpu.run();
                    require(
                        matches!(&complete,Err(e) if e.errno()==libc::EINTR),
                        format!("completion-only reentry: {complete:?}"),
                    )?;
                    self.vcpu.set_kvm_immediate_exit(0);
                    let regs = checked(self.vcpu.get_regs(), "completed regs")?;
                    let table = fixture_table();
                    let expected = match tag {
                        1..=3 => table[4],
                        0xba0d => table[5],
                        0xd00d => table[6],
                        _ => return Err(format!("unknown tag {tag:#x}")),
                    };
                    require(
                        regs.rip == expected,
                        format!("PIO completion RIP {:#x}!={expected:#x}", regs.rip),
                    )?;
                    return Ok(tag);
                }
                Err(e) if e.errno() == libc::EINTR => continue,
                other => return Err(format!("unexpected KVM exit {other:?}")),
            }
        }
    }
    fn snapshot(&self) -> Result<Value> {
        self.owner.check();
        let r = checked(self.vcpu.get_regs(), "snapshot regs")?;
        let mut bytes = Vec::new();
        for value in [
            r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10, r.r11,
            r.r12, r.r13, r.r14, r.r15, r.rip, r.rflags,
        ] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Ok(
            json!({"registers_sha256":hash(&bytes),"memory_sha256":hash(self.memory.bytes()),"rip":r.rip}),
        )
    }
}
impl Drop for Machine {
    fn drop(&mut self) {
        self.owner.check();
    }
}
fn execute(kvm: &Kvm, owner: &Owner, seed: u64, variant: Variant) -> Result<Value> {
    let mut m = Machine::new(kvm, owner, variant)?;
    let table = fixture_table();
    let mut events = Vec::new();
    let mut boundaries = Vec::new();
    let mut responses: Vec<(u64, u32)> = Vec::new();
    for _ in 0..8 {
        let tag = m.exit()?;
        if !responses.is_empty() {
            let previous = responses.len() - 1;
            let length = if previous == 0 { 2 } else { 3 };
            require(
                m.memory.get(0x4038) == table[previous] + length,
                "observed emulated instruction next RIP mismatch",
            )?;
        }
        if tag == 0xba0d {
            require(
                m.memory.get(0x4008) == table[3]
                    && m.memory.get(0x4018) == 0
                    && m.memory.get(0x4010) & 3 == 3,
                "unexpected #GP rejection context",
            )?;
            return Err("unrelated_gp_cli_rejected error=0 cpl=3".into());
        }
        if tag == 0xd00d {
            let count = m.memory.get(0x4080);
            require(
                responses.len() == 3 && count == 3,
                format!(
                    "missing_tsc_traps count={count} mediated={}",
                    responses.len()
                ),
            )?;
            let mut outputs = Vec::new();
            let unchanged = [
                (1, 0x1111222233334444),
                (4, 0x2222333344445555),
                (5, 0x3333444455556666),
                (6, 0x4444555566667777),
                (7, 0x5555666677778888),
                (8, 0x6666777788889999),
                (9, 0x777788889999aaaa),
                (12, 0x88889999aaaabbbb),
                (13, 0x9999aaaabbbbcccc),
                (14, 0xaaaabbbbccccdddd),
                (15, 0x30000),
            ];
            for (i, (tsc, aux)) in responses.iter().copied().enumerate() {
                let output: Vec<u64> = (0..16)
                    .map(|j| m.memory.get(0x4400 + i * 128 + j * 8))
                    .collect();
                require(
                    output[0] == tsc as u32 as u64 && output[3] == (tsc >> 32) as u32 as u64,
                    format!("TSC result width/value case{i}: {output:?}"),
                )?;
                let rcx = if i == 1 {
                    aux as u64
                } else {
                    0xcafebabe76543210
                };
                require(output[2] == rcx, format!("RCX/AUX mismatch case{i}"))?;
                require(
                    output[10] == 0x8d7 && output[11] == 0x8d7,
                    format!(
                        "flags changed case{i} before={:#x} after={:#x}",
                        output[10], output[11]
                    ),
                )?;
                for (register, expected) in unchanged {
                    require(
                        output[register] == expected,
                        format!("GPR preservation case{i} reg{register}"),
                    )?;
                }
                outputs.push(output);
            }
            boundaries.push(m.snapshot()?);
            events.push(json!({"kind":"done","trap_count":count}));
            return Ok(
                json!({"seed":seed,"events":events,"outputs":outputs,"boundaries":boundaries}),
            );
        }
        let index = responses.len();
        require(
            index < 3 && tag as usize == index + 1,
            "unexpected TSC request order",
        )?;
        let len = if index == 0 { 2 } else { 3 };
        require(
            m.memory.get(0x4000) == tag as u64
                && m.memory.get(0x4008) == table[index]
                && m.memory.get(0x4020) == len
                && m.memory.get(0x4018) == 0
                && m.memory.get(0x4010) & 3 == 3,
            "invalid decoded #GP request",
        )?;
        let tsc = timestamp(seed, index as u64);
        let aux = 0xc0ff00u32 | index as u32;
        m.memory.put(0x4028, tsc);
        m.memory.put(0x4030, aux as u64);
        responses.push((tsc, aux));
        events.push(json!({"kind":"tsc_emulated","opcode_case":tag,"fault_pc":table[index],"next_pc":table[index]+len,"timestamp":tsc,"aux":aux}));
        boundaries.push(m.snapshot()?);
    }
    Err("finite PIO event budget exceeded".into())
}
pub fn run() -> Result<()> {
    let owner = Owner::new();
    let mut seed = 0x51eeb5eed;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--once" => {}
            "--seed" => {
                let s = args.next().ok_or("seed missing")?;
                seed = if let Some(s) = s.strip_prefix("0x") {
                    checked(u64::from_str_radix(s, 16), "seed")?
                } else {
                    checked(s.parse(), "seed")?
                };
            }
            _ => return Err(format!("unknown argument {arg}")),
        }
    }
    let kvm = checked(Kvm::new(), "open KVM")?;
    require(kvm.get_api_version() == 12, "KVM API12 required")?;
    require(
        kvm.check_extension(Cap::ImmediateExit),
        "immediate_exit required",
    )?;
    eprintln!(
        "metadata host_tid={} kvm_api=12 profile=bare_long_mode_cpl3_tsd",
        owner.tid
    );
    let positive = execute(&kvm, &owner, seed, Variant::Positive)?;
    match execute(&kvm, &owner, seed, Variant::TsdOff) {
        Err(e) if e == "missing_tsc_traps count=0 mediated=0" => {
            eprintln!("NEGATIVE_CONTROL_REJECTED TSD_off: {e}")
        }
        other => {
            return Err(format!(
                "TSD-off negative did not specifically miss all traps: {other:?}"
            ));
        }
    }
    match execute(&kvm, &owner, seed, Variant::Unrelated) {
        Err(e) if e == "unrelated_gp_cli_rejected error=0 cpl=3" => {
            eprintln!("NEGATIVE_CONTROL_REJECTED unrelated_CLI_GP: {e}")
        }
        other => return Err(format!("unrelated-GP negative failed: {other:?}")),
    }
    println!(
        "{}",
        json!({"profile":"bare_x86_64_ring3_exact_tsc_gp","world":positive,"negative_tsd_off_missed_traps":true,"negative_unrelated_gp_rejected":true,"guest_sha256":hash(GUEST),
        "not_tested":["linux_task_policy_pr_set_tsc","kernel_tsc_reads","vdso_time","irq_timer_delivery","full_instruction_decoder","cross_page_or_unmapped_fetch","host_portability","complete_cpu_snapshot"]})
    );
    Ok(())
}
