//! Breakpoint stepping: advance a stopped vCPU to an exact conditional-branch
//! count without touching RFLAGS.TF (single-step TF leaks into PUSHF, the
//! SYSCALL R11 image and exception frames, and SYSCALL masks it).
//!
//! From the current RIP we decode straight-line code up to the next control
//! decision and arm hardware execution breakpoints at its possible successors,
//! plus the #PF and #GP handler entries. Each exit therefore retires at most
//! one conditional branch, and the landing point is the instruction following
//! the target branch: the same position single-stepping would reach.

use iced_x86::{
    Code, Decoder, DecoderOptions, FlowControl, Instruction, MemorySize, Mnemonic, OpKind, Register,
};
use kvm_bindings::*;
use kvm_ioctls::VcpuFd;

pub struct Planner {
    pf: u64,
    gp: u64,
    ud: u64,
    lstar: u64,
    idt_base: u64,
    bitness: u32,
    paging: Paging,
    fs_base: u64,
    gs_base: u64,
}

/// Guest paging context for software page walks (no KVM_TRANSLATE: it uses
/// supervisor access rights, so SMAP hides user pages).
#[derive(Clone, Copy)]
pub struct Paging {
    pg: bool,
    long: bool,
    cr3: u64,
}

impl Paging {
    pub fn from(s: &kvm_sregs) -> Self {
        Self {
            pg: s.cr0 & (1 << 31) != 0,
            long: s.efer & (1 << 10) != 0,
            cr3: s.cr3,
        }
    }

    fn translate(&self, mem: &[u8], gva: u64) -> Option<u64> {
        if !self.pg {
            return Some(gva & 0xffff_ffff);
        }
        if !self.long {
            return None; // 32-bit paging is never used by this guest profile
        }
        let read = |pa: u64| -> Option<u64> {
            let a = pa as usize;
            mem.get(a..a + 8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        };
        let mask = 0x000f_ffff_ffff_f000u64;
        let mut table = self.cr3 & mask;
        for level in (0..4).rev() {
            let idx = (gva >> (12 + 9 * level)) & 0x1ff;
            let e = read(table + idx * 8)?;
            if e & 1 == 0 {
                return None;
            }
            if level > 0 && level < 3 && e & 0x80 != 0 {
                let size = 1u64 << (12 + 9 * level);
                return Some((e & mask & !(size - 1)) | (gva & (size - 1)));
            }
            table = e & mask;
        }
        Some(table | (gva & 0xfff))
    }
}

fn read_virt(p: &Paging, mem: &[u8], gva: u64, buf: &mut [u8]) -> usize {
    let mut done = 0;
    while done < buf.len() {
        let va = gva.wrapping_add(done as u64);
        let Some(pa) = p.translate(mem, va) else {
            break;
        };
        let pa = pa as usize;
        let n = (4096 - (va as usize & 0xfff)).min(buf.len() - done);
        let Some(src) = mem.get(pa..pa + n) else {
            break;
        };
        buf[done..done + n].copy_from_slice(src);
        done += n;
    }
    done
}

/// Why a branch target could not be computed.
#[derive(Debug)]
enum TargetError {
    /// The target is read from guest memory that is not mapped: the
    /// instruction faults into the #PF handler before it transfers control.
    Unmapped(u64),
    Other(String),
}

impl From<String> for TargetError {
    fn from(e: String) -> Self {
        Self::Other(e)
    }
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unmapped(gva) => write!(f, "unmapped data at {gva:#x}"),
            Self::Other(e) => f.write_str(e),
        }
    }
}

impl From<TargetError> for String {
    fn from(e: TargetError) -> Self {
        e.to_string()
    }
}

fn read_word(p: &Paging, mem: &[u8], gva: u64, bytes: usize) -> Result<u64, TargetError> {
    let mut b = [0u8; 8];
    if read_virt(p, mem, gva, &mut b[..bytes]) != bytes {
        return Err(TargetError::Unmapped(gva));
    }
    Ok(u64::from_le_bytes(b))
}

