//! Read actual KVM banks without settling I/O, entering the guest or capturing
//! a migration image. Explicit fields omit ABI padding and reserved bytes.
use super::*;
use crate::machine::pv::worker::evidence::part;
use seedbox_machine::machine::hex;
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
/// The exception KVM would deliver. Without `injected` or `pending`, KVM keeps
/// the vector and error code of an earlier host-assisted delivery, which
/// depends on host exits and is never delivered again, so it is canonicalized.
// Evidence describes what the next entry delivers; KVM ignores these fields on
// restore unless injected or pending is set.
pub(super) fn exception_evidence(e: &kvm_vcpu_events) -> [u64; 7] {
    let x = &e.exception;
    if x.injected == 0 && x.pending == 0 {
        return [0; 7];
    }
    [
        x.injected as u64,
        x.nr as u64,
        x.has_error_code as u64,
        x.pending as u64,
        x.error_code as u64,
        e.exception_has_payload as u64,
        e.exception_payload,
    ]
}
/// The interrupt KVM would deliver; its vector and kind matter only when
/// injected, while the interrupt shadow always does.
pub(super) fn interrupt_evidence(e: &kvm_vcpu_events) -> [u8; 4] {
    let i = &e.interrupt;
    if i.injected == 0 {
        return [0, 0, 0, i.shadow];
    }
    [i.injected, i.nr, i.soft, i.shadow]
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
                &json!({"exception":exception_evidence(&e),"interrupt":interrupt_evidence(&e),"nmi":[e.nmi.injected,e.nmi.pending,e.nmi.masked],"sipi":e.sipi_vector,"flags":e.flags,"smi":[e.smi.smm,e.smi.pending,e.smi.smm_inside_nmi,e.smi.latched_init],"triple_fault":e.triple_fault.pending}),
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
        let cpuid = checked(
            "evidence KVM_GET_CPUID2",
            self.cpu.get_cpuid2(KVM_MAX_CPUID_ENTRIES),
        )?;
        // User state components in the standard (non-compacted) layout:
        // CPUID.(0DH, i).ECX bit 0 marks supervisor components, EBX is the offset.
        let components: Vec<_> = cpuid
            .as_slice()
            .iter()
            .filter(|e| e.function == 0xd && e.index >= 2 && e.index < 64)
            .filter(|e| e.ecx & 1 == 0 && e.ebx != 0)
            .map(|e| VectorComponent {
                index: e.index,
                offset: e.ebx,
                size: e.eax,
            })
            .collect();
        canonical_vector_digest(&bytes, &components)
    }
}

/// One user XSAVE state component from guest CPUID leaf 0DH.
#[derive(Clone, Copy, Debug)]
pub(super) struct VectorComponent {
    pub(super) index: u32,
    pub(super) offset: u32,
    pub(super) size: u32,
}

const X87_INIT: [u8; 24] = {
    let mut init = [0; 24];
    // FCW 037FH; FSW, abridged FTW (all empty), FOP, FIP and FDP are zero.
    init[0] = 0x7f;
    init[1] = 0x03;
    init
};
const MXCSR_INIT: [u8; 4] = 0x1f80u32.to_le_bytes();

