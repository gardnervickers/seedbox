//! Linux x86-64 KVM adapter with a userspace interrupt controller. It holds one
//! unfinished port or MMIO instruction at most; host cancellations hold none.
//! All deterministic scheduling and device policy belong to Machine.
use super::{memory::Memory, Result};
use kvm_bindings::*;
use kvm_ioctls::{Cap, Kvm, VcpuExit, VcpuFd, VmFd};
use serde::{Deserialize, Serialize};
use std::{os::fd::AsRawFd, rc::Rc};
mod evidence;

#[derive(Clone, Debug)]
pub(in super::super) enum Exit {
    Mmio {
        address: u64,
        width: u8,
        write: bool,
        value: u64,
    },
    Port {
        port: u16,
        width: u8,
        write: bool,
        value: u64,
    },
    /// HLT; `interrupts` is the guest's RFLAGS.IF.
    Halt {
        interrupts: bool,
    },
    /// The requested interrupt window opened at an instruction boundary.
    Window,
    /// Triple fault or other architectural shutdown.
    Shutdown,
    Interrupted,
}

/// Initial long-mode register state for the 64-bit boot protocol.
pub(in super::super) struct Boot {
    pub(super) rip: u64,
    pub(super) rsi: u64,
    pub(super) rsp: u64,
    pub(super) cr3: u64,
    pub(super) gdt: u64,
    pub(super) code: u16,
    pub(super) data: u16,
}

/// Complete architectural vCPU state. KVM structures are kept as their exact
/// ABI bytes; MSRs as index/value pairs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(in super::super) struct Architecture {
    regs: Vec<u8>,
    sregs: Vec<u8>,
    xsave: Vec<u8>,
    xcrs: Vec<u8>,
    msrs: Vec<(u32, u64)>,
    debugregs: Vec<u8>,
    events: Vec<u8>,
    mp_state: u32,
}

pub(in super::super) trait Backend: Sized {
    fn create(ram: Rc<Memory>, page: Rc<Memory>) -> Result<Self>;
    fn boot(&mut self, boot: Boot) -> Result<()>;
    fn enter(&mut self) -> Result<Exit>;
    fn complete(&mut self, response: Option<u64>) -> Result<()>;
    /// Canonical owed native IO only; never settle/enter during observation.
    fn canonical_pending_io(&self) -> Result<Vec<u8>> {
        Err("native pending IO observation unavailable".into())
    }
    /// The vCPU can take an external interrupt at its next entry.
    fn interruptible(&mut self) -> bool;
    fn inject(&mut self, vector: u8) -> Result<()>;
    fn request_window(&mut self, enabled: bool);
    /// Guest instruction pointer, for diagnostics only.
    fn location(&self) -> u64;
    fn capture(&mut self) -> Result<Architecture>;
    /// Actual register observation, never KVM_RUN or a snapshot-byte digest.
    fn stopped_parts(&mut self) -> Result<Vec<(String, String)>> {
        Err("PV actual stopped register evidence unavailable for this backend".into())
    }
    /// Synchronous backends already complete at complete(); KVM settles its
    /// owed I/O within execution, before exposing a configured stopped boundary.
    fn finish_stop(&mut self) -> Result<()> {
        Ok(())
    }
    fn restore(&mut self, state: &Architecture) -> Result<()>;
}

fn checked<T>(operation: &str, result: std::result::Result<T, kvm_ioctls::Error>) -> Result<T> {
    result.map_err(|e| format!("PV {operation}: {e}"))
}
fn raw<T>(value: &T) -> Vec<u8> {
    // SAFETY: KVM ABI structures are plain old data without padding invariants.
    unsafe { std::slice::from_raw_parts(value as *const T as *const u8, std::mem::size_of::<T>()) }
        .to_vec()
}
fn from_raw<T>(bytes: &[u8], what: &str) -> Result<T> {
    if bytes.len() != std::mem::size_of::<T>() {
        return Err(format!("PV snapshot {what} size"));
    }
    // SAFETY: every bit pattern of these KVM ABI structures is a valid value.
    unsafe {
        let mut value = std::mem::MaybeUninit::<T>::zeroed();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), value.as_mut_ptr() as *mut u8, bytes.len());
        Ok(value.assume_init())
    }
}

