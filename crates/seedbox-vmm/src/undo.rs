//! Checkpoint and rollback of one machine, for undo-able precise stops.
//!
//! Before the final overflow approach to a stop the machine saves its vCPU
//! state; if the overflow interrupt skids past the stop, the machine is put
//! back and approaches again from further away. Guest RAM is restored from a
//! shadow copy kept equal to RAM at every checkpoint: pages written by the
//! guest come from the KVM dirty log, pages written by the VMM (device DMA,
//! boot loading) are marked explicitly.

use kvm_bindings::*;
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use std::os::fd::AsRawFd;

const KVM_GET_DIRTY_LOG: libc::c_ulong = 0x4010_ae42;
const PAGE: usize = 4096;

#[derive(Default, Debug)]
pub struct UndoStats {
    pub checkpoints: u64,
    pub rollbacks: u64,
    pub synced_pages: u64,
    pub restored_pages: u64,
    pub checkpoint_ns: u128,
    pub rollback_ns: u128,
}

pub struct Undo {
    shadow: *mut u8,
    size: usize,
    /// Dirty bitmap buffer for KVM_GET_DIRTY_LOG.
    log: Vec<u64>,
    /// Pages written by the VMM since the last sync.
    vmm: Vec<u64>,
    msr_index: Vec<u32>,
    /// A checkpoint is held and nothing observable happened since.
    pub valid: bool,
    regs: kvm_regs,
    sregs: kvm_sregs,
    xsave: Box<kvm_xsave>,
    xcrs: kvm_xcrs,
    msrs: Msrs,
    debugregs: kvm_debugregs,
    events: kvm_vcpu_events,
    mp: kvm_mp_state,
    /// Machine-level state restored with the vCPU.
    pub page: [u64; crate::machine::PAGE_FIELDS],
    pub time: u64,
    pub branches: u64,
    pub exit_req: bool,
    pub stats: UndoStats,
}

fn is_tsc_or_pv(i: u32) -> bool {
    matches!(i, 0x10 | 0x11 | 0x12 | 0x1b | 0x3b | 0x6e0) || (0x4b56_4d00..=0x4b56_4dff).contains(&i)
}

impl Undo {
    pub fn new(kvm: &Kvm, vcpu: &VcpuFd, size: usize) -> Result<Self, String> {
        let shadow = crate::machine::map(size)?;
        // MSRs the guest may change, minus the TSC and paravirt ones; keep
        // those KVM lets the VMM read.
        let list = kvm.get_msr_index_list().map_err(|e| format!("msr list: {e}"))?;
        let mut msr_index = Vec::new();
        for &i in list.as_slice() {
            if is_tsc_or_pv(i) {
                continue;
            }
            let mut one = Msrs::from_entries(&[kvm_msr_entry { index: i, ..Default::default() }])
                .map_err(|e| format!("msrs: {e:?}"))?;
            if matches!(vcpu.get_msrs(&mut one), Ok(1)) {
                msr_index.push(i);
            }
        }
        let entries: Vec<kvm_msr_entry> = msr_index
            .iter()
            .map(|&index| kvm_msr_entry { index, ..Default::default() })
            .collect();
        let pages = size / PAGE;
        Ok(Self {
            shadow,
            size,
            log: vec![0; pages.div_ceil(64)],
            vmm: vec![0; pages.div_ceil(64)],
            msrs: Msrs::from_entries(&entries).map_err(|e| format!("msrs: {e:?}"))?,
            msr_index,
            valid: false,
            regs: Default::default(),
            sregs: Default::default(),
            xsave: Box::default(),
            xcrs: Default::default(),
            debugregs: Default::default(),
            events: Default::default(),
            mp: Default::default(),
            page: [0; crate::machine::PAGE_FIELDS],
            time: 0,
            branches: 0,
            exit_req: false,
            stats: UndoStats::default(),
        })
    }

    /// Record a VMM write to guest RAM.
    pub fn mark(&mut self, start: usize, len: usize) {
        if len == 0 {
            return;
        }
        for p in start / PAGE..=(start + len - 1) / PAGE {
            self.vmm[p / 64] |= 1 << (p % 64);
        }
    }

    /// Fetch (and reset) the pages written since the last fetch, by the guest
    /// or the VMM, into `log`.
    fn fetch(&mut self, vm: &VmFd) -> Result<(), String> {
        let d = kvm_dirty_log {
            slot: 0,
            padding1: 0,
            __bindgen_anon_1: kvm_dirty_log__bindgen_ty_1 {
                dirty_bitmap: self.log.as_mut_ptr().cast(),
            },
        };
        let rc = unsafe { libc::ioctl(vm.as_raw_fd(), KVM_GET_DIRTY_LOG, &d) };
        if rc != 0 {
            return Err(format!("dirty log: {}", std::io::Error::last_os_error()));
        }
        for (l, v) in self.log.iter_mut().zip(self.vmm.iter_mut()) {
            *l |= std::mem::take(v);
        }
        Ok(())
    }

