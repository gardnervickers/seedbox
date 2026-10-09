//! Checkpoint and rollback of one machine, for undo-able precise stops.
//!
//! Before the final overflow approach to a stop the machine saves its vCPU
//! state; if the overflow interrupt skids past the stop, the machine is put
//! back and approaches again from further away. Guest RAM is restored from a
//! shadow copy kept equal to RAM at every checkpoint: pages written by the
//! guest come from the KVM dirty log, pages written by the VMM (device DMA,
//! boot loading) are marked explicitly.

use crate::machine::Mapping;
use kvm_bindings::*;
use kvm_ioctls::{Kvm, VcpuFd, VmFd};

const PAGE: usize = 4096;

#[derive(Clone, Default, Debug)]
pub struct UndoStats {
    pub checkpoints: u64,
    pub rollbacks: u64,
    pub synced_pages: u64,
    pub restored_pages: u64,
    pub checkpoint_ns: u128,
    pub rollback_ns: u128,
}

/// Architectural vCPU state that a checkpoint or a world fork puts back.
pub struct VcpuState {
    regs: kvm_regs,
    sregs: kvm_sregs,
    xsave: Box<kvm_xsave>,
    xcrs: kvm_xcrs,
    msrs: Msrs,
    debugregs: kvm_debugregs,
    events: kvm_vcpu_events,
    mp: kvm_mp_state,
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

    /// The first part, if any, in which two saved states differ, among those
    /// a checkpoint restores that the guest can change by running: registers,
    /// FPU/XSAVE, XCRs, MSRs other than free-running counters, and pending
    /// events (not KVM's bookkeeping flags).
    pub fn differs(&self, o: &Self) -> Option<String> {
        let (e, f) = (&self.events, &o.events);
        let events = e.exception == f.exception
            && e.interrupt == f.interrupt
            && e.nmi == f.nmi
            && e.smi == f.smi
            && e.triple_fault == f.triple_fault
            && e.exception_has_payload == f.exception_has_payload
            && e.exception_payload == f.exception_payload;
        let part = if self.regs != o.regs {
            "regs"
        } else if self.sregs != o.sregs {
            "sregs"
        } else if self.xsave.region != o.xsave.region {
            "xsave"
        } else if self.xcrs != o.xcrs {
            "xcrs"
        } else if !events {
            "events"
        } else {
            let msr = self
                .msrs
                .as_slice()
                .iter()
                .zip(o.msrs.as_slice())
                .find(|(a, b)| {
                    (a.index != b.index || a.data != b.data) && !is_free_running(a.index)
                });
            return msr.map(|(a, _)| format!("msr {:#x}", a.index));
        };
        Some(part.into())
    }