/// CPUID visible to the guest: no APIC, host entropy, time sources,
/// monitor/wait, performance counters, hypervisor leaves or host topology.
/// TSC_ADJUST is hidden too: KVM changes it when host TSC adjustments (such as
/// a host suspend) shift the guest TSC, and Linux rereads it while idle.
fn guest_cpuid(mut cpuid: CpuId) -> CpuId {
    for entry in cpuid.as_mut_slice() {
        match (entry.function, entry.index) {
            (1, _) => {
                // One logical processor, APIC ID 0.
                entry.ebx &= 0xffff;
                entry.ebx |= 1 << 16;
                // RDRAND, hypervisor, TSC-deadline, x2APIC, MONITOR, PDCM.
                entry.ecx &= !(1 << 30 | 1 << 31 | 1 << 24 | 1 << 21 | 1 << 3 | 1 << 15);
                // Local APIC.
                entry.edx &= !(1 << 9);
            }
            (6, _) => entry.ecx &= !1, // APERF/MPERF.
            (7, 0) => {
                entry.ebx &= !(1 << 18 | 1 << 4 | 1 << 11 | 1 << 1); // RDSEED, HLE, RTM, TSC_ADJUST.
                entry.ecx &= !(1 << 22 | 1 << 5); // RDPID, WAITPKG.
            }
            // Performance monitoring, topology, and host TSC/core frequencies
            // (the virtual TSC is 1 GHz regardless of the host).
            (0xa, _)
            | (0xb, _)
            | (0x15, _)
            | (0x16, _)
            | (0x1f, _)
            | (0x8000_001e, _)
            | (0x8000_0022, _) => {
                entry.eax = 0;
                entry.ebx = 0;
                entry.ecx = 0;
                entry.edx = 0;
            }
            (4, _) | (0x8000_001d, _) => entry.eax &= !(0x3f << 26),
            // SVM, core/NB/LLC performance counters, MONITORX.
            (0x8000_0001, _) => entry.ecx &= !(1 << 2 | 1 << 23 | 1 << 24 | 1 << 28 | 1 << 29),
            (0x8000_0007, _) => entry.edx = 1 << 8, // Invariant TSC only.
            (0x8000_0008, _) => {
                entry.ecx &= !0xff;
                entry.ebx &= !(1 << 4); // RDPRU.
            }
            (function, _) if (0x4000_0000..0x5000_0000).contains(&function) => {
                entry.eax = 0;
                entry.ebx = 0;
                entry.ecx = 0;
                entry.edx = 0;
            }
            _ => (),
        }
    }
    cpuid
}

/// The read-only KVM module parameter is authoritative for every x86 vendor:
/// when disabled, KVM creates VMs without a PMU and reports no disable capability.
/// Accept only the kernel boolean's exact disabled representation; inaccessible
/// or unexpected contents are not evidence. An open KVM fd prevents module unload.
fn module_pmu_disabled(parameter: Option<&str>) -> bool {
    matches!(parameter, Some("N\n" | "N"))
}

/// Without the disable capability or a globally disabled PMU, Intel's
/// architectural PMU version zero proves KVM has no vPMU. This is not true of
/// AMD's legacy K7 counters, which
/// KVM may expose through MSRs/RDPMC even with leaf A zero. Unknown vendors and
/// missing evidence fail closed. Inspect KVM's supported CPUID, not our masked
/// guest CPUID: hiding a feature alone does not disable its counters.
fn pmu_needs_disable(
    capability: i32,
    supported_cpuid: &[kvm_cpuid_entry2],
    module_disabled: bool,
) -> Result<bool> {
    if capability & KVM_PMU_CAP_DISABLE as i32 != 0 {
        return Ok(true);
    }
    if module_disabled {
        return Ok(false);
    }
    let mut vendor_leaves = supported_cpuid.iter().filter(|entry| entry.function == 0);
    let intel = vendor_leaves.next().is_some_and(|entry| {
        [entry.ebx, entry.edx, entry.ecx]
            == [
                u32::from_le_bytes(*b"Genu"),
                u32::from_le_bytes(*b"ineI"),
                u32::from_le_bytes(*b"ntel"),
            ]
    }) && vendor_leaves.next().is_none();
    let mut pmu_leaves = supported_cpuid.iter().filter(|entry| entry.function == 0xa);
    if intel
        && pmu_leaves.next().is_some_and(|entry| entry.eax & 0xff == 0)
        && pmu_leaves.all(|entry| entry.eax & 0xff == 0)
    {
        Ok(false)
    } else {
        Err("PV KVM cannot disable the guest PMU".into())
    }
}