/// Digest of the architectural vector state in a standard-format XSAVE buffer.
///
/// Host FPU save/restore may mark an initial component present or absent
/// (XINUSE), and absent components leave ignored payload bytes. Both restore
/// the same registers, so each component contributes its effective value: its
/// bytes when present, its initial value when absent. The XSTATE_BV bitmap and
/// the host-constant MXCSR_MASK are not hashed.
pub(super) fn canonical_vector_digest(
    bytes: &[u8],
    components: &[VectorComponent],
) -> Result<String> {
    if bytes.len() < 576 {
        return Err("PV XSAVE header truncated".into());
    }
    let active = u64::from_le_bytes(bytes[512..520].try_into().expect("eight bytes"));
    let compact = u64::from_le_bytes(bytes[520..528].try_into().expect("eight bytes"));
    if compact != 0 {
        return Err("PV evidence requires standard KVM XSAVE component layout".into());
    }
    let mut h = Sha256::new();
    h.update(b"seedbox-pv-vectors-v2\0");
    // x87: control/status words, pointers and the eight 80-bit registers.
    if active & 1 != 0 {
        h.update(&bytes[..24]);
        for i in 0..8 {
            h.update(&bytes[32 + i * 16..42 + i * 16]);
        }
    } else {
        h.update(X87_INIT);
        h.update([0u8; 80]);
    }
    // MXCSR belongs to the SSE and AVX components; absent both, it is initial.
    if active & 0b110 != 0 {
        h.update(&bytes[24..28]);
    } else {
        h.update(MXCSR_INIT);
    }
    if active & 2 != 0 {
        h.update(&bytes[160..416]);
    } else {
        h.update([0u8; 256]);
    }
    let mut components = components.to_vec();
    components.sort_by_key(|c| c.index);
    let mut seen = 3u64;
    for c in components {
        let bit = 1u64 << c.index;
        seen |= bit;
        h.update(c.index.to_le_bytes());
        h.update(c.size.to_le_bytes());
        if active & bit != 0 {
            let start = c.offset as usize;
            let end = start
                .checked_add(c.size as usize)
                .ok_or("PV XSAVE component overflow")?;
            h.update(
                bytes
                    .get(start..end)
                    .ok_or("PV XSAVE active bank exceeds returned native area")?,
            );
        } else {
            // Every user component above SSE initializes to zeros.
            h.update(vec![0u8; c.size as usize]);
        }
    }
    if active & !seen != 0 {
        return Err("PV XSAVE active bank absent from guest CPUID layout".into());
    }
    Ok(hex(&h.finalize()))
}

#[cfg(test)]
mod vector_tests {
    use super::*;

    const YMM: VectorComponent = VectorComponent {
        index: 2,
        offset: 576,
        size: 256,
    };
    const OPMASK: VectorComponent = VectorComponent {
        index: 5,
        offset: 1088,
        size: 64,
    };

    fn area(active: u64) -> Vec<u8> {
        let mut bytes = vec![0u8; 1152];
        bytes[..2].copy_from_slice(&[0x7f, 0x03]);
        bytes[24..28].copy_from_slice(&MXCSR_INIT);
        // Host-constant mask; never part of the guest's state.
        bytes[28..32].copy_from_slice(&0xffffu32.to_le_bytes());
        bytes[512..520].copy_from_slice(&active.to_le_bytes());
        bytes
    }
    fn digest(bytes: &[u8]) -> String {
        canonical_vector_digest(bytes, &[YMM, OPMASK]).unwrap()
    }

    #[test]
    fn initial_components_digest_alike_whether_marked_present_or_absent() {
        let absent = area(0);
        let mut stale = area(0);
        // Ignored payload bytes of absent components.
        stale[160..416].fill(0xaa);
        stale[576..832].fill(0xbb);
        stale[1088..1152].fill(0xcc);
        stale[2..24].fill(0xdd);
        let present = area(0b100111);
        for other in [&stale, &present] {
            assert_eq!(digest(&absent), digest(other));
        }
        let mut mask = area(0b100111);
        mask[28..32].copy_from_slice(&0xbfffu32.to_le_bytes());
        assert_eq!(digest(&absent), digest(&mask));
    }

    #[test]
    fn every_architectural_difference_remains_visible() {
        let base = digest(&area(0b100111));
        let mut changes = Vec::new();
        for (offset, active) in [
            (0, 0b1),     // FCW
            (40, 0b1),    // ST0
            (24, 0b110),  // MXCSR
            (160, 0b10),  // XMM0
            (700, 0b100), // YMM upper half
            (1100, 0b100000),
        ] {
            let mut bytes = area(0b100111 | active);
            bytes[offset] ^= 1;
            changes.push(digest(&bytes));
        }
        for changed in &changes {
            assert_ne!(changed, &base);
        }
        let mut mxcsr = area(0b100);
        mxcsr[24] ^= 1;
        assert_ne!(digest(&mxcsr), base, "AVX alone keeps MXCSR live");
    }

    #[test]
    fn rejects_compacted_truncated_and_unknown_active_components() {
        assert!(canonical_vector_digest(&[0; 100], &[]).is_err());
        let mut compact = area(0);
        compact[527] = 0x80;
        assert!(canonical_vector_digest(&compact, &[]).is_err());
        assert!(canonical_vector_digest(&area(1 << 9), &[YMM]).is_err());
    }
}
