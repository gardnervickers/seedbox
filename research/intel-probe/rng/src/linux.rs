use crate::{GUEST, ORG, fixture_table, hash, random, rewrite, word};
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
const PORT: u16 = 0x506;
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
    Exposed,
    HiddenRaw,
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
    fn new(
        kvm: &Kvm,
        owner: &Owner,
        variant: Variant,
        portable_rewrite: bool,
        synthetic_ud: bool,
    ) -> Result<Self> {
        owner.check();
        let mut memory = Memory::new(owner)?;
        let mut code = GUEST.to_vec();
        if portable_rewrite && matches!(variant, Variant::Positive | Variant::Unrelated) {
            rewrite(&mut code)?;
            memory.put(0x40f1, 1);
        }
        if synthetic_ud && matches!(variant, Variant::Positive) {
            // A DPL3 software INT 6 saves RIP at the original RNG instruction.
            // This validates handler/PIO semantics without claiming hardware #UD.
            for pc in &fixture_table()[..3] {
                let at = (*pc - ORG) as usize;
                require(
                    code[at - 2..at] == [0x90, 0x90],
                    "synthetic trap preamble mismatch",
                )?;
                code[at - 2..at].copy_from_slice(&[0xcd, 0x06]);
            }
        }
        memory.write(ORG as usize, &code);
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
        memory.write(
            0x7000 + 6 * 16,
            &gate(0x9000, if synthetic_ud { 3 } else { 0 }),
        );
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
        let mut cpuid = checked(
            kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES),
            "supported CPUID",
        )?;
        let rdrand = cpuid
            .as_slice()
            .iter()
            .any(|e| e.function == 1 && e.ecx & (1 << 30) != 0);
        let rdseed = cpuid
            .as_slice()
            .iter()
            .any(|e| e.function == 7 && e.index == 0 && e.ebx & (1 << 18) != 0);
        require(
            rdrand && rdseed,
            "native RNG features missing; raw bypass control inconclusive",
        )?;
        for e in cpuid.as_mut_slice() {
            if e.function == 1 && !matches!(variant, Variant::Exposed) {
                e.ecx &= !(1 << 30);
            }
            if e.function == 7 && e.index == 0 && !matches!(variant, Variant::Exposed) {
                e.ebx &= !(1 << 18);
            }
        }
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
        s.cr4 = 1 << 5;
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
fn execute(
    kvm: &Kvm,
    owner: &Owner,
    seed: u64,
    variant: Variant,
    portable_rewrite: bool,
    synthetic_ud: bool,
) -> Result<Value> {
    let mut m = Machine::new(kvm, owner, variant, portable_rewrite, synthetic_ud)?;
    let table = fixture_table();
    let mut events = Vec::new();
    let mut boundaries = Vec::new();
    let mut responses: Vec<u64> = Vec::new();
    for _ in 0..8 {
        let tag = m.exit()?;
        let advertised = if matches!(variant, Variant::Exposed) {
            [1 << 30, 1 << 18]
        } else {
            [0, 0]
        };
        require(
            m.memory.get(0x40a0) == advertised[0] && m.memory.get(0x40a8) == advertised[1],
            "guest CPUID RNG advertisement mismatch",
        )?;
        if !responses.is_empty() {
            let previous = responses.len() - 1;
            let length = [4, 3, 4][previous];
            require(
                m.memory.get(0x4038) == table[previous] + length,
                "observed emulated instruction next RIP mismatch",
            )?;
        }
        if tag == 0xba0d {
            require(
                m.memory.get(0x4008) == table[3] && m.memory.get(0x4010) & 3 == 3,
                "unexpected #UD rejection context",
            )?;
            return Err("ordinary_ud2_rejected cpl=3".into());
        }
        if tag == 0xd00d {
            let count = m.memory.get(0x4080);
            if matches!(variant, Variant::Exposed | Variant::HiddenRaw) {
                eprintln!(
                    "RAW_UNMEDIATED_OUTPUTS {:?}",
                    (0..3)
                        .map(|i| m.memory.get(0x4400 + i * 128))
                        .collect::<Vec<_>>()
                );
            }
            require(
                responses.len() == 3 && count == 3,
                format!(
                    "missing_rng_traps count={count} mediated={}",
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
            for (i, value) in responses.iter().copied().enumerate() {
                let output: Vec<u64> = (0..16)
                    .map(|j| m.memory.get(0x4400 + i * 128 + j * 8))
                    .collect();
                let expected = match i {
                    0 => 0xffffffffffff0000 | (value & 0xffff),
                    1 => value as u32 as u64,
                    _ => value,
                };
                require(
                    output[0] == expected,
                    format!("RNG result width/value case{i}: {output:?}"),
                )?;
                require(
                    output[2] == 0xcafebabe76543210 && output[3] == 0xffffffff00000000,
                    "RCX/RDX preservation",
                )?;
                require(
                    output[10] == 0x8d7 && output[11] == 3,
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
            "unexpected RNG request order",
        )?;
        let len = [4, 3, 4][index];
        require(
            m.memory.get(0x4000) == tag as u64
                && m.memory.get(0x4008) == table[index]
                && m.memory.get(0x4020) == len
                && m.memory.get(0x4010) & 3 == 3,
            "invalid manifest #UD request",
        )?;
        let value = random(seed, index as u64);
        m.memory.put(0x4028, value);
        responses.push(value);
        events.push(json!({"kind":"rng_emulated","opcode_case":tag,"fault_pc":table[index],"next_pc":table[index]+len,"value":value}));
        boundaries.push(m.snapshot()?);
    }
    Err("finite PIO event budget exceeded".into())
}
pub fn run() -> Result<()> {
    let owner = Owner::new();
    let mut seed = 0x51eeb5eed;
    let mut portable_rewrite = false;
    let mut synthetic_ud = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--once" => {}
            "--portable-rewrite-smoke" => portable_rewrite = true,
            "--synthetic-ud-smoke" => synthetic_ud = true,
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
    let vendor = unsafe {
        let c = std::arch::x86_64::__cpuid(0);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&c.ebx.to_le_bytes());
        bytes.extend_from_slice(&c.edx.to_le_bytes());
        bytes.extend_from_slice(&c.ecx.to_le_bytes());
        String::from_utf8(bytes).unwrap()
    };
    require(
        !(portable_rewrite && synthetic_ud),
        "fixture modes are mutually exclusive",
    )?;
    require(
        portable_rewrite || synthetic_ud || vendor == "GenuineIntel",
        format!(
            "Intel admission requires GenuineIntel; actual={vendor}; fixture smoke modes alone allow other hosts"
        ),
    )?;
    let kvm = checked(Kvm::new(), "open KVM")?;
    require(kvm.get_api_version() == 12, "KVM API12 required")?;
    require(
        kvm.check_extension(Cap::ImmediateExit),
        "immediate_exit required",
    )?;
    eprintln!(
        "metadata host_tid={} kvm_api=12 vendor={} portable_rewrite={} synthetic_ud={}",
        owner.tid, vendor, portable_rewrite, synthetic_ud
    );
    let positive = execute(
        &kvm,
        &owner,
        seed,
        Variant::Positive,
        portable_rewrite,
        synthetic_ud,
    )?;
    if portable_rewrite || synthetic_ud {
        match execute(&kvm, &owner, seed, Variant::HiddenRaw, false, false) {
            Err(e) if e == "missing_rng_traps count=0 mediated=0" => {
                eprintln!("NEGATIVE_CONTROL_REJECTED hidden_cpuid_raw_rng_bypass: {e}");
            }
            other => {
                return Err(format!(
                    "hidden-CPUID AMD bypass control inconclusive: {other:?}"
                ));
            }
        }
    }
    match execute(
        &kvm,
        &owner,
        seed,
        Variant::Exposed,
        portable_rewrite,
        synthetic_ud,
    ) {
        Err(e) if e == "missing_rng_traps count=0 mediated=0" => {
            eprintln!("NEGATIVE_CONTROL_REJECTED exposed_features_allow_raw_rng: {e}")
        }
        other => {
            return Err(format!(
                "exposed-feature negative did not specifically miss all traps: {other:?}"
            ));
        }
    }
    match execute(
        &kvm,
        &owner,
        seed,
        Variant::Unrelated,
        portable_rewrite,
        synthetic_ud,
    ) {
        Err(e) if e == "ordinary_ud2_rejected cpl=3" => {
            eprintln!("NEGATIVE_CONTROL_REJECTED ordinary_UD2: {e}")
        }
        other => return Err(format!("ordinary-UD2 negative failed: {other:?}")),
    }
    println!(
        "{}",
        json!({"profile":if portable_rewrite { "amd_fixed_site_rewrite_guest_ud_pio" } else if synthetic_ud { "synthetic_INT6_original_opcode_handler_smoke_only" } else { "intel_original_rng_guest_ud_pio" },"host_vendor":vendor,"world":positive,"negative_exposed_bypass_rejected":true,"negative_hidden_cpuid_bypass_rejected":portable_rewrite || synthetic_ud,"negative_ordinary_ud2_rejected":true,"guest_sha256":hash(GUEST),"guest_cpuid_rng_bits":[false,false],"native_kvm_cpuid_rng_bits":[true,true],"mediation_interface":"guest_exception_handler_KVM_EXIT_IO_completed_before_seed_response",
        "not_tested":["linux_rng_interfaces","kernel_rng_sites","dynamic_or_self_modifying_code","irq_timer_delivery","full_instruction_decoder","cross_page_or_unmapped_fetch","host_portability","complete_cpu_snapshot"]})
    );
    Ok(())
}
