//! Saved vCPU state, and a shadow copy of guest RAM with a dirty log.
//!
//! A world fork puts a machine back by copying only the pages written since
//! the fork point: pages written by the guest come from the KVM dirty log,
//! pages written by the VMM (device DMA, boot loading) are marked explicitly.

use kvm_bindings::*;
use kvm_ioctls::{Kvm, VcpuFd, VmFd};
use std::os::fd::AsRawFd;

const KVM_GET_DIRTY_LOG: libc::c_ulong = 0x4010_ae42;
const PAGE: usize = 4096;

#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct ShadowStats {
    pub synced_pages: u64,
    pub restored_pages: u64,
}

/// Architectural vCPU state that a world fork puts back.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VcpuState {
    regs: kvm_regs,
    sregs: kvm_sregs,
    xsave: Box<kvm_xsave>,
    xcrs: kvm_xcrs,
    #[serde(with = "saved_msrs")]
    msrs: Msrs,
    debugregs: kvm_debugregs,
    events: kvm_vcpu_events,
    mp: kvm_mp_state,
}

// Serialize the actual entries, not the flexible-array wrapper allocation.
mod saved_msrs {
    use super::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(msrs: &Msrs, serializer: S) -> Result<S::Ok, S::Error> {
        msrs.as_slice().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Msrs, D::Error> {
        let entries = Vec::<kvm_msr_entry>::deserialize(deserializer)?;
        Msrs::from_entries(&entries).map_err(serde::de::Error::custom)
    }
}

/// MSRs the guest may change, minus the TSC and paravirt ones; keep those KVM
/// lets the VMM read.
pub fn msr_list(kvm: &Kvm, vcpu: &VcpuFd) -> Result<Msrs, String> {
    let list = kvm
        .get_msr_index_list()
        .map_err(|e| format!("msr list: {e}"))?;
    let mut msr_index = Vec::new();
    for &i in list.as_slice() {
        if is_tsc_or_pv(i) {
            continue;
        }
        let mut one = Msrs::from_entries(&[kvm_msr_entry {
            index: i,
            ..Default::default()
        }])
        .map_err(|e| format!("msrs: {e:?}"))?;
        if matches!(vcpu.get_msrs(&mut one), Ok(1)) {
            msr_index.push(i);
        }
    }
    let entries: Vec<kvm_msr_entry> = msr_index
        .iter()
        .map(|&index| kvm_msr_entry {
            index,
            ..Default::default()
        })
        .collect();
    Msrs::from_entries(&entries).map_err(|e| format!("msrs: {e:?}"))
}

impl VcpuState {
    pub fn new(msrs: Msrs) -> Self {
        Self {
            regs: Default::default(),
            sregs: Default::default(),
            xsave: Box::default(),
            xcrs: Default::default(),
            msrs,
            debugregs: Default::default(),
            events: Default::default(),
            mp: Default::default(),
        }
    }

    /// Read the vCPU. It must have no I/O pending completion.
    pub fn save(&mut self, vcpu: &VcpuFd, what: &str) -> Result<(), String> {
        let e = |part: &str, err: kvm_ioctls::Error| format!("{what} {part}: {err}");
        self.regs = vcpu.get_regs().map_err(|x| e("regs", x))?;
        self.sregs = vcpu.get_sregs().map_err(|x| e("sregs", x))?;
        *self.xsave = vcpu.get_xsave().map_err(|x| e("xsave", x))?;
        self.xcrs = vcpu.get_xcrs().map_err(|x| e("xcrs", x))?;
        let want = self.msrs.as_slice().len();
        let n = vcpu.get_msrs(&mut self.msrs).map_err(|x| e("msrs", x))?;
        if n != want {
            return Err(format!("{what} msrs: read {n} of {want}"));
        }
        self.debugregs = vcpu.get_debug_regs().map_err(|x| e("debugregs", x))?;
        self.events = vcpu.get_vcpu_events().map_err(|x| e("events", x))?;
        self.mp = vcpu.get_mp_state().map_err(|x| e("mp_state", x))?;
        Ok(())
    }