/// MSRs that are host-owned rather than guest state: the TSC family, the
/// APIC base, the TSC deadline, and KVM's paravirtual and Hyper-V synthetic
/// ranges. The guest sees neither paravirtual interface (CPUID hides the
/// hypervisor leaves, and Hyper-V access is CPUID-enforced), while their
/// counters (for example Hyper-V VP_RUNTIME and TIME_REF_COUNT) advance with
/// host time. They are never captured, restored or witnessed.
// Only guest-observable registers are simulated state; host-clock counters the
// guest cannot read would make every run's stopped state differ.
fn host_owned_msr(index: u32) -> bool {
    matches!(index, 0x10 | 0x11 | 0x12 | 0x1b | 0x3b | 0x6e0)
        || (0x4b56_4d00..=0x4b56_4dff).contains(&index)
        || (0x4000_0000..=0x4000_00ff).contains(&index)
}
/// Guest-writable MSRs that this vCPU can read, without host-owned ones.
fn msr_list(kvm: &Kvm, cpu: &VcpuFd) -> Result<Vec<u32>> {
    let list = checked("KVM_GET_MSR_INDEX_LIST", kvm.get_msr_index_list())?;
    let mut indices = Vec::new();
    for &index in list.as_slice() {
        if host_owned_msr(index) {
            continue;
        }
        let mut one = Msrs::from_entries(&[kvm_msr_entry {
            index,
            ..Default::default()
        }])
        .map_err(|e| format!("PV MSR list: {e:?}"))?;
        if matches!(cpu.get_msrs(&mut one), Ok(1)) {
            indices.push(index);
        }
    }
    Ok(indices)
}

enum Held {
    Input { width: usize },
    Mmio { width: usize },
    Other,
}

pub(crate) struct Native {
    cpu: VcpuFd,
    _vm: VmFd,
    msrs: Vec<u32>,
    held: Option<Held>,
    injected: bool,
    _ram: Rc<Memory>,
    _page: Rc<Memory>,
}

impl Native {
    fn read_msrs(&self) -> Result<Vec<(u32, u64)>> {
        let entries: Vec<_> = self
            .msrs
            .iter()
            .map(|&index| kvm_msr_entry {
                index,
                ..Default::default()
            })
            .collect();
        let mut msrs = Msrs::from_entries(&entries).map_err(|e| format!("PV MSRs: {e:?}"))?;
        let read = checked("KVM_GET_MSRS", self.cpu.get_msrs(&mut msrs))?;
        if read != entries.len() {
            return Err(format!("PV KVM_GET_MSRS read {read} of {}", entries.len()));
        }
        Ok(msrs.as_slice().iter().map(|e| (e.index, e.data)).collect())
    }
    /// Finish a held port/MMIO instruction without running guest code, so the
    /// architectural state is complete.
    fn settle(&mut self) -> Result<()> {
        if self.held.is_some() {
            return Err("PV KVM capture before completion".into());
        }
        self.cpu.set_kvm_immediate_exit(1);
        let result = self.cpu.run().map(|exit| format!("{exit:?}"));
        self.cpu.set_kvm_immediate_exit(0);
        match result {
            Err(e) if e.errno() == libc::EINTR => Ok(()),
            Err(e) => Err(format!("PV KVM completing I/O: {e}")),
            Ok(exit) => Err(format!("PV KVM exit while completing I/O: {exit}")),
        }
    }
}