    /// Copy every page in `log` from `from` to `to`; returns the page count.
    fn copy_logged(&self, from: *const u8, to: *mut u8) -> u64 {
        let mut n = 0;
        for (w, &bits) in self.log.iter().enumerate() {
            let mut b = bits;
            while b != 0 {
                let p = w * 64 + b.trailing_zeros() as usize;
                b &= b - 1;
                let off = p * PAGE;
                if off + PAGE <= self.size {
                    unsafe { std::ptr::copy_nonoverlapping(from.add(off), to.add(off), PAGE) };
                    n += 1;
                }
            }
        }
        n
    }

    /// Save guest state. The vCPU must have no I/O pending completion.
    pub fn save(&mut self, vm: &VmFd, vcpu: &VcpuFd, mem: *mut u8) -> Result<(), String> {
        let e = |what: &str, err: kvm_ioctls::Error| format!("checkpoint {what}: {err}");
        self.fetch(vm)?;
        self.stats.synced_pages += self.copy_logged(mem, self.shadow);
        self.regs = vcpu.get_regs().map_err(|x| e("regs", x))?;
        self.sregs = vcpu.get_sregs().map_err(|x| e("sregs", x))?;
        *self.xsave = vcpu.get_xsave().map_err(|x| e("xsave", x))?;
        self.xcrs = vcpu.get_xcrs().map_err(|x| e("xcrs", x))?;
        let n = vcpu.get_msrs(&mut self.msrs).map_err(|x| e("msrs", x))?;
        if n != self.msr_index.len() {
            return Err(format!("checkpoint msrs: read {n} of {}", self.msr_index.len()));
        }
        self.debugregs = vcpu.get_debug_regs().map_err(|x| e("debugregs", x))?;
        self.events = vcpu.get_vcpu_events().map_err(|x| e("events", x))?;
        self.mp = vcpu.get_mp_state().map_err(|x| e("mp_state", x))?;
        self.valid = true;
        self.stats.checkpoints += 1;
        Ok(())
    }

    /// Put guest RAM and vCPU back to the checkpoint. The vCPU must have no
    /// I/O pending completion. Returns the restored registers.
    pub fn restore(
        &mut self,
        vm: &VmFd,
        vcpu: &VcpuFd,
        mem: *mut u8,
    ) -> Result<(kvm_regs, kvm_sregs), String> {
        let e = |what: &str, err: kvm_ioctls::Error| format!("rollback {what}: {err}");
        self.fetch(vm)?;
        self.stats.restored_pages += self.copy_logged(self.shadow, mem);
        vcpu.set_regs(&self.regs).map_err(|x| e("regs", x))?;
        unsafe { vcpu.set_xsave(&self.xsave) }.map_err(|x| e("xsave", x))?;
        vcpu.set_xcrs(&self.xcrs).map_err(|x| e("xcrs", x))?;
        vcpu.set_sregs(&self.sregs).map_err(|x| e("sregs", x))?;
        // Write back only the MSRs that changed.
        let mut now = self.msrs.clone();
        vcpu.get_msrs(&mut now).map_err(|x| e("msrs", x))?;
        let changed: Vec<kvm_msr_entry> = self
            .msrs
            .as_slice()
            .iter()
            .zip(now.as_slice())
            .filter(|(a, b)| a.data != b.data)
            .map(|(a, _)| *a)
            .collect();
        if !changed.is_empty() {
            let m = Msrs::from_entries(&changed).map_err(|x| format!("msrs: {x:?}"))?;
            let n = vcpu.set_msrs(&m).map_err(|x| e("msrs", x))?;
            if n != changed.len() {
                return Err(format!(
                    "rollback msrs: wrote {n} of {} ({:#x})",
                    changed.len(),
                    changed[n].index
                ));
            }
        }
        let mut ev = self.events;
        ev.flags |= KVM_VCPUEVENT_VALID_NMI_PENDING;
        vcpu.set_vcpu_events(&ev).map_err(|x| e("events", x))?;
        vcpu.set_mp_state(self.mp).map_err(|x| e("mp_state", x))?;
        vcpu.set_debug_regs(&self.debugregs).map_err(|x| e("debugregs", x))?;
        self.stats.rollbacks += 1;
        Ok((self.regs, self.sregs))
    }
}
