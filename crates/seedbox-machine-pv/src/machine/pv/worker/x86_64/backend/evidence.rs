//! Read actual KVM banks without settling I/O, entering the guest or capturing
//! a migration image. Explicit fields omit ABI padding and reserved bytes.
use super::*;
use crate::machine::pv::worker::evidence::{hex, part};
use serde_json::json;
use sha2::{Digest, Sha256};
fn segment(s: &kvm_segment) -> [u64; 12] {
    [
        s.base,
        s.limit as u64,
        s.selector as u64,
        s.type_ as u64,
        s.present as u64,
        s.dpl as u64,
        s.db as u64,
        s.s as u64,
        s.l as u64,
        s.g as u64,
        s.avl as u64,
        s.unusable as u64,
    ]
}
impl Native {
    pub(super) fn read_evidence(&mut self) -> Result<Vec<(String, String)>> {
        if self.held.is_some() {
            return Err("PV evidence before native exit completion".into());
        }
        let r = checked("evidence KVM_GET_REGS", self.cpu.get_regs())?;
        let s = checked("evidence KVM_GET_SREGS", self.cpu.get_sregs())?;
        let x = checked("evidence KVM_GET_XCRS", self.cpu.get_xcrs())?;
        let count = usize::try_from(x.nr_xcrs).map_err(|_| "PV XCR count")?;
        if count > x.xcrs.len() {
            return Err("PV XCR count exceeds ABI".into());
        }
        let mut xcrs: Vec<_> = x.xcrs[..count].iter().map(|r| (r.xcr, r.value)).collect();
        xcrs.sort_by_key(|r| r.0);
        let mut msrs = self.read_msrs()?;
        msrs.sort_by_key(|p| p.0);
        let d = checked("evidence KVM_GET_DEBUGREGS", self.cpu.get_debug_regs())?;
        let e = checked("evidence KVM_GET_VCPU_EVENTS", self.cpu.get_vcpu_events())?;
        let mp = checked("evidence KVM_GET_MP_STATE", self.cpu.get_mp_state())?;
        let cpuid = checked(
            "evidence KVM_GET_CPUID2",
            self.cpu.get_cpuid2(KVM_MAX_CPUID_ENTRIES),
        )?;
        let mut cpuid: Vec<_> = cpuid
            .as_slice()
            .iter()
            .map(|e| [e.function, e.index, e.flags, e.eax, e.ebx, e.ecx, e.edx])
            .collect();
        cpuid.sort_unstable();
        // request_interrupt_window affects the next architectural entry. Do not
        // include immediate_exit/last exit_reason or other host scheduling fields.
        let window = self.cpu.get_kvm_run().request_interrupt_window;
        Ok(vec![
            part(
                "regs",
                &[
                    r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10,
                    r.r11, r.r12, r.r13, r.r14, r.r15, r.rip, r.rflags,
                ],
            )?,
            part(
                "sregs",
                &json!({"segments":[segment(&s.cs),segment(&s.ds),segment(&s.es),segment(&s.fs),segment(&s.gs),segment(&s.ss),segment(&s.tr),segment(&s.ldt)],"gdt":[s.gdt.base,s.gdt.limit as u64],"idt":[s.idt.base,s.idt.limit as u64],"control":[s.cr0,s.cr2,s.cr3,s.cr4,s.cr8,s.efer,s.apic_base],"interrupts":s.interrupt_bitmap}),
            )?,
            ("vectors".into(), self.vector_evidence()?),
            part("xcrs", &xcrs)?,
            part("msrs", &msrs)?,
            part(
                "debugregs",
                &json!({"db":d.db,"dr6":d.dr6,"dr7":d.dr7,"flags":d.flags}),
            )?,
            part(
                "events",
                &json!({"exception":[e.exception.injected as u64,e.exception.nr as u64,e.exception.has_error_code as u64,e.exception.pending as u64,e.exception.error_code as u64,e.exception_has_payload as u64,e.exception_payload],"interrupt":[e.interrupt.injected,e.interrupt.nr,e.interrupt.soft,e.interrupt.shadow],"nmi":[e.nmi.injected,e.nmi.pending,e.nmi.masked],"sipi":e.sipi_vector,"flags":e.flags,"smi":[e.smi.smm,e.smi.pending,e.smi.smm_inside_nmi,e.smi.latched_init],"triple_fault":e.triple_fault.pending}),
            )?,
            part(
                "vcpu",
                &json!({"mp_state":mp.mp_state,"injected":self.injected,"interrupt_window":window}),
            )?,
            part("cpuid", &cpuid)?,
        ])
    }
    pub(super) fn read_xsave(&self) -> Result<Vec<u8>> {
        // XSAVE2 includes dynamically sized banks (e.g. AMX), not the legacy
        // fixed 4KiB snapshot field. Allocation size comes from this actual VM.
        let size = self._vm.check_extension_int(Cap::Xsave2);
        let mut bytes = Vec::new();
        if size > 0 {
            let size = usize::try_from(size).map_err(|_| "PV XSAVE2 size")?;
            let base = std::mem::size_of::<kvm_xsave>();
            if size < base {
                return Err("PV XSAVE2 shorter than base ABI".into());
            }
            let mut area = Xsave::new((size - base).div_ceil(4))
                .map_err(|e| format!("PV XSAVE2 allocation: {e}"))?;
            // SAFETY: VM-reported complete size, allocated before this read-only ioctl.
            checked("evidence KVM_GET_XSAVE2", unsafe {
                self.cpu.get_xsave2(&mut area)
            })?;
            bytes.extend(
                area.as_fam_struct_ref()
                    .xsave
                    .region
                    .iter()
                    .flat_map(|v| v.to_le_bytes()),
            );
            bytes.extend(area.as_slice().iter().flat_map(|v| v.to_le_bytes()));
            bytes.truncate(size);
        } else {
            let area = checked("evidence KVM_GET_XSAVE", self.cpu.get_xsave())?;
            bytes.extend(area.region.iter().flat_map(|v| v.to_le_bytes()));
        }
        Ok(bytes)
    }
    pub(super) fn restore_xsave(&self, bytes: &[u8]) -> Result<()> {
        let base = std::mem::size_of::<kvm_xsave>();
        let reported = self._vm.check_extension_int(Cap::Xsave2);
        if reported <= 0 {
            let area = from_raw::<kvm_xsave>(bytes, "XSAVE")?;
            return checked("KVM_SET_XSAVE", unsafe { self.cpu.set_xsave(&area) });
        }
        let size = usize::try_from(reported).map_err(|_| "PV XSAVE2 size")?;
        if size < base || bytes.len() > size || bytes.len() < base || !bytes.len().is_multiple_of(4)
        {
            return Err("PV XSAVE bytes exceed target architectural area".into());
        }
        let mut area = Xsave::new((size - base).div_ceil(4))
            .map_err(|e| format!("PV XSAVE2 allocation: {e}"))?;
        // SAFETY: modify only the fixed XSAVE bank, never the FAM length/header.
        for (i, v) in unsafe { area.as_mut_fam_struct() }
            .xsave
            .region
            .iter_mut()
            .enumerate()
        {
            *v = u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().expect("four bytes"));
        }
        for (i, v) in area.as_mut_slice().iter_mut().enumerate() {
            let offset = base + i * 4;
            *v = if offset + 4 <= bytes.len() {
                u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
            } else {
                0
            };
        }
        // SAFETY: complete current VM area allocated, checked source bounds above.
        checked("KVM_SET_XSAVE2", unsafe { self.cpu.set_xsave2(&area) })
    }
    fn vector_evidence(&self) -> Result<String> {
        let bytes = self.read_xsave()?;
        if bytes.len() < 576 {
            return Err("PV XSAVE header truncated".into());
        }
        let active = u64::from_le_bytes(bytes[512..520].try_into().expect("eight bytes"));
        let compact = u64::from_le_bytes(bytes[520..528].try_into().expect("eight bytes"));
        if compact != 0 {
            return Err("PV evidence requires standard KVM XSAVE component layout".into());
        }
        let mut h = Sha256::new();
        h.update(active.to_le_bytes());
        if active & 1 != 0 {
            h.update(&bytes[..24]);
            for i in 0..8 {
                h.update(&bytes[32 + i * 16..42 + i * 16]);
            }
        }
        if active & 2 != 0 {
            h.update(&bytes[24..32]);
            h.update(&bytes[160..416]);
        }
        let cpuid = checked(
            "evidence KVM_GET_CPUID2",
            self.cpu.get_cpuid2(KVM_MAX_CPUID_ENTRIES),
        )?;
        let mut components: Vec<_> = cpuid
            .as_slice()
            .iter()
            .filter(|e| {
                e.function == 0xd && e.index >= 2 && e.index < 64 && active & (1u64 << e.index) != 0
            })
            .collect();
        components.sort_by_key(|e| e.index);
        let mut seen = 3u64;
        for e in components {
            let start = e.ebx as usize;
            let end = start
                .checked_add(e.eax as usize)
                .ok_or("PV XSAVE component overflow")?;
            let data = bytes
                .get(start..end)
                .ok_or("PV XSAVE active bank exceeds returned native area")?;
            h.update(e.index.to_le_bytes());
            h.update(e.eax.to_le_bytes());
            h.update(data);
            seen |= 1u64 << e.index;
        }
        if active & !seen != 0 {
            return Err("PV XSAVE active bank absent from guest CPUID layout".into());
        }
        Ok(hex(&h.finalize()))
    }
}