impl Backend for Native {
    fn create(ram: Rc<Memory>, page: Rc<Memory>) -> Result<Self> {
        let kvm = checked("open /dev/kvm", Kvm::new())?;
        if !kvm.check_extension(Cap::ImmediateExit) {
            return Err("PV KVM immediate exit capability required".into());
        }
        let vm = checked("KVM_CREATE_VM", kvm.create_vm())?;
        let supported_cpuid = checked(
            "KVM_GET_SUPPORTED_CPUID",
            kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES),
        )?;
        // Host performance counters are not guest time sources: no vPMU.
        // KVM with its PMU disabled globally may offer no disable capability.
        let pmu_parameter = std::fs::read_to_string("/sys/module/kvm/parameters/enable_pmu").ok();
        if pmu_needs_disable(
            vm.check_extension_raw(KVM_CAP_PMU_CAPABILITY.into()),
            supported_cpuid.as_slice(),
            module_pmu_disabled(pmu_parameter.as_deref()),
        )? {
            checked(
                "disable guest PMU",
                vm.enable_cap(&kvm_enable_cap {
                    cap: KVM_CAP_PMU_CAPABILITY,
                    args: [KVM_PMU_CAP_DISABLE.into(), 0, 0, 0],
                    ..Default::default()
                }),
            )?;
        }
        // Required by VMX; harmless on SVM. Reserved from every RAM segment.
        checked("KVM_SET_TSS_ADDR", vm.set_tss_address(0xfffb_d000))?;
        // KVM reserves three TSS pages and one identity-map page; both are
        // wholly inside the layout's high aperture, even with RAM above 4 GiB.
        checked(
            "KVM_SET_IDENTITY_MAP_ADDR",
            vm.set_identity_map_address(0xfffb_c000),
        )?;
        let layout = ram.layout()?;
        for (slot, region) in layout.segments.iter().enumerate() {
            unsafe {
                checked(
                    "map guest RAM segment",
                    vm.set_user_memory_region(kvm_userspace_memory_region {
                        slot: slot as u32,
                        flags: 0,
                        guest_phys_addr: region.start,
                        memory_size: region.end - region.start,
                        userspace_addr: ram.ptr().add((region.start - layout.base) as usize) as u64,
                    }),
                )?;
            }
        }
        unsafe {
            checked(
                "map PV page",
                vm.set_user_memory_region(kvm_userspace_memory_region {
                    slot: layout.segments.len() as u32,
                    flags: 0,
                    guest_phys_addr: 0x0a00_0000,
                    memory_size: page.len() as u64,
                    userspace_addr: page.ptr() as u64,
                }),
            )?;
        }
        let cpu = checked("KVM_CREATE_VCPU", vm.create_vcpu(0))?;
        // Without this, KVM serves Hyper-V synthetic MSRs (host-time counters)
        // to a guest RDMSR even though CPUID advertises no Hyper-V interface.
        if kvm.check_extension_raw(KVM_CAP_HYPERV_ENFORCE_CPUID.into()) <= 0 {
            return Err("PV KVM cannot enforce Hyper-V CPUID".into());
        }
        checked(
            "enforce Hyper-V CPUID",
            cpu.enable_cap(&kvm_enable_cap {
                cap: KVM_CAP_HYPERV_ENFORCE_CPUID,
                args: [1, 0, 0, 0],
                ..Default::default()
            }),
        )?;
        checked(
            "KVM_SET_CPUID2",
            cpu.set_cpuid2(&guest_cpuid(supported_cpuid)),
        )?;
        let msrs = msr_list(&kvm, &cpu)?;
        Ok(Self {
            cpu,
            _vm: vm,
            msrs,
            held: None,
            injected: false,
            _ram: ram,
            _page: page,
        })
    }
    fn boot(&mut self, boot: Boot) -> Result<()> {
        // KVM derives CPUID.APIC from IA32_APIC_BASE.EN. Disable the absent
        // local APIC so Linux uses the 8259 PIC alone.
        let disabled = Msrs::from_entries(&[kvm_msr_entry {
            index: 0x1b,
            data: 0,
            ..Default::default()
        }])
        .map_err(|e| format!("PV MSRs: {e:?}"))?;
        if checked("KVM_SET_MSRS", self.cpu.set_msrs(&disabled))? != 1 {
            return Err("PV KVM could not disable the local APIC".into());
        }
        let mut sregs = checked("KVM_GET_SREGS", self.cpu.get_sregs())?;
        let data = kvm_segment {
            base: 0,
            limit: 0xffff_ffff,
            selector: boot.data,
            type_: 3,
            present: 1,
            dpl: 0,
            db: 1,
            s: 1,
            l: 0,
            g: 1,
            avl: 0,
            unusable: 0,
            padding: 0,
        };
        sregs.cs = kvm_segment {
            selector: boot.code,
            type_: 11,
            db: 0,
            l: 1,
            ..data
        };
        sregs.ds = data;
        sregs.es = data;
        sregs.fs = data;
        sregs.gs = data;
        sregs.ss = data;
        sregs.tr = kvm_segment {
            selector: 0,
            type_: 11,
            s: 0,
            db: 0,
            g: 0,
            limit: 0xffff,
            ..data
        };
        sregs.gdt = kvm_dtable {
            base: boot.gdt,
            limit: 31,
            padding: [0; 3],
        };
        sregs.idt = kvm_dtable {
            base: 0,
            limit: 0,
            padding: [0; 3],
        };
        sregs.cr0 = 0x8000_0033; // PG | NE | ET | MP | PE.
        sregs.cr3 = boot.cr3;
        sregs.cr4 = 0x20; // PAE.
        sregs.efer = 0x500; // LMA | LME.
        checked("KVM_SET_SREGS", self.cpu.set_sregs(&sregs))?;
        checked(
            "KVM_SET_REGS",
            self.cpu.set_regs(&kvm_regs {
                rip: boot.rip,
                rsi: boot.rsi,
                rsp: boot.rsp,
                rflags: 2,
                ..Default::default()
            }),
        )?;
        checked(
            "KVM_SET_FPU",
            self.cpu.set_fpu(&kvm_fpu {
                fcw: 0x37f,
                mxcsr: 0x1f80,
                ..Default::default()
            }),
        )
    }
    fn enter(&mut self) -> Result<Exit> {
        if self.held.is_some() {
            return Err("PV KVM entry before completion".into());
        }
        let (exit, held) = match self.cpu.run() {
            Ok(VcpuExit::IoIn(port, data)) => {
                if data.len() > 8 {
                    return Err("PV KVM repeated or oversized port input".into());
                }
                let width = data.len();
                (
                    Exit::Port {
                        port,
                        width: width as u8,
                        write: false,
                        value: 0,
                    },
                    Held::Input { width },
                )
            }
            Ok(VcpuExit::IoOut(port, data)) => {
                if data.len() > 8 {
                    return Err("PV KVM repeated or oversized port output".into());
                }
                let mut value = [0; 8];
                value[..data.len()].copy_from_slice(data);
                (
                    Exit::Port {
                        port,
                        width: data.len() as u8,
                        write: true,
                        value: u64::from_le_bytes(value),
                    },
                    Held::Other,
                )
            }
            Ok(VcpuExit::MmioRead(address, data)) => {
                let width = data.len();

                (
                    Exit::Mmio {
                        address,
                        width: width as u8,
                        write: false,
                        value: 0,
                    },
                    Held::Mmio { width },
                )
            }
            Ok(VcpuExit::MmioWrite(address, data)) => {
                if data.len() > 8 {
                    return Err("PV KVM oversized MMIO".into());
                }
                let mut value = [0; 8];
                value[..data.len()].copy_from_slice(data);
                (
                    Exit::Mmio {
                        address,
                        width: data.len() as u8,
                        write: true,
                        value: u64::from_le_bytes(value),
                    },
                    Held::Other,
                )
            }
            Ok(VcpuExit::Hlt) => {
                let interrupts = self.cpu.get_kvm_run().if_flag != 0;
                (Exit::Halt { interrupts }, Held::Other)
            }
            Ok(VcpuExit::IrqWindowOpen) => (Exit::Window, Held::Other),
            Ok(VcpuExit::Shutdown) => (Exit::Shutdown, Held::Other),
            Ok(VcpuExit::Intr) => return Ok(Exit::Interrupted),
            Err(e) if e.errno() == libc::EINTR => return Ok(Exit::Interrupted),
            Err(e) => return Err(format!("PV KVM_RUN: {e}")),
            Ok(exit) => return Err(format!("PV unexpected KVM exit {exit:?}")),
        };
        // A semantic exit means the guest ran, so any injected vector was taken.
        self.injected = false;
        self.held = Some(held);
        Ok(exit)
    }
    fn canonical_pending_io(&self) -> Result<Vec<u8>> {
        if self.held.is_some() {
            return Err("native witness before IO completion".into());
        }
        Ok(vec![0]) // Explicit no owed operation, not a default/unsupported witness.
    }
    fn complete(&mut self, response: Option<u64>) -> Result<()> {
        let held = self.held.take().ok_or("PV no held KVM exit")?;
        let run = self.cpu.get_kvm_run();
        match (held, response) {
            (Held::Input { width }, Some(value)) => unsafe {
                let io = run.__bindgen_anon_1.io;
                if io.count != 1 || io.size as usize != width {
                    return Err("PV KVM repeated port input".into());
                }
                let data = (run as *mut kvm_run as *mut u8).add(io.data_offset as usize);
                std::ptr::copy_nonoverlapping(value.to_le_bytes().as_ptr(), data, width);
            },
            (Held::Mmio { width }, Some(value)) => unsafe {
                let mmio = &mut run.__bindgen_anon_1.mmio;
                if width > 8 || mmio.len as usize != width {
                    return Err("PV KVM response width".into());
                }
                mmio.data[..width].copy_from_slice(&value.to_le_bytes()[..width]);
            },
            (Held::Other, None) => (),
            _ => return Err("PV KVM response does not match held exit".into()),
        }
        Ok(())
    }
    fn interruptible(&mut self) -> bool {
        let injected = self.injected;
        let run = self.cpu.get_kvm_run();
        !injected && run.ready_for_interrupt_injection != 0 && run.if_flag != 0
    }
    fn inject(&mut self, vector: u8) -> Result<()> {
        const KVM_INTERRUPT: libc::c_ulong = 0x4004_ae86;
        let request = kvm_interrupt { irq: vector as u32 };
        // SAFETY: KVM_INTERRUPT reads one kvm_interrupt from a live vCPU fd.
        if unsafe { libc::ioctl(self.cpu.as_raw_fd(), KVM_INTERRUPT, &request) } != 0 {
            return Err(format!(
                "PV KVM_INTERRUPT: {}",
                std::io::Error::last_os_error()
            ));
        }
        self.injected = true;
        Ok(())
    }
    fn request_window(&mut self, enabled: bool) {
        self.cpu.get_kvm_run().request_interrupt_window = enabled as u8;
    }
    fn location(&self) -> u64 {
        self.cpu.get_regs().map_or(0, |regs| regs.rip)
    }
    fn stopped_parts(&mut self) -> Result<Vec<(String, String)>> {
        self.read_evidence()
    }
    fn finish_stop(&mut self) -> Result<()> {
        self.settle()
    }
    fn capture(&mut self) -> Result<Architecture> {
        if self.injected {
            return Err("PV KVM capture with an injection pending entry".into());
        }
        self.settle()?;
        Ok(Architecture {
            regs: raw(&checked("KVM_GET_REGS", self.cpu.get_regs())?),
            sregs: raw(&checked("KVM_GET_SREGS", self.cpu.get_sregs())?),
            xsave: self.read_xsave()?,
            xcrs: raw(&checked("KVM_GET_XCRS", self.cpu.get_xcrs())?),
            msrs: self.read_msrs()?,
            debugregs: raw(&checked("KVM_GET_DEBUGREGS", self.cpu.get_debug_regs())?),
            events: raw(&checked("KVM_GET_VCPU_EVENTS", self.cpu.get_vcpu_events())?),
            mp_state: checked("KVM_GET_MP_STATE", self.cpu.get_mp_state())?.mp_state,
        })
    }
    fn restore(&mut self, state: &Architecture) -> Result<()> {
        if self.held.is_some() || self.injected {
            return Err("PV KVM restore over unfinished entry".into());
        }
        checked(
            "KVM_SET_REGS",
            self.cpu
                .set_regs(&from_raw::<kvm_regs>(&state.regs, "regs")?),
        )?;
        self.restore_xsave(&state.xsave)?;
        checked(
            "KVM_SET_XCRS",
            self.cpu
                .set_xcrs(&from_raw::<kvm_xcrs>(&state.xcrs, "xcrs")?),
        )?;
        checked(
            "KVM_SET_SREGS",
            self.cpu
                .set_sregs(&from_raw::<kvm_sregs>(&state.sregs, "sregs")?),
        )?;
        // Write back only the MSRs that differ from this fresh vCPU. Snapshots
        // taken before host-owned MSRs were excluded still carry them; they are
        // host clocks, not guest state, so they are dropped rather than restored.
        let current = self.read_msrs()?;
        let saved: Vec<_> = state
            .msrs
            .iter()
            .filter(|(index, _)| !host_owned_msr(*index))
            .collect();
        if current.len() != saved.len() || current.iter().zip(&saved).any(|(a, b)| a.0 != b.0) {
            return Err("PV snapshot MSR inventory differs from this host".into());
        }
        let changed: Vec<_> = saved
            .into_iter()
            .zip(&current)
            .filter(|(saved, now)| saved.1 != now.1)
            .map(|(&(index, data), _)| kvm_msr_entry {
                index,
                data,
                ..Default::default()
            })
            .collect();
        if !changed.is_empty() {
            let msrs = Msrs::from_entries(&changed).map_err(|e| format!("PV MSRs: {e:?}"))?;
            let written = checked("KVM_SET_MSRS", self.cpu.set_msrs(&msrs))?;
            if written != changed.len() {
                return Err(format!(
                    "PV KVM_SET_MSRS wrote {written} of {} ({:#x})",
                    changed.len(),
                    changed[written].index
                ));
            }
        }
        let mut events = from_raw::<kvm_vcpu_events>(&state.events, "events")?;
        events.flags |= KVM_VCPUEVENT_VALID_NMI_PENDING;
        checked("KVM_SET_VCPU_EVENTS", self.cpu.set_vcpu_events(&events))?;
        checked(
            "KVM_SET_MP_STATE",
            self.cpu.set_mp_state(kvm_mp_state {
                mp_state: state.mp_state,
            }),
        )?;
        checked(
            "KVM_SET_DEBUGREGS",
            self.cpu
                .set_debug_regs(&from_raw::<kvm_debugregs>(&state.debugregs, "debugregs")?),
        )?;
        // The kvm_run interruptibility outputs are stale for this fresh vCPU;
        // an immediate window exit refreshes them from the restored state.
        self.request_window(false);
        Ok(())
    }
}