fn gpr(r: &kvm_regs, reg: Register) -> Result<u64, String> {
    use Register::*;
    let v = match reg.full_register() {
        RAX => r.rax,
        RBX => r.rbx,
        RCX => r.rcx,
        RDX => r.rdx,
        RSI => r.rsi,
        RDI => r.rdi,
        RSP => r.rsp,
        RBP => r.rbp,
        R8 => r.r8,
        R9 => r.r9,
        R10 => r.r10,
        R11 => r.r11,
        R12 => r.r12,
        R13 => r.r13,
        R14 => r.r14,
        R15 => r.r15,
        RIP => r.rip,
        other => return Err(format!("unsupported register {other:?}")),
    };
    Ok(match reg.size() {
        8 => v,
        4 => v as u32 as u64,
        2 => v as u16 as u64,
        _ => return Err(format!("unsupported register size {reg:?}")),
    })
}

impl Planner {
    pub fn load(vcpu: &VcpuFd, mem: &[u8]) -> Result<Self, String> {
        let s = vcpu.get_sregs().map_err(|e| e.to_string())?;
        let mut msrs = Msrs::from_entries(&[kvm_msr_entry {
            index: 0xc000_0082,
            ..Default::default()
        }])
        .map_err(|e| format!("{e:?}"))?;
        vcpu.get_msrs(&mut msrs).map_err(|e| e.to_string())?;
        let bitness = if s.efer & (1 << 10) != 0 && s.cs.l != 0 {
            64
        } else if s.cs.db != 0 {
            32
        } else {
            16
        };
        let mut p = Self {
            pf: 0,
            gp: 0,
            ud: 0,
            lstar: msrs.as_slice()[0].data,
            idt_base: s.idt.base,
            bitness,
            paging: Paging::from(&s),
            fs_base: s.fs.base,
            gs_base: s.gs.base,
        };
        p.pf = p.handler(mem, 14)?;
        p.gp = p.handler(mem, 13)?;
        p.ud = p.handler(mem, 6)?;
        Ok(p)
    }

    /// Refresh the parts of CPU state that can change between steps.
    pub fn refresh_from(&mut self, s: &kvm_sregs) {
        self.paging = Paging::from(&s);
        self.fs_base = s.fs.base;
        self.gs_base = s.gs.base;
        self.bitness = if s.efer & (1 << 10) != 0 && s.cs.l != 0 {
            64
        } else if s.cs.db != 0 {
            32
        } else {
            16
        };
    }

    fn handler(&self, mem: &[u8], vector: u64) -> Result<u64, String> {
        if self.bitness != 64 {
            let mut g = [0u8; 8];
            if read_virt(&self.paging, mem, self.idt_base + 8 * vector, &mut g) != 8 {
                return Err("IDT unmapped".into());
            }
            return Ok(u16::from_le_bytes([g[0], g[1]]) as u64
                | (u16::from_le_bytes([g[6], g[7]]) as u64) << 16);
        }
        let mut g = [0u8; 16];
        if read_virt(&self.paging, mem, self.idt_base + 16 * vector, &mut g) != 16 {
            return Err("IDT unmapped".into());
        }
        Ok(u16::from_le_bytes([g[0], g[1]]) as u64
            | (u16::from_le_bytes([g[6], g[7]]) as u64) << 16
            | (u32::from_le_bytes([g[8], g[9], g[10], g[11]]) as u64) << 32)
    }

    fn decode(&self, mem: &[u8], ip: u64) -> Result<Instruction, String> {
        let mut b = [0u8; 16];
        let n = read_virt(&self.paging, mem, ip, &mut b);
        let mut d = Decoder::with_ip(self.bitness, &b[..n], ip, DecoderOptions::NONE);
        let ins = d.decode();
        if ins.is_invalid() {
            return Err(format!("cannot decode at {ip:#x} ({n} bytes readable)"));
        }
        Ok(ins)
    }