    /// Write the state back. The vCPU must have no I/O pending completion.
    /// Returns the restored registers.
    pub fn restore(&self, vcpu: &VcpuFd, what: &str) -> Result<(kvm_regs, kvm_sregs), String> {
        let e = |part: &str, err: kvm_ioctls::Error| format!("{what} {part}: {err}");
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
                    "{what} msrs: wrote {n} of {} ({:#x})",
                    changed.len(),
                    changed[n].index
                ));
            }
        }
        let mut ev = self.events;
        ev.flags |= KVM_VCPUEVENT_VALID_NMI_PENDING;
        vcpu.set_vcpu_events(&ev).map_err(|x| e("events", x))?;
        vcpu.set_mp_state(self.mp).map_err(|x| e("mp_state", x))?;
        vcpu.set_debug_regs(&self.debugregs)
            .map_err(|x| e("debugregs", x))?;
        Ok((self.regs, self.sregs))
    }
}

pub struct Shadow {
    shadow: crate::machine::SendPtr<u8>,
    size: usize,
    /// Dirty bitmap buffer for KVM_GET_DIRTY_LOG.
    log: Vec<u64>,
    /// Pages written by the VMM since the last fetch.
    vmm: Vec<u64>,
    /// Pages written by anyone since the world's fork point, when one is held.
    since_fork: Option<Vec<u64>>,
    pub stats: ShadowStats,
}

fn is_tsc_or_pv(i: u32) -> bool {
    matches!(i, 0x10 | 0x11 | 0x12 | 0x1b | 0x3b | 0x6e0)
        || (0x4b56_4d00..=0x4b56_4dff).contains(&i)
}

impl Shadow {
    pub fn new(size: usize) -> Result<Self, String> {
        let shadow = crate::machine::map(size)?;
        let pages = size / PAGE;
        Ok(Self {
            shadow: crate::machine::SendPtr(shadow),
            size,
            log: vec![0; pages.div_ceil(64)],
            vmm: vec![0; pages.div_ceil(64)],
            since_fork: None,
            stats: ShadowStats::default(),
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
        if let Some(fork) = self.since_fork.as_mut() {
            for (f, l) in fork.iter_mut().zip(self.log.iter()) {
                *f |= *l;
            }
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

    /// Bring the shadow up to RAM and start tracking every page written from
    /// here on, so a fork can later be put back by copying only those pages.
    pub fn fork_point(&mut self, vm: &VmFd, mem: *mut u8) -> Result<(), String> {
        self.fetch(vm)?;
        self.stats.synced_pages += self.copy_logged(mem, self.shadow.0);
        self.since_fork = Some(vec![0; self.log.len()]);
        Ok(())
    }

    pub fn holds_fork_point(&self) -> bool {
        self.since_fork.is_some()
    }

    /// Put RAM and the shadow back to `image`, the RAM saved at the fork point,
    /// on every page written since. Returns the page count.
    pub fn return_to_fork(
        &mut self,
        vm: &VmFd,
        mem: *mut u8,
        image: *const u8,
    ) -> Result<u64, String> {
        if self.since_fork.is_none() {
            return Err("no fork point held".into());
        }
        self.fetch(vm)?;
        let fork = self.since_fork.as_mut().unwrap();
        std::mem::swap(&mut self.log, fork);
        let n = self.copy_logged(image, mem);
        self.copy_logged(image, self.shadow.0);
        self.log.fill(0);
        self.since_fork.as_mut().unwrap().fill(0);
        Ok(n)
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn architectural_checkpoint_round_trips_registers_xsave_and_msrs() {
        let mut saved = VcpuState::new(
            Msrs::from_entries(&[
                kvm_msr_entry {
                    index: 0x174,
                    data: 0x1234,
                    ..Default::default()
                },
                kvm_msr_entry {
                    index: 0x175,
                    data: 0x5678,
                    ..Default::default()
                },
            ])
            .unwrap(),
        );
        saved.regs.rip = 0xffff_ffff_8100_0042;
        saved.regs.rax = 0x1234_5678_90ab_cdef;
        saved.sregs.cr3 = 0x234000;
        saved.xsave.region[140] = 0xfeed_beef;
        let encoded = serde_json::to_vec(&saved).unwrap();
        let restored: VcpuState = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored.regs, saved.regs);
        assert_eq!(restored.sregs, saved.sregs);
        assert_eq!(restored.xsave.region, saved.xsave.region);
        assert_eq!(restored.msrs.as_slice(), saved.msrs.as_slice());
        assert_eq!(serde_json::to_vec(&restored).unwrap(), encoded);
    }
}