    /// Write the state back. The vCPU must have no I/O pending completion.
    /// Returns the restored registers.
    pub fn restore(&self, vcpu: &VcpuFd, what: &str) -> Result<(kvm_regs, kvm_sregs), String> {
        let e = |part: &str, err: kvm_ioctls::Error| format!("{what} {part}: {err}");
        vcpu.set_regs(&self.regs).map_err(|x| e("regs", x))?;
        // SAFETY: the region came from KVM_GET_XSAVE on this vCPU, so its
        // size matches what KVM_SET_XSAVE reads.
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

pub struct Undo {
    /// Guest RAM as of the last save, sync or fork point.
    shadow: Mapping,
    size: usize,
    /// Dirty bitmap buffer for KVM_GET_DIRTY_LOG.
    log: Vec<u64>,
    /// Pages written by the VMM since the last sync.
    vmm: Vec<u64>,
    /// Pages written by anyone since the world's fork point, when one is held.
    since_fork: Option<Vec<u64>>,
    /// A checkpoint is held and nothing observable happened since.
    pub valid: bool,
    cpu: VcpuState,
    /// Machine-level state restored with the vCPU.
    pub page: [u64; crate::machine::PAGE_FIELDS],
    // The time page is slot 1, outside the slot-0 RAM dirty log. Keep the
    // guest-written ABI-4 word explicitly, without extending legacy digests.
    pub timer_deadline: Option<u64>,
    pub time: u64,
    pub branches: u64,
    pub stats: UndoStats,
}

/// Counters that move while the guest runs whatever it does: APERF/MPERF,
/// AMD's read-only copies and IRPERF, the legacy and core perf counters, and
/// the Hyper-V synthetic MSRs.
fn is_free_running(i: u32) -> bool {
    matches!(
        i,
        0xe7 | 0xe8 | 0xc000_00e7..=0xc000_00e9 | 0xc001_0004..=0xc001_0007 | 0xc001_0200..=0xc001_020b
            // Hyper-V synthetic MSRs, time counters among them. KVM lists them,
            // but the guest sees no Hyper-V CPUID and so cannot use them.
            | 0x4000_0000..=0x4000_ffff
    )
}

fn is_tsc_or_pv(i: u32) -> bool {
    matches!(i, 0x10 | 0x11 | 0x12 | 0x1b | 0x3b | 0x6e0)
        || (0x4b56_4d00..=0x4b56_4dff).contains(&i)
}

impl Undo {
    pub fn new(kvm: &Kvm, vcpu: &VcpuFd, size: usize) -> Result<Self, String> {
        let pages = size / PAGE;
        Ok(Self {
            shadow: Mapping::new(size)?,
            size,
            log: vec![0; pages.div_ceil(64)],
            vmm: vec![0; pages.div_ceil(64)],
            since_fork: None,
            valid: false,
            cpu: VcpuState::new(msr_list(kvm, vcpu)?),
            page: [0; crate::machine::PAGE_FIELDS],
            timer_deadline: None,
            time: 0,
            branches: 0,
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
        self.log = vm.get_dirty_log(0, self.size).map_err(|e| {
            format!(
                "dirty log: {}",
                std::io::Error::from_raw_os_error(e.errno())
            )
        })?;
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

    /// Save guest state. The vCPU must have no I/O pending completion.
    pub fn save(&mut self, vm: &VmFd, vcpu: &VcpuFd, mem: &[u8]) -> Result<(), String> {
        self.fetch(vm)?;
        self.stats.synced_pages += copy_logged(&self.log, mem, self.shadow.as_mut_slice());
        self.cpu.save(vcpu, "checkpoint")?;
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
        mem: &mut [u8],
    ) -> Result<(kvm_regs, kvm_sregs), String> {
        self.fetch(vm)?;
        self.stats.restored_pages += copy_logged(&self.log, self.shadow.as_slice(), mem);
        let restored = self.cpu.restore(vcpu, "rollback")?;
        self.stats.rollbacks += 1;
        Ok(restored)
    }

    /// Bring the shadow up to RAM. A held checkpoint no longer describes RAM.
    pub fn sync(&mut self, vm: &VmFd, mem: &[u8]) -> Result<(), String> {
        self.valid = false;
        self.fetch(vm)?;
        self.stats.synced_pages += copy_logged(&self.log, mem, self.shadow.as_mut_slice());
        Ok(())
    }

    /// Whether every page written since the last sync still holds what it
    /// held then. `written` accumulates those pages across calls (start it
    /// empty after each sync), so every call compares with the sync itself.
    /// The shadow keeps the sync's contents; the pages stay marked, so the
    /// next save, sync or fork point brings them up to RAM. Meaningful only
    /// when nothing was restored or checkpointed since that sync.
    pub fn unchanged_since_sync(
        &mut self,
        vm: &VmFd,
        mem: &[u8],
        written: &mut Vec<u64>,
    ) -> Result<bool, String> {
        self.valid = false;
        self.fetch(vm)?;
        written.resize(self.log.len(), 0);
        for ((w, &l), v) in written
            .iter_mut()
            .zip(self.log.iter())
            .zip(self.vmm.iter_mut())
        {
            *w |= l;
            *v |= l;
        }
        for (i, &bits) in written.iter().enumerate() {
            let mut b = bits;
            while b != 0 {
                let off = (i * 64 + b.trailing_zeros() as usize) * PAGE;
                b &= b - 1;
                if off + PAGE <= self.size {
                    let page = off..off + PAGE;
                    if mem[page.clone()] != self.shadow.as_slice()[page] {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    /// Bring the shadow up to RAM and start tracking every page written from
    /// here on, so a fork can later be put back by copying only those pages.
    pub fn fork_point(&mut self, vm: &VmFd, mem: &[u8]) -> Result<(), String> {
        self.fetch(vm)?;
        self.stats.synced_pages += copy_logged(&self.log, mem, self.shadow.as_mut_slice());
        self.since_fork = Some(vec![0; self.log.len()]);
        self.valid = false;
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
        mem: &mut [u8],
        image: &[u8],
    ) -> Result<u64, String> {
        if self.since_fork.is_none() {
            return Err("no fork point held".into());
        }
        self.fetch(vm)?;
        let fork = self.since_fork.as_mut().unwrap();
        std::mem::swap(&mut self.log, fork);
        let n = copy_logged(&self.log, image, mem);
        copy_logged(&self.log, image, self.shadow.as_mut_slice());
        self.log.fill(0);
        self.since_fork.as_mut().unwrap().fill(0);
        self.valid = false;
        Ok(n)
    }
}

/// Copy every page in `log` from `from` to `to`, two images of guest RAM;
/// returns the page count.
fn copy_logged(log: &[u64], from: &[u8], to: &mut [u8]) -> u64 {
    let mut n = 0;
    for (w, &bits) in log.iter().enumerate() {
        let mut b = bits;
        while b != 0 {
            let p = w * 64 + b.trailing_zeros() as usize;
            b &= b - 1;
            let page = p * PAGE..(p + 1) * PAGE;
            if page.end <= to.len() {
                to[page.clone()].copy_from_slice(&from[page]);
                n += 1;
            }
        }
    }
    n
}