    /// Successor of the control-transfer instruction at the current RIP, and
    /// whether it resumes with RFLAGS.RF set (suppressing a breakpoint there).
    fn successor(
        &self,
        mem: &[u8],
        r: &kvm_regs,
        ins: &Instruction,
    ) -> Result<(u64, bool), TargetError> {
        const RF: u64 = 1 << 16;
        match ins.mnemonic() {
            Mnemonic::Sysret | Mnemonic::Sysretq => return Ok((r.rcx, r.r11 & RF != 0)),
            Mnemonic::Iretq => {
                let rip = read_word(&self.paging, mem, r.rsp, 8)?;
                let flags = read_word(&self.paging, mem, r.rsp + 16, 8)?;
                return Ok((rip, flags & RF != 0));
            }
            Mnemonic::Iretd => {
                let rip = read_word(&self.paging, mem, r.rsp, 4)?;
                let flags = read_word(&self.paging, mem, r.rsp + 8, 4)?;
                return Ok((rip, flags & RF != 0));
            }
            _ => {}
        }
        self.plain_successor(mem, r, ins).map(|t| (t, false))
    }

    fn plain_successor(
        &self,
        mem: &[u8],
        r: &kvm_regs,
        ins: &Instruction,
    ) -> Result<u64, TargetError> {
        match ins.mnemonic() {
            Mnemonic::Syscall => return Ok(self.lstar),
            Mnemonic::Ret => return read_word(&self.paging, mem, r.rsp, 8),
            // Far return pops the offset first. The guest's code segments are
            // flat, so the offset is the linear target.
            Mnemonic::Retf => {
                let bytes = match ins.code() {
                    Code::Retfq | Code::Retfq_imm16 => 8,
                    Code::Retfd | Code::Retfd_imm16 => 4,
                    _ => 2,
                };
                return read_word(&self.paging, mem, r.rsp, bytes);
            }
            Mnemonic::Int3 => return Ok(self.handler(mem, 3)?),
            Mnemonic::Int => return Ok(self.handler(mem, ins.immediate8() as u64)?),
            Mnemonic::Ud0 | Mnemonic::Ud1 | Mnemonic::Ud2 => return Ok(self.ud),
            _ => {}
        }
        match ins.op0_kind() {
            OpKind::FarBranch32 => Ok(ins.far_branch32() as u64),
            OpKind::FarBranch16 => Ok(ins.far_branch16() as u64),
            OpKind::Register => Ok(gpr(r, ins.op0_register())?),
            OpKind::Memory => {
                let ea = if ins.is_ip_rel_memory_operand() {
                    ins.ip_rel_memory_address()
                } else {
                    let mut ea = ins.memory_displacement64();
                    if ins.memory_base() != Register::None {
                        ea = ea.wrapping_add(gpr(r, ins.memory_base())?);
                    }
                    if ins.memory_index() != Register::None {
                        ea = ea.wrapping_add(
                            gpr(r, ins.memory_index())?
                                .wrapping_mul(ins.memory_index_scale() as u64),
                        );
                    }
                    match ins.segment_prefix() {
                        Register::FS => ea.wrapping_add(self.fs_base),
                        Register::GS => ea.wrapping_add(self.gs_base),
                        _ => ea,
                    }
                };
                // A far pointer operand (m16:16/32/64) starts with its offset.
                let bytes = match ins.memory_size() {
                    MemorySize::SegPtr16 => 2,
                    MemorySize::SegPtr32 => 4,
                    MemorySize::SegPtr64 => 8,
                    _ => (self.bitness / 8) as usize,
                };
                read_word(&self.paging, mem, ea, bytes)
            }
            other => Err(TargetError::Other(format!(
                "unsupported control transfer {:?} ({other:?}) at {:#x}",
                ins.mnemonic(),
                ins.ip()
            ))),
        }
    }

    /// Breakpoint addresses for the next advance from the current state.
    #[cfg(test)]
    pub fn plan(&self, mem: &[u8], r: &kvm_regs) -> Result<Vec<u64>, String> {
        self.plan_capped(mem, r).map(|(targets, _)| targets)
    }