#[cfg(test)]
mod tests;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl seedbox_machine::snapshot::Record for Architecture {
    const MIN: u64 = <Vec<u8> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<u8> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<u8> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<u8> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<(u32, u64)> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<u8> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<u8> as seedbox_machine::snapshot::Record>::MIN
        + <u32 as seedbox_machine::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            regs,
            sregs,
            xsave,
            xcrs,
            msrs,
            debugregs,
            events,
            mp_state,
        } = self;
        regs.encode(w)?;
        sregs.encode(w)?;
        xsave.encode(w)?;
        xcrs.encode(w)?;
        msrs.encode(w)?;
        debugregs.encode(w)?;
        events.encode(w)?;
        mp_state.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            regs: seedbox_machine::snapshot::Record::decode(r)?,
            sregs: seedbox_machine::snapshot::Record::decode(r)?,
            xsave: seedbox_machine::snapshot::Record::decode(r)?,
            xcrs: seedbox_machine::snapshot::Record::decode(r)?,
            msrs: seedbox_machine::snapshot::Record::decode(r)?,
            debugregs: seedbox_machine::snapshot::Record::decode(r)?,
            events: seedbox_machine::snapshot::Record::decode(r)?,
            mp_state: seedbox_machine::snapshot::Record::decode(r)?,
        })
    }
}

impl Architecture {
    pub(in super::super) fn validate_snapshot(&self) -> Result<()> {
        if self.regs.len() != std::mem::size_of::<kvm_regs>()
            || self.sregs.len() != std::mem::size_of::<kvm_sregs>()
            || self.xsave.len() < std::mem::size_of::<kvm_xsave>()
            || !self.xsave.len().is_multiple_of(4)
            || self.xcrs.len() != std::mem::size_of::<kvm_xcrs>()
            || self.debugregs.len() != std::mem::size_of::<kvm_debugregs>()
            || self.events.len() != std::mem::size_of::<kvm_vcpu_events>()
        {
            return Err("PV snapshot KVM register shape".into());
        }
        // MSR inventories/order and all register semantics are native trust.
        Ok(())
    }
}