    /// As `plan`, and whether the advance stops after a run of straight-line
    /// code at an arbitrary point (the cap) rather than at a branch.
    pub fn plan_capped(&self, mem: &[u8], r: &kvm_regs) -> Result<(Vec<u64>, bool), String> {
        let mut capped = false;
        let mut ip = r.rip;
        let mut targets = Vec::new();
        let mut at_rip = true;
        // The instruction at `ip` will execute with RF set: no breakpoint there.
        let mut unbreakable = false;
        // Register state at that RF-resumed instruction (IRET/SYSRET change
        // only RIP, RSP and flags), so its own successor can be computed.
        let mut resumed: Option<kvm_regs> = None;
        for n in 0..256 {
            let ins = match self.decode(mem, ip) {
                Ok(i) => i,
                // Unmapped or partial code: the fetch will fault into the #PF
                // handler (armed below); otherwise stop just before it.
                Err(_) => {
                    if !at_rip && !unbreakable {
                        targets.push(ip);
                    }
                    break;
                }
            };
            let special = matches!(
                ins.mnemonic(),
                Mnemonic::Syscall
                    | Mnemonic::Sysret
                    | Mnemonic::Sysretq
                    | Mnemonic::Iretq
                    | Mnemonic::Iretd
                    | Mnemonic::Int3
                    | Mnemonic::Int
                    | Mnemonic::Ud0
                    | Mnemonic::Ud1
                    | Mnemonic::Ud2
            );
            let flow = ins.flow_control();
            if special
                || matches!(
                    flow,
                    FlowControl::IndirectBranch
                        | FlowControl::IndirectCall
                        | FlowControl::Return
                        | FlowControl::Interrupt
                )
            {
                if at_rip {
                    let (t, rf) = match self.successor(mem, r, &ins) {
                        Ok(s) => s,
                        // The target is read from a page that is not present
                        // (a return address, jump table or IRET frame not yet
                        // faulted in): the instruction raises #PF before it
                        // transfers control, and that handler is armed below.
                        Err(TargetError::Unmapped(_)) => break,
                        Err(e) => return Err(e.into()),
                    };
                    if rf {
                        let mut r2 = *r;
                        r2.rip = t;
                        if ins.mnemonic() == Mnemonic::Iretq {
                            r2.rsp = read_word(&self.paging, mem, r.rsp + 24, 8)?;
                        }
                        resumed = Some(r2);
                        ip = t;
                        at_rip = false;
                        unbreakable = true;
                        continue;
                    }
                    targets.push(t);
                } else if unbreakable {
                    let r2 = resumed.ok_or("RF-resumed instruction without state")?;
                    let mnemonic = ins.mnemonic();
                    if matches!(
                        mnemonic,
                        Mnemonic::Iretq | Mnemonic::Iretd | Mnemonic::Sysret | Mnemonic::Sysretq
                    ) {
                        return Err(format!(
                            "control transfer {mnemonic:?} at RF-resumed {ip:#x}"
                        ));
                    }
                    match self.plain_successor(mem, &r2, &ins) {
                        Ok(t) => targets.push(t),
                        // As above: it faults into the armed #PF handler.
                        Err(TargetError::Unmapped(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                } else {
                    targets.push(ip); // stop before it; evaluate with real state
                }
                break;
            }
            at_rip = false;
            unbreakable = false;
            match flow {
                FlowControl::Next => ip = ins.next_ip(),
                FlowControl::UnconditionalBranch | FlowControl::Call
                    if matches!(
                        ins.op0_kind(),
                        OpKind::NearBranch64 | OpKind::NearBranch32 | OpKind::NearBranch16
                    ) =>
                {
                    let t = ins.near_branch_target();
                    if t == ip {
                        targets.push(ip);
                        break;
                    }
                    ip = t;
                }
                // A direct far jump or call can change the code segment's
                // mode, so decode no further: stop at its target.
                FlowControl::UnconditionalBranch | FlowControl::Call
                    if matches!(ins.op0_kind(), OpKind::FarBranch32 | OpKind::FarBranch16) =>
                {
                    targets.push(self.plain_successor(mem, r, &ins)?);
                    break;
                }
                FlowControl::ConditionalBranch => {
                    targets.push(ins.next_ip());
                    targets.push(ins.near_branch_target());
                    break;
                }
                _ => {
                    return Err(format!(
                        "unsupported instruction {:?} at {:#x}",
                        ins.mnemonic(),
                        ip
                    ))
                }
            }
            if n == 255 {
                targets.push(ip);
                capped = true;
            }
        }
        for h in [self.pf, self.gp, self.ud] {
            if !targets.contains(&h) {
                targets.push(h);
            }
        }
        targets.dedup();
        targets.truncate(4);
        Ok((targets, capped))
    }

    pub fn arm(vcpu: &VcpuFd, targets: &[u64]) -> Result<(), String> {
        let mut dbg = kvm_guest_debug {
            control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_HW_BP,
            ..Default::default()
        };
        let mut dr7 = 0x400u64;
        for (i, t) in targets.iter().enumerate() {
            dbg.arch.debugreg[i] = *t;
            dr7 |= 1 << (2 * i);
        }
        dbg.arch.debugreg[7] = dr7;
        vcpu.set_guest_debug(&dbg)
            .map_err(|e| format!("set breakpoints: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planner(bitness: u32) -> Planner {
        Planner {
            pf: 0xf000,
            gp: 0xf100,
            ud: 0xf200,
            lstar: 0,
            idt_base: 0,
            bitness,
            paging: Paging {
                pg: false,
                long: false,
                cr3: 0,
            },
            fs_base: 0,
            gs_base: 0,
        }
    }

    #[test]
    fn far_return_targets_the_popped_offset() {
        let mut mem = vec![0u8; 0x4000];
        mem[0x1000..0x1002].copy_from_slice(&[0x48, 0xcb]); // lretq
        mem[0x2000..0x2008].copy_from_slice(&0x3000u64.to_le_bytes());
        mem[0x2008..0x2010].copy_from_slice(&0x10u64.to_le_bytes());
        let r = kvm_regs {
            rip: 0x1000,
            rsp: 0x2000,
            ..Default::default()
        };
        let targets = planner(64).plan(&mem, &r).unwrap();
        assert_eq!(targets[0], 0x3000);
    }

    #[test]
    fn unmapped_branch_operands_fault_into_the_page_fault_handler() {
        let mut mem = vec![0u8; 0x4000];
        mem[0x1000] = 0xc3; // ret, with the stack not present
        mem[0x1100..0x1102].copy_from_slice(&[0xff, 0x20]); // jmp [rax], table not present
        mem[0x1200..0x1202].copy_from_slice(&[0x48, 0xcf]); // iretq, frame not present
        for rip in [0x1000, 0x1100, 0x1200] {
            let r = kvm_regs {
                rip,
                rsp: 0x10_0000,
                rax: 0x20_0000,
                ..Default::default()
            };
            let targets = planner(64).plan(&mem, &r).unwrap();
            assert_eq!(targets, [0xf000, 0xf100, 0xf200], "rip {rip:#x}");
        }
    }

    #[test]
    fn a_long_straight_run_lands_at_the_cap_and_says_so() {
        let mut mem = vec![0u8; 0x4000];
        // a: 300 x nop; jmp a; and a short one: nop; jz +0
        mem[0x1000..0x1000 + 300].fill(0x90);
        let back = 0x1000i64 - (0x1000 + 300 + 5);
        mem[0x1000 + 300] = 0xe9;
        mem[0x1000 + 301..0x1000 + 305].copy_from_slice(&(back as i32).to_le_bytes());
        mem[0x2000..0x2003].copy_from_slice(&[0x90, 0x74, 0x00]);
        let p = planner(32);
        for (rip, capped) in [(0x1000, true), (0x1000 + 100, true), (0x2000, false)] {
            let r = kvm_regs {
                rip,
                ..Default::default()
            };
            assert_eq!(p.plan_capped(&mem, &r).unwrap().1, capped, "rip {rip:#x}");
        }
    }

    #[test]
    fn direct_far_jump_stops_at_its_offset() {
        let mut mem = vec![0u8; 0x4000];
        // nop; ljmp $0x10, $0x2000 (32-bit)
        mem[0x1000..0x1008].copy_from_slice(&[0x90, 0xea, 0x00, 0x20, 0x00, 0x00, 0x10, 0x00]);
        let r = kvm_regs {
            rip: 0x1000,
            ..Default::default()
        };
        let targets = planner(32).plan(&mem, &r).unwrap();
        assert_eq!(targets[0], 0x2000);
    }
}
