//! One x86-64 Linux machine: a KVM VM with one vCPU, run only by the world
//! thread. Guest progress is measured in retired conditional branches; the
//! machine can be stopped at an exact branch count (precise preemption).

use crate::boundary::{advance_time, hard_limit, io_charge, missed_soft_stop, PendingIo};
use crate::counter::Counter;
use crate::virtio_console::VirtioConsole;
use crate::devices::{rtc_read, Pic, Pit, Uart};
use crate::stepper::Planner;
use crate::undo::Undo;
use crate::virtio_net::{ReceiveOutcome, VirtioNet};
use kvm_bindings::*;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use sha2::{Digest, Sha256};
use std::os::fd::AsRawFd;
use std::time::Instant;

pub const TIME_PAGE: u64 = 0xd000_0000;
pub const NET_MMIO: u64 = 0xd000_1000;
pub const NET_IRQ: u8 = 5;
pub const CHANNEL_MMIO: u64 = 0xd1000000;
pub const CHANNEL_IRQ: u8 = 6;
const INITRD_ADDR: usize = 0x0800_0000;
/// Default distance (in branches) before a stop target at which we stop
/// trusting the overflow interrupt and advance by breakpoints. It must exceed
/// the PMU interrupt skid; it affects only speed, never where a stop lands.
/// An overshoot aborts the run, which can be rerun with a larger margin.
pub const MARGIN: u64 = 12288;
/// Host-time allowance for overflow-interrupt skid used to size an adaptive
/// margin (observed tail ~10 us, with headroom).
const SKID_NS: f64 = 18_000.0;
/// Distance of the first, rate-measuring overflow before a hard stop.
const APPROACH: u64 = 65536;
/// Floor of the adaptive margin. Skids of 5-14k branches occur in dense code,
/// and the syscall-exit window before a soft stop must outlast them.
const MARGIN_MIN: u64 = 16384;
/// With undo-able stops, the largest final margin tried after rollbacks.
const UNDO_MAX: u64 = 1 << 18;
/// Pending interrupts are injected only when the next stop is further away
/// than this. Part of the execution semantics: changing it changes runs.
pub const INJECT_GUARD: u64 = 65536;
/// Time page fields (u64 index), see kernel/seedbox.h.
const PAGE_SYSCALLS: usize = 3;
const PAGE_PIC_IMR: usize = 4;
const PAGE_NET_ISR: usize = 5;
pub const PAGE_FIELDS: usize = 6;

pub struct BootSpec<'a> {
    pub kernel: &'a [u8],
    pub initrd: &'a [u8],
    pub cmdline: String,
    pub mem: usize,
    pub rng_seed: [u8; 32],
    pub mac: Option<[u8; 6]>,
    pub control_channel: bool,
    /// Final overflow margin for undo-able precise stops; 0 disables them.
    pub undo_margin: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Running,
    Halted,
    Exited(u8),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    Reached,
    Halted,
    Exited,
    /// The guest produced frames; the world must re-plan before continuing.
    Sent,
    /// A committed management request/effect requires global causal replanning.
    Control,
}

#[derive(Default, Debug)]
pub struct Stats {
    pub exits: u64,
    pub pio: u64,
    pub mmio: u64,
    pub halts: u64,
    pub irqs: u64,
    pub window_exits: u64,
    pub eintr: u64,
    pub precise_stops: u64,
    pub steps: u64,
    pub step_ns: u128,
    pub run_ns: u128,
    pub skid_max: u64,
    pub yields: u64,
    pub yield_stops: u64,
    pub skids: Vec<u64>,
    pub kvm_ns: [u128; 8],
    pub kvm_n: [u64; 8],
    /// Runs that passed a soft stop with the yield window closed and could
    /// not be undone (a syscall there would have been a missed stop).
    pub soft_misses: u64,
    pub soft_undos: u64,
    /// Of the soft misses, those with no syscall in the run (provably harmless).
    pub soft_misses_clean: u64,
}

pub struct Machine {
    pub name: String,
    _vm: VmFd,
    vcpu: VcpuFd,
    mem: *mut u8,
    mem_size: usize,
    page: *mut u64,
    pub time: u64,
    pub branches: u64,
    pub pending_io: Option<PendingIo>,
    tsc_latch: u64,
    pub state: State,
    /// Only an explicit guest exit-port write is a workload exit. KVM errors
    /// which also stop the machine must not be mistaken for application status.
    pub guest_exit_code: Option<u8>,
    pub guest_init_error: Option<u8>,
    pic: Pic,
    pit: Pit,
    rtc_index: u8,
    pub com1: Uart,
    pub com2: Uart,
    pub fault_effects: Vec<(u32, u64, bool)>,
    pub control_requests: Vec<(u64, Vec<u8>)>,
    management_framer: crate::control::ManagementFramer,
    pub net: Option<VirtioNet>,
    pub channel: Option<VirtioConsole>,
    pub control_dma_bytes: u64,
    pub control_tx_bytes: u64,
    stepping: bool,
    /// Seeded from authoritative ioctls or a successful synchronized KVM exit.
    sync_regs_ready: bool,
    #[cfg(test)]
    require_io_checkpoint: bool,
    planner: Option<Planner>,
    exit_req: bool,
    /// The guest uses the shared page for interrupt masks and status (ABI 2).
    pv: bool,
    armed_wake: u64,
    /// The pending timer deadline must be delivered at its exact time.
    pit_exact: bool,
    pub exact_timer_ppm: u64,
    /// Last breakpoint plan, for overshoot diagnostics.
    last_plan: (u64, Vec<u64>),
    pub tolerance: u64,
    pub timer_jitter: u64,
    pub timer_rng: Option<crate::rng::Rng>,
    pub margin: u64,
    /// Recompute `margin` from the recent guest branch rate.
    pub adaptive_margin: bool,
    step_started: Option<Instant>,
    /// Undo-able precise stops: checkpoint before the final approach, roll
    /// back on overshoot. `undo_margin` is the initial final margin, `fm` the
    /// current one (grown after rollbacks, reset at deterministic points).
    pub undo: Option<Box<Undo>>,
    undo_margin: u64,
    fm: u64,
    pub stats: Stats,
    pub witness: Sha256,
    pub trace: Option<std::io::BufWriter<std::fs::File>>,
}

fn put(m: &mut [u8], at: usize, v: u64, n: usize) {
    m[at..at + n].copy_from_slice(&v.to_le_bytes()[..n]);
}

pub(crate) fn map(size: usize) -> Result<*mut u8, String> {
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(format!("mmap: {}", std::io::Error::last_os_error()));
    }
    Ok(p.cast())
}

fn guest_cpuid(kvm: &Kvm) -> Result<CpuId, String> {
    let mut c = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| e.to_string())?;
    for e in c.as_mut_slice() {
        match (e.function, e.index) {
            (1, _) => {
                e.ebx = (e.ebx & 0xffff) | (1 << 16);
                // no RDRAND, hypervisor bit, TSC-deadline, x2APIC, MONITOR; no APIC
                e.ecx &= !(1 << 30 | 1 << 31 | 1 << 24 | 1 << 21 | 1 << 3);
                e.edx &= !(1 << 9);
            }
            (6, _) => e.ecx &= !1, // APERF/MPERF
            (7, 0) => {
                e.ebx &= !(1 << 18 | 1 << 4 | 1 << 11); // RDSEED, HLE, RTM
                e.ecx &= !(1 << 22 | 1 << 5); // RDPID, WAITPKG
            }
            (0xb, _) | (0x1f, _) | (0x8000001e, _) | (0x80000022, _) => {
                e.eax = 0;
                e.ebx = 0;
                e.ecx = 0;
                e.edx = 0;
            }
            (4, _) | (0x8000001d, _) => e.eax &= !(0x3f << 26),
            (0x80000001, _) => e.ecx &= !(1 << 2 | 1 << 23), // SVM, core perf counters
            (0x80000007, _) => e.edx = 1 << 8,               // invariant TSC only
            (0x80000008, _) => {
                e.ecx &= !0xff;
                e.ebx &= !(1 << 4); // RDPRU
            }
            (f, _) if (0x4000_0000..0x5000_0000).contains(&f) => {
                e.eax = 0;
                e.ebx = 0;
                e.ecx = 0;
                e.edx = 0;
            }
            _ => {}
        }
    }
    Ok(c)
}

impl Machine {
    pub fn new(kvm: &Kvm, name: &str, spec: &BootSpec) -> Result<Self, String> {
        if !kvm.check_extension(kvm_ioctls::Cap::ImmediateExit) {
            return Err("KVM immediate-exit capability required for I/O completion".into());
        }
        let mem_size = spec.mem;
        let mem = map(mem_size)?;
        let page = map(4096)?;
        let m = unsafe { std::slice::from_raw_parts_mut(mem, mem_size) };
        let k = spec.kernel;
        let setup = (k[0x1f1] as usize + 1) * 512;
        m[0x100000..0x100000 + k.len() - setup].copy_from_slice(&k[setup..]);
        let zp = 0x10000;
        m[zp + 0x1f1..zp + 0x290].copy_from_slice(&k[0x1f1..0x290]);
        put(m, zp + 0x210, 0xff, 1); // type_of_loader
        put(m, zp + 0x211, 0x01, 1); // loadflags: LOADED_HIGH
        put(m, zp + 0x218, INITRD_ADDR as u64, 4);
        put(m, zp + 0x21c, spec.initrd.len() as u64, 4);
        put(m, zp + 0x228, 0x20000, 4);
        put(m, zp + 0x1e8, 2, 1); // e820 entries
        for (i, (addr, size)) in [(0u64, 0x9fc00u64), (0x100000, (mem_size - 0x100000) as u64)]
            .iter()
            .enumerate()
        {
            let at = zp + 0x2d0 + i * 20;
            put(m, at, *addr, 8);
            put(m, at + 8, *size, 8);
            put(m, at + 16, 1, 4);
        }
        put(m, zp + 0x250, 0x30000, 8); // setup_data: SETUP_RNG_SEED
        put(m, 0x30008, 9, 4);
        put(m, 0x3000c, 32, 4);
        m[0x30010..0x30030].copy_from_slice(&spec.rng_seed);
        let cmd = format!("{}\0", spec.cmdline);
        m[0x20000..0x20000 + cmd.len()].copy_from_slice(cmd.as_bytes());
        if INITRD_ADDR + spec.initrd.len() > mem_size {
            return Err("initrd does not fit".into());
        }
        m[INITRD_ADDR..INITRD_ADDR + spec.initrd.len()].copy_from_slice(spec.initrd);

        let vm = kvm.create_vm().map_err(|e| format!("create_vm: {e}"))?;
        unsafe {
            vm.set_user_memory_region(kvm_userspace_memory_region {
                slot: 0,
                flags: if spec.undo_margin > 0 {
                    KVM_MEM_LOG_DIRTY_PAGES
                } else {
                    0
                },
                guest_phys_addr: 0,
                memory_size: mem_size as u64,
                userspace_addr: mem as u64,
            })
            .map_err(|e| format!("ram slot: {e}"))?;
            vm.set_user_memory_region(kvm_userspace_memory_region {
                slot: 1,
                flags: 0,
                guest_phys_addr: TIME_PAGE,
                memory_size: 4096,
                userspace_addr: page as u64,
            })
            .map_err(|e| format!("time page slot: {e}"))?;
        }
        let vcpu = vm.create_vcpu(0).map_err(|e| format!("vcpu: {e}"))?;
        vcpu.set_cpuid2(&guest_cpuid(kvm)?)
            .map_err(|e| format!("cpuid: {e}"))?;
        let mut s = vcpu.get_sregs().map_err(|e| e.to_string())?;
        s.cr0 = 0x31;
        s.cr4 = 0;
        s.efer = 0;
        let seg = kvm_segment {
            base: 0,
            limit: 0xffff_ffff,
            selector: 0x10,
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
        s.ds = seg;
        s.es = seg;
        s.fs = seg;
        s.gs = seg;
        s.ss = seg;
        s.cs = kvm_segment {
            selector: 8,
            type_: 11,
            ..seg
        };
        vcpu.set_sregs(&s).map_err(|e| e.to_string())?;
        vcpu.set_regs(&kvm_regs {
            rip: 0x100000,
            rsi: zp as u64,
            rsp: 0x90000,
            rflags: 2,
            ..Default::default()
        })
        .map_err(|e| e.to_string())?;
        let net = spec.mac.map(VirtioNet::new);
        let undo = if spec.undo_margin > 0 {
            let mut u = Undo::new(kvm, &vcpu, mem_size)?;
            // Everything loaded so far was written by the VMM.
            u.mark(0, 0x100000);
            u.mark(0x100000, k.len() - setup);
            u.mark(INITRD_ADDR, spec.initrd.len());
            Some(Box::new(u))
        } else {
            None
        };
        Ok(Self {
            name: name.to_string(),
            _vm: vm,
            vcpu,
            mem,
            mem_size,
            page: page.cast(),
            time: 0,
            branches: 0,
            pending_io: None,
            tsc_latch: 0,
            state: State::Running,
            guest_exit_code: None,
            guest_init_error: None,
            pic: Pic::new(),
            pit: Pit::default(),
            rtc_index: 0,
            com1: Uart::default(),
            com2: Uart::default(),
            fault_effects: Vec::new(),
            control_requests: Vec::new(),
            management_framer: crate::control::ManagementFramer::default(),
            net,
            channel: spec.control_channel.then(VirtioConsole::new),
            control_dma_bytes:0,
            control_tx_bytes:0,
            stepping: false,
            sync_regs_ready: false,
            #[cfg(test)]
            require_io_checkpoint: false,
            planner: None,
            exit_req: false,
            pv: false,
            armed_wake: 0,
            pit_exact: false,
            exact_timer_ppm: 0,
            last_plan: (0, Vec::new()),
            tolerance: 0,
            timer_jitter: 0,
            timer_rng: None,
            margin: MARGIN,
            adaptive_margin: false,
            step_started: None,
            undo,
            undo_margin: spec.undo_margin,
            fm: spec.undo_margin,
            stats: Stats::default(),
            witness: Sha256::new(),
            trace: None,
        })
    }

    pub fn memory(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.mem, self.mem_size) }
    }

    fn record(&mut self, kind: u8, extra: u64) {
        if let Some(t) = self.trace.as_mut() {
            use std::io::Write;
            let rip = self.vcpu.get_regs().map(|r| r.rip).unwrap_or(0);
            let _ = writeln!(
                t,
                "{} {} {} {:x} {:x}",
                kind as char, self.branches, self.time, extra, rip
            );
        }
        self.witness.update([kind]);
        self.witness.update(self.branches.to_le_bytes());
        self.witness.update(self.time.to_le_bytes());
        self.witness.update(extra.to_le_bytes());
    }

    /// Earliest internal event (timer) for a halted machine.
    pub fn next_deadline(&self) -> u64 {
        self.pit.deadline.unwrap_or(u64::MAX)
    }

    pub fn has_pending_irq(&self) -> bool {
        self.pic.pending().is_some()
    }

    /// Resume a halted machine at virtual time `t`.
    pub fn wake(&mut self, t: u64) {
        self.time = self.time.max(t);
        if self.state == State::Halted {
            self.state = State::Running;
        }
    }

    pub fn deliver_serial(&mut self, bytes: &[u8]) {
        self.com2.rx.extend(bytes);
        if self.com2.edge() {
            self.pic.raise(3);
        }
    }

    /// Strict runtime delivery requires all bytes DMA into posted guest buffers.
    pub fn deliver_control(&mut self,bytes:&[u8])->Result<(),String> {
        let memory=unsafe {std::slice::from_raw_parts_mut(self.mem,self.mem_size)};
        let channel=self.channel.as_mut().ok_or("runtime control channel absent")?;
        let received=channel.receive(memory,bytes)?;
        let complete=received.accepted && channel.pending_rx_bytes()==0;
        self.take_dma();if received.irq {self.pic.raise(CHANNEL_IRQ);}
        if !complete {return Err("control transport receive capacity insufficient at exact delivery time".into())}
        self.control_dma_bytes+=bytes.len() as u64;Ok(())
    }
    fn channel_output(&mut self,bytes:&[u8])->Result<bool,String> {
        if self.com2.output_limit.is_some_and(|limit|self.com2.out.len().checked_add(bytes.len()).is_none_or(|n|n>limit)){return Err("guest control output limit exceeded".into())}
        if let Some(file)=self.com2.output_file.as_mut() {use std::io::Write;file.write_all(bytes).map_err(|e|format!("control log: {e}"))?;}
        self.com2.out.extend_from_slice(bytes);self.control_tx_bytes+=bytes.len() as u64;
        let frames=self.management_framer.receive(bytes)?;let stop=!frames.is_empty();
        self.control_requests.extend(frames.into_iter().map(|f|(self.time,f)));Ok(stop)
    }

    pub fn deliver_frame(&mut self, frame: &[u8]) -> ReceiveOutcome {
        let (mem, size) = (self.mem, self.mem_size);
        let m = unsafe { std::slice::from_raw_parts_mut(mem, size) };
        let outcome = self
            .net
            .as_mut()
            .map(|n| n.receive(m, frame))
            .unwrap_or_default();
        self.take_dma();
        if outcome.irq {
            self.raise_net();
        }
        outcome
    }

    fn page_get(&self, i: usize) -> u64 {
        unsafe { std::ptr::read_volatile(self.page.add(i)) }
    }

    fn page_set(&mut self, i: usize, v: u64) {
        unsafe { std::ptr::write_volatile(self.page.add(i), v) }
    }

    fn raise_net(&mut self) {
        if self.pv {
            let isr = self.page_get(PAGE_NET_ISR);
            self.page_set(PAGE_NET_ISR, isr | 1);
        }
        self.pic.raise(NET_IRQ);
    }

    /// Adopt interrupt masks the guest stored in the shared page.
    fn sync_pic(&mut self) {
        if self.pv {
            self.pic.set_masks(self.page_get(PAGE_PIC_IMR) as u16);
        }
    }

    /// Account device writes to guest RAM for the undo shadow.
    fn take_dma(&mut self) {
        if let Some(n) = self.net.as_mut() {
            match self.undo.as_mut() {
                Some(u) => {
                    for (a, l) in n.dirty.drain(..) {
                        u.mark(a, l);
                    }
                }
                None => n.dirty.clear(),
            }
        }
        if let Some(channel)=self.channel.as_mut() {
            match self.undo.as_mut() {Some(undo)=>{for (address,length) in channel.dirty.drain(..){undo.mark(address,length);}},None=>channel.dirty.clear()}
        }
    }

    /// Complete an I/O instruction the guest exited on, without running it
    /// further, so vCPU state can be saved or replaced.
    fn complete_io(&mut self) -> Result<(), String> {
        self.vcpu.set_kvm_immediate_exit(1);
        let r = self.vcpu.run().map(|e| format!("{e:?}"));
        self.vcpu.set_kvm_immediate_exit(0);
        match r {
            Err(e) if e.errno() == libc::EINTR => Ok(()),
            Err(e) => Err(format!("{}: completing I/O: {e}", self.name)),
            Ok(e) => Err(format!("{}: exit while completing I/O: {e}", self.name)),
        }
    }

    fn pending_io_bytes(&self) -> Vec<u8> {
        self.pending_io.as_ref().map(PendingIo::canonical).unwrap_or_else(|| vec![0])
    }

    pub fn exit_witness(&self) -> String {
        let mut h = self.witness.clone();
        h.update(b"pending_io");
        h.update(self.pending_io_bytes());
        format!("{:x}", h.finalize())
    }

    fn expire_timers(&mut self) -> Result<(), String> {
        while self.pit.expire(self.time)? {
            self.pic.raise(0);
        }
        Ok(())
    }

    /// Device effects and the KVM response have already been committed once.
    /// No guest entry, publication, injection or checkpoint until all cost is paid.
    fn drain_io(&mut self, limit: u64) -> Result<bool, String> {
        while self.pending_io.is_some() {
            self.expire_timers()?;
            let pending = self.pending_io.as_mut().unwrap();
            if pending.remaining_ns == 0 {
                self.complete_io()?;
                self.pending_io = None;
                return Ok(true);
            }
            if self.time == limit {
                return Ok(false);
            }
            self.time = pending.advance(self.time, limit, self.pit.deadline)?;
            self.expire_timers()?;
        }
        Ok(true)
    }

    fn checkpoint(&mut self) -> Result<(), String> {
        let t = Instant::now();
        self.complete_io()?;
        let page: [u64; PAGE_FIELDS] = std::array::from_fn(|i| self.page_get(i));
        let u = self.undo.as_mut().unwrap();
        u.save(&self._vm, &self.vcpu, self.mem)?;
        u.page = page;
        u.time = self.time;
        u.branches = self.branches;
        u.exit_req = self.exit_req;
        u.stats.checkpoint_ns += t.elapsed().as_nanos();
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), String> {
        let t = Instant::now();
        self.complete_io()?;
        let u = self.undo.as_mut().unwrap();
        let (regs, sregs) = u.restore(&self._vm, &self.vcpu, self.mem)?;
        for i in 0..PAGE_FIELDS {
            unsafe { std::ptr::write_volatile(self.page.add(i), u.page[i]) };
        }
        self.time = u.time;
        self.branches = u.branches;
        self.exit_req = u.exit_req;
        u.stats.rollback_ns += t.elapsed().as_nanos();
        // The planner reads registers from the run structure.
        let run = self.vcpu.get_kvm_run();
        run.s.regs.regs = regs;
        run.s.regs.sregs = sregs;
        self.sync_regs_ready = true;
        Ok(())
    }

    fn checked_page_time(&self, limit: u64) -> Result<u64, String> {
        let last = unsafe { std::ptr::read_volatile(self.page.add(1)) };
        advance_time(self.time, last.saturating_sub(self.time), limit)
    }

    fn sync_from_page(&mut self, limit: u64) -> Result<(), String> {
        self.time = self.checked_page_time(limit)?;
        self.sync_pic();
        Ok(())
    }

    fn publish(&mut self) {
        unsafe { std::ptr::write_volatile(self.page, self.time) };
    }

    fn set_stepping(&mut self, on: bool) -> Result<(), String> {
        if on == self.stepping {
            return Ok(());
        }
        if on {
            // The mmap starts with zeroed synchronization fields, even after
            // KVM_SET_REGS/SREGS configured the actual guest. A first short
            // slice must not RF-resume those zeroes as the boot state.
            if !self.sync_regs_ready {
                let regs=self.vcpu.get_regs().map_err(|e|format!("initial step registers: {e}"))?;
                let sregs=self.vcpu.get_sregs().map_err(|e|format!("initial step system registers: {e}"))?;
                let run=self.vcpu.get_kvm_run();
                run.s.regs.regs=regs;
                run.s.regs.sregs=sregs;
                self.sync_regs_ready=true;
            }
            let mem = unsafe { std::slice::from_raw_parts(self.mem, self.mem_size) };
            self.planner = Some(Planner::load(&self.vcpu, mem)?);
        } else {
            self.vcpu
                .set_guest_debug(&kvm_guest_debug::default())
                .map_err(|e| format!("guest debug: {e}"))?;
            self.planner = None;
        }
        self.stepping = on;
        if on {
            self.stats.precise_stops += 1;
            self.step_started = Some(Instant::now());
        } else if let Some(t) = self.step_started.take() {
            self.stats.step_ns += t.elapsed().as_nanos();
        }
        Ok(())
    }

    fn inject(&mut self) {
        let run = self.vcpu.get_kvm_run();
        match self.pic.pending() {
            Some(irq) if run.ready_for_interrupt_injection != 0 && run.if_flag != 0 => {
                let vector = self.pic.take(irq);
                let rc = unsafe {
                    libc::ioctl(
                        self.vcpu.as_raw_fd(),
                        0x4004ae86,
                        &kvm_interrupt { irq: vector },
                    )
                };
                assert_eq!(rc, 0, "KVM_INTERRUPT");
                self.vcpu.get_kvm_run().request_interrupt_window = 0;
                self.stats.irqs += 1;
                self.record(b'I', vector as u64);
            }
            Some(_) => run.request_interrupt_window = 1,
            None => run.request_interrupt_window = 0,
        }
    }

    fn serial_edges(&mut self) {
        if self.com1.edge() {
            self.pic.raise(4);
        }
        if self.com2.edge() {
            self.pic.raise(3);
        }
    }

    /// Run to the first deterministic point at or after `limit - early` (at
    /// the latest an exact stop at `limit`), or until the guest halts, exits or
    /// sends frames. Never runs past `limit`.
    pub fn run_until(
        &mut self,
        counter: &mut Counter,
        limit: u64,
        early: u64,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop, String> {
        let started = Instant::now();
        let r = self.run_inner(counter, limit, early, frames);
        self.stats.run_ns += started.elapsed().as_nanos();
        r
    }

    fn run_inner(
        &mut self,
        counter: &mut Counter,
        limit: u64,
        early: u64,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop, String> {
        // Invariant: entered at a deterministic point. Guest-visible state (time
        // page, interrupts, timer expiry) changes only at deterministic points:
        // natural exits, requested syscall exits and exact counted stops, never
        // at overflow landings or intermediate single steps.
        let slice_end = limit
            .saturating_sub(early)
            .max(self.time.saturating_add(1))
            .min(limit);
        let mut legal = true;
        loop {
            if self.pending_io.is_some() {
                if !self.drain_io(limit)? {
                    self.set_stepping(false)?;
                    return Ok(Stop::Reached);
                }
            }
            if legal {
                // Observable effects follow; a held checkpoint no longer
                // describes the machine.
                if let Some(u) = self.undo.as_mut() {
                    u.valid = false;
                }
                self.fm = self.undo_margin;
                // Guest clock reads can advance time independently of the PMU.
                // Check before expiring timers or accepting the slice boundary.
                self.sync_from_page(hard_limit(
                    limit,
                    self.pit.deadline,
                    self.pit_exact,
                    self.tolerance,
                ))?;
                self.expire_timers()?;
                if self.time >= slice_end {
                    self.set_stepping(false)?;
                    return Ok(Stop::Reached);
                }
                self.publish();
            }
            // Stop at the first natural exit at or after `soft`, at the latest
            // by an exact counted stop at `hard`. Two rules combine: the slice
            // ends in [slice_end, limit], and a timer due at `pit` fires in
            // [pit, pit + tolerance] (exactly at `pit` if so drawn). Neither
            // passes `limit`, the world's causal boundary. Syscalls are not
            // stopping points, so no yield window is requested: a natural exit
            // is never skipped, whatever the overflow skid.
            let pit = self.pit.deadline.unwrap_or(u64::MAX);
            let soft = slice_end.min(pit);
            let hard = hard_limit(limit, self.pit.deadline, self.pit_exact, self.tolerance);
            let remaining = hard - self.time;
            // Interrupt injection depends only on the distance to the stop, never
            // on the stepping margin, so it is deterministic.
            if legal {
                if remaining > INJECT_GUARD {
                    self.inject();
                } else {
                    self.vcpu.get_kvm_run().request_interrupt_window = 0;
                }
            }
            let rem_soft = soft.saturating_sub(self.time);
            let windowed = false;
            let window = false;
            if window != self.exit_req {
                unsafe { std::ptr::write_volatile(self.page.add(2), window as u64) };
                self.exit_req = window;
            }
            let undo = self.undo.is_some();
            let threshold = if undo { self.fm } else { self.margin };
            if remaining > threshold {
                self.set_stepping(false)?;
                let wake = if (self.adaptive_margin || undo) && remaining > APPROACH {
                    // Approach a hard stop in two overflows: the first measures
                    // the branch rate of the code actually running near it.
                    remaining - APPROACH
                } else {
                    // Final approach. With a checkpoint, aim close and undo an
                    // overshoot rather than step through a worst-case skid.
                    if undo && !self.undo.as_ref().unwrap().valid {
                        self.checkpoint()?;
                    }
                    remaining - threshold
                };
                let now = counter.last();
                if wake >= crate::counter::NEVER {
                    counter.disarm();
                    self.armed_wake = 0;
                } else {
                    counter.arm(now, now + wake);
                    self.armed_wake = wake;
                }
            } else {
                if !self.stepping {
                    counter.disarm();
                }
                self.set_stepping(true)?;
                let (r, sr) = {
                    let run = self.vcpu.get_kvm_run();
                    unsafe { (run.s.regs.regs, run.s.regs.sregs) }
                };
                let mem = unsafe { std::slice::from_raw_parts(self.mem, self.mem_size) };
                let planner = self.planner.as_mut().unwrap();
                planner.refresh_from(&sr);
                let targets = planner
                    .plan(mem, &r)
                    .map_err(|e| format!("{}: step plan: {e}", self.name))?;
                Planner::arm(&self.vcpu, &targets)?;
                self.last_plan = (r.rip, targets.clone());
                if targets.contains(&r.rip) {
                    let mut resumed = r;
                    resumed.rflags |= 1 << 16; // RF: do not re-trigger at the current instruction
                    self.vcpu.set_regs(&resumed).map_err(|e| e.to_string())?;
                }
            }
            self.vcpu.get_kvm_run().kvm_valid_regs =
                (KVM_SYNC_X86_REGS | KVM_SYNC_X86_SREGS) as u64;
            let syscalls0 = self.page_get(PAGE_SYSCALLS);
            let c0 = counter.last();
            let t_run = Instant::now();
            let run_ptr = self.vcpu.get_kvm_run() as *mut kvm_run;
            let exit = self.vcpu.run();
            // Requested validity bits alone are not initialized output. A
            // successful run with both bits set provides the synchronized state.
            if exit.is_ok() { self.sync_regs_ready=true; }
            let run_ns = t_run.elapsed().as_nanos();
            let kind = match &exit {
                Err(_) => 0,
                Ok(VcpuExit::Debug(_)) => 1,
                Ok(VcpuExit::IoOut(..)) | Ok(VcpuExit::IoIn(..)) => 2,
                Ok(VcpuExit::MmioRead(..)) | Ok(VcpuExit::MmioWrite(..)) => 3,
                Ok(VcpuExit::Hlt) => 4,
                Ok(VcpuExit::IrqWindowOpen) => 5,
                _ => 6,
            };
            self.stats.kvm_ns[kind] += run_ns;
            self.stats.kvm_n[kind] += 1;
            let c1 = counter.read_fast();
            let delta = c1 - c0;
            let interrupted = matches!(&exit, Err(e) if e.errno() == libc::EINTR);
            let overshoot =
                delta > remaining || (interrupted && !self.stepping && delta == remaining);
            // With the yield window closed, a run must not pass `soft`: a
            // syscall there would have been the stop.
            let soft_miss = missed_soft_stop(
                windowed,
                window,
                delta,
                rem_soft,
                interrupted,
                self.stepping,
            );
            if (overshoot || soft_miss) && self.undo.as_ref().is_some_and(|u| u.valid) {
                if overshoot && self.fm >= UNDO_MAX {
                    return Err(format!(
                        "overshoot on {}: {} branches past stop target at final margin {}",
                        self.name,
                        delta - remaining,
                        self.fm
                    ));
                }
                drop(exit);
                self.rollback()?;
                // A rollback costs far less than stepping, so grow slowly.
                self.fm = (self.fm * 2).min(UNDO_MAX);
                if !overshoot {
                    self.stats.soft_undos += 1;
                }
                legal = false;
                continue;
            }
            // Changing the margin must never change where a stop lands. A
            // paravirtual guest counts syscalls: a run without one skipped no
            // stop. Otherwise the run fails.
            if soft_miss && !overshoot {
                self.stats.soft_misses += 1;
                let syscalls = unsafe { std::ptr::read_volatile(self.page.add(PAGE_SYSCALLS)) };
                if !self.pv || syscalls != syscalls0 {
                    return Err(format!(
                        "soft overshoot on {}: {} branches past (or an ambiguous overflow at) a soft stop before its syscall window opened",
                        self.name,
                        delta - rem_soft
                    ));
                }
                self.stats.soft_misses_clean += 1;
            }
            if overshoot {
                let what = exit
                    .as_ref()
                    .map(|e| format!("{e:?}"))
                    .unwrap_or_else(|e| e.to_string());
                drop(exit);
                let r = self.vcpu.get_regs().ok();
                return Err(format!(
                    "overshoot on {}: {} branches past stop target (stepping={} exit={} rip={:#x?} last_plan={:x?})",
                    self.name,
                    delta - remaining,
                    self.stepping,
                    what,
                    r.map(|r| r.rip),
                    self.last_plan,
                ));
            }
            if interrupted && !self.stepping {
                let skid = delta.saturating_sub(self.armed_wake);
                self.stats.skid_max = self.stats.skid_max.max(skid);
                self.stats.skids.push(skid);
            }
            // Overflow skid is roughly constant in host time, so its size in
            // branches follows the guest's recent branch rate (Go's GC scan loop
            // is several times denser than ordinary code). Size the margin from
            // that rate. Performance only: stops land on the same count.
            if self.adaptive_margin && !self.stepping && run_ns > 20_000 && delta > 0 {
                let rate = delta as f64 / run_ns as f64; // branches per host ns
                self.margin = ((rate * SKID_NS) as u64).clamp(MARGIN_MIN, APPROACH);
            }
            legal = !interrupted;
            if matches!(exit, Ok(VcpuExit::Debug(_))) {
                legal = delta == remaining;
            }
            self.branches += delta;
            self.time += delta;
            if legal {
                // Sent/Hlt/Exited can return before the next loop synchronizes
                // the page. Validate guest clock progress before any exit
                // effects, while preserving the existing publication timing.
                // The exit still borrows vcpu; inspect only the disjoint page
                // and clock fields until its payload has been consumed.
                let last = unsafe { std::ptr::read_volatile(self.page.add(1)) };
                advance_time(self.time, last.saturating_sub(self.time), hard)?;
            }
            self.stats.exits += 1;
            if self.stepping {
                self.stats.steps += 1;
            }
            let now = self.time;
            match exit {
                Err(e) if e.errno() == libc::EINTR => {
                    self.stats.eintr += 1;
                }
                Ok(VcpuExit::Debug(_)) => {}
                Ok(VcpuExit::IoOut(0x52c, _)) => {
                    // Requested yield: a stop if we reached the soft target,
                    // otherwise transparent (nothing observable happens).
                    self.stats.yields += 1;
                    if self.time >= soft {
                        self.stats.yield_stops += 1;
                        self.record(b'Y', 0);
                    } else {
                        legal = false;
                    }
                }
                Ok(VcpuExit::IoOut(port, data)) => {
                    self.stats.pio += 1;
                    let v = data[0];
                    let charge = io_charge(port, true);
                    // Reserve the full cost arithmetically, without requiring
                    // it to fit before this world's next delivery boundary.
                    advance_time(self.time, charge, u64::MAX)?;
                    let descriptor = unsafe { (*run_ptr).__bindgen_anon_1.io };
                    let pending = if charge > 0 {
                        Some(PendingIo::new(self.time, port, true, descriptor.size, descriptor.count, data.to_vec())?)
                    } else { None };
                    #[cfg(test)]
                    if charge>0 && self.require_io_checkpoint {
                        assert!(self.undo.as_ref().is_some_and(|u|u.valid),"charged access must begin with the real held checkpoint");
                    }
                    let after_io = self.time;
                    let mut control_stop = false;
                    match port {
                        0x3f8..=0x3ff => {
                            if port == 0x3f8
                                && v == b'\n'
                                && std::env::var("SEEDBOX_TRACE_CONSOLE").is_ok()
                            {
                                let line_start = self
                                    .com1
                                    .out
                                    .iter()
                                    .rposition(|&b| b == b'\n')
                                    .map(|p| p + 1)
                                    .unwrap_or(0);
                                eprintln!(
                                    "[host {:.3}s guest {:.6}s br {}] {}",
                                    crate::START.with(|s| s.elapsed().as_secs_f64()),
                                    self.time as f64 / 1e9,
                                    self.branches,
                                    String::from_utf8_lossy(&self.com1.out[line_start..])
                                );
                            }
                            self.com1.write(port - 0x3f8, v)?
                        }
                        0x2f8..=0x2ff => {
                            if self.channel.is_some() {self.com2.write(port-0x2f8,v)?;} else if let Some(line) =
                                self.management_framer
                                    .write(&mut self.com2, port - 0x2f8, v)?
                            {
                                self.control_requests.push((after_io, line));
                                control_stop = true;
                            }
                        }
                        0xf6 | 0xf7 => {
                            let id = u32::from_le_bytes(
                                data.try_into()
                                    .map_err(|_| "fault effect must be a 32-bit I/O write")?,
                            );
                            self.fault_effects.push((id, after_io, port == 0xf6));
                            control_stop = true;
                        }
                        0x20 | 0x21 | 0xa0 | 0xa1 => {
                            self.pic.write(port, v);
                            if self.pv {
                                let m = self.pic.masks() as u64;
                                self.page_set(PAGE_PIC_IMR, m);
                            }
                        }
                        0x528 => {
                            self.pv = v >= 2;
                            if self.pv {
                                self.sync_pic();
                            }
                        }
                        0x40..=0x43 | 0x61 | 0x530 => {
                            if port == 0x530 {
                                let hi = data.get(1).copied().unwrap_or(0);
                                let ticks = u16::from_le_bytes([data[0], hi]);
                                self.pit.program(ticks, now);
                            } else {
                                self.pit.write(port, v, now);
                            }
                            if self.pit.fresh {
                                // Seeded timer latency: varies preemption points across seeds.
                                self.pit.fresh = false;
                                if let (Some(d), Some(rng)) =
                                    (self.pit.deadline, self.timer_rng.as_mut())
                                {
                                    if self.timer_jitter > 0 {
                                        self.pit.deadline =
                                            Some(d + rng.below(self.timer_jitter + 1));
                                    }
                                    // Some deadlines interrupt at their exact time, i.e. at an
                                    // arbitrary instruction, rather than the next syscall.
                                    self.pit_exact = self.exact_timer_ppm > 0
                                        && rng.below(1_000_000) < self.exact_timer_ppm;
                                }
                            }
                        }
                        0x70 => self.rtc_index = v & 0x7f,
                        0xe9 => self.com1.transmit(v)?,
                        0xf4 => {
                            self.guest_exit_code = Some(v);
                            self.state = State::Exited(v);
                            self.record(b'X', v as u64);
                            self.set_stepping(false)?;
                            return Ok(Stop::Exited);
                        }
                        0xf5 => {
                            self.guest_init_error = Some(v);
                            self.state = State::Exited(v);
                            self.record(b'E', v as u64);
                            self.set_stepping(false)?;
                            return Ok(Stop::Exited);
                        }
                        _ => {}
                    }
                    self.record(b'O', (port as u64) << 8 | v as u64);
                    self.serial_edges();
                    self.pending_io = pending;
                    if charge > 0 {
                        if let Some(u) = self.undo.as_mut() { u.valid = false; }
                    }
                    if control_stop {
                        self.set_stepping(false)?;
                        return Ok(Stop::Control);
                    }
                }
                Ok(VcpuExit::IoIn(port, data)) => {
                    self.stats.pio += 1;
                    let charge = io_charge(port, false);
                    advance_time(self.time, charge, u64::MAX)?;
                    let descriptor = unsafe { (*run_ptr).__bindgen_anon_1.io };
                    match port {
                        0x520 => {
                            let t = self.time.max(advance_time(self.tsc_latch, 1, hard)?);
                            self.tsc_latch = t;
                            self.time = t;
                            data.copy_from_slice(&(t as u32).to_le_bytes()[..data.len()]);
                        }
                        0x524 => data.copy_from_slice(
                            &((self.tsc_latch >> 32) as u32).to_le_bytes()[..data.len()],
                        ),
                        _ => {
                            let v = match port {
                                0x3f8..=0x3ff => self.com1.read(port - 0x3f8),
                                0x2f8..=0x2ff => self.com2.read(port - 0x2f8),
                                0x20 | 0x21 | 0xa0 | 0xa1 => self.pic.read(port),
                                0x40..=0x42 | 0x61 => self.pit.read(port, now),
                                0x71 => rtc_read(self.rtc_index),
                                _ => 0xff,
                            };
                            data.fill(v);
                        }
                    }
                    let pending = if charge > 0 {
                        Some(PendingIo::new(self.time, port, false, descriptor.size, descriptor.count, data.to_vec())?)
                    } else { None };
                    self.record(b'i', port as u64);
                    self.serial_edges();
                    self.pending_io = pending;
                    if charge > 0 {
                        if let Some(u) = self.undo.as_mut() { u.valid = false; }
                    }
                }
                Ok(VcpuExit::MmioRead(addr, data)) => {
                    self.stats.mmio += 1;
                    let v = if (CHANNEL_MMIO..CHANNEL_MMIO+0x1000).contains(&addr) {
                        self.channel.as_mut().map(|channel|channel.read(addr-CHANNEL_MMIO)).unwrap_or(0)
                    } else {match self.net.as_mut() {
                        Some(n) if (NET_MMIO..NET_MMIO + 0x1000).contains(&addr) => {
                            n.read(addr - NET_MMIO)
                        }
                        _ => 0,
                    }};
                    let bytes = v.to_le_bytes();
                    let len = data.len().min(4);
                    data[..len].copy_from_slice(&bytes[..len]);
                    self.record(b'r', addr);
                }
                Ok(VcpuExit::MmioWrite(addr, data)) => {
                    self.stats.mmio += 1;
                    let mut v = [0u8; 4];
                    let len = data.len().min(4);
                    v[..len].copy_from_slice(&data[..len]);
                    let v = u32::from_le_bytes(v);
                    self.record(b'w', addr << 32 | v as u64);
                    if (CHANNEL_MMIO..CHANNEL_MMIO+0x1000).contains(&addr) {
                        let memory=unsafe{std::slice::from_raw_parts_mut(self.mem,self.mem_size)};
                        if let Some(channel)=self.channel.as_mut() {
                            let output=channel.write(memory,addr-CHANNEL_MMIO,v)?;
                            self.take_dma();if output.irq {self.pic.raise(CHANNEL_IRQ);}
                            if self.channel_output(&output.bytes)? {self.set_stepping(false)?;return Ok(Stop::Control);}
                        }
                    }
                    if (NET_MMIO..NET_MMIO + 0x1000).contains(&addr) {
                        let (mem, size) = (self.mem, self.mem_size);
                        let m = unsafe { std::slice::from_raw_parts_mut(mem, size) };
                        if let Some(n) = self.net.as_mut() {
                            let out = n.write(m, addr - NET_MMIO, v);
                            self.take_dma();
                            if out.irq {
                                self.raise_net();
                            }
                            if !out.frames.is_empty() {
                                frames.extend(out.frames);
                                self.set_stepping(false)?;
                                return Ok(Stop::Sent);
                            }
                        }
                    }
                }
                Ok(VcpuExit::Hlt) => {
                    self.stats.halts += 1;
                    self.record(b'H', 0);
                    // The world checks pending interrupts while halted.
                    self.sync_pic();
                    self.set_stepping(false)?;
                    if self.vcpu.get_kvm_run().if_flag == 0 {
                        self.state = State::Exited(0xfe);
                        return Ok(Stop::Exited);
                    }
                    self.state = State::Halted;
                    return Ok(Stop::Halted);
                }
                Ok(VcpuExit::IrqWindowOpen) => {
                    self.stats.window_exits += 1;
                }
                Ok(VcpuExit::Shutdown) => {
                    self.state = State::Exited(0xff);
                    self.set_stepping(false)?;
                    return Ok(Stop::Exited);
                }
                Ok(other) => return Err(format!("{}: unexpected exit {other:?}", self.name)),
                Err(e) => return Err(format!("{}: KVM_RUN: {e}", self.name)),
            }
        }
    }

    /// Hash of architectural registers and all guest RAM.
    /// Hashes of guest RAM and the vCPU state a checkpoint restores, by part:
    /// general and control registers with segment bases, FPU/XSAVE, XCRs,
    /// pending exception/interrupt/NMI state, syscall/TLS MSRs, the shared time
    /// page and RAM. The digest is the hash of the parts. Not covered: other
    /// MSRs, debug registers and VMM device models.
    pub fn state_digest(&mut self) -> (String, Vec<(&'static str, String)>) {
        let hex = |h: Sha256| format!("{:x}", h.finalize());
        let mut parts = Vec::new();
        let r = self.vcpu.get_regs().unwrap();
        let s = self.vcpu.get_sregs().unwrap();
        let mut h = Sha256::new();
        for v in [
            r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10, r.r11,
            r.r12, r.r13, r.r14, r.r15, r.rip, r.rflags, s.cr0, s.cr3, s.cr4, s.efer, s.cr2, s.cr8,
            s.fs.base, s.gs.base, s.tr.base, s.gdt.base, s.idt.base,
        ] {
            h.update(v.to_le_bytes());
        }
        parts.push(("regs", hex(h)));
        let mut h = Sha256::new();
        for w in self.vcpu.get_xsave().unwrap().region {
            h.update(w.to_le_bytes());
        }
        parts.push(("xsave", hex(h)));
        let mut h = Sha256::new();
        let xcrs = self.vcpu.get_xcrs().unwrap();
        for x in &xcrs.xcrs[..xcrs.nr_xcrs as usize] {
            h.update(x.xcr.to_le_bytes());
            h.update(x.value.to_le_bytes());
        }
        parts.push(("xcrs", hex(h)));
        // Event fields count only while live: KVM keeps the last exception's
        // vector/error code and the last interrupt's vector after delivery, so
        // they differ after a rollback without any guest-visible effect.
        let e = self.vcpu.get_vcpu_events().unwrap();
        let exception = if e.exception.injected != 0 || e.exception.pending != 0 {
            format!(
                "{}/{}/{}/{}/{}",
                e.exception.injected,
                e.exception.pending,
                e.exception.nr,
                e.exception.has_error_code,
                e.exception.error_code
            )
        } else {
            "none".into()
        };
        let interrupt = if e.interrupt.injected != 0 {
            format!("{}/{}", e.interrupt.nr, e.interrupt.soft)
        } else {
            "none".into()
        };
        parts.push((
            "events",
            format!(
                "exc:{exception} int:{interrupt} shadow:{} nmi:{}/{}/{}",
                e.interrupt.shadow, e.nmi.injected, e.nmi.pending, e.nmi.masked,
            ),
        ));
        // KERNEL_GS_BASE, STAR, LSTAR, CSTAR, SFMASK, TSC_AUX, PAT, SYSENTER_*.
        let index = [
            0xc000_0102u32,
            0xc000_0081,
            0xc000_0082,
            0xc000_0083,
            0xc000_0084,
            0xc000_0103,
            0x277,
            0x174,
            0x175,
            0x176,
        ];
        let entries: Vec<kvm_msr_entry> = index
            .iter()
            .map(|&index| kvm_msr_entry {
                index,
                ..Default::default()
            })
            .collect();
        let mut msrs = Msrs::from_entries(&entries).unwrap();
        let n = self.vcpu.get_msrs(&mut msrs).unwrap();
        let mut h = Sha256::new();
        for m in &msrs.as_slice()[..n] {
            h.update(m.index.to_le_bytes());
            h.update(m.data.to_le_bytes());
        }
        parts.push(("msrs", hex(h)));
        let mut h = Sha256::new();
        for i in 0..PAGE_FIELDS {
            h.update(self.page_get(i).to_le_bytes());
        }
        parts.push(("time_page", hex(h)));
        let mut h = Sha256::new();
        h.update(self.memory());
        parts.push(("ram", hex(h)));
        let mut h = Sha256::new();
        h.update(self.pending_io_bytes());
        parts.push(("pending_io", hex(h)));
        let mut all = Sha256::new();
        for (_, d) in &parts {
            all.update(d.as_bytes());
        }
        (hex(all), parts)
    }

    /// First 8 bytes of SHA-256 of each 4 KiB guest RAM page, for locating
    /// differences between runs.
    pub fn page_hashes(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for page in self.memory().chunks(4096) {
            out.extend_from_slice(&Sha256::digest(page)[..8]);
        }
        out
    }
}


#[cfg(test)]
mod io_boundary_tests {
    use super::*;

    fn probe(kvm: &Kvm, port: u16, write: bool, undo: u64) -> Machine {
        // A minimal 32-bit payload using the real Linux boot handoff path.
        // After one charged I/O: conditional branch, marker store, HLT.
        let value=if port==0x530 {1u32} else {0x1234u32};
        let mut code=vec![0xb8];code.extend_from_slice(&value.to_le_bytes());
        code.push(0xba);code.extend_from_slice(&(port as u32).to_le_bytes());
        if port==0x530 {code.extend_from_slice(&[0x66,0xef]);}
        else {code.push(if write {0xee} else {0xec});}
        code.extend_from_slice(&[0x85,0xc0,0x74,0x00,0xc7,0x05,0x00,0x00,0x08,0x00,0x78,0x56,0x34,0x12,0xf4]);
        let mut kernel=vec![0u8;1024];kernel[0x1f1]=1;kernel.extend(code);
        let spec=BootSpec {kernel:&kernel,initrd:&[],cmdline:String::new(),mem:INITRD_ADDR+0x100000,rng_seed:[0;32],mac:None,control_channel:false,undo_margin:undo};
        let mut machine=Machine::new(kvm,"io-probe",&spec).unwrap();
        let mut r=machine.vcpu.get_regs().unwrap();r.rflags=0x202;machine.vcpu.set_regs(&r).unwrap();
        machine.pit.write(0x43,0x34,0);
        machine.pit.program(0x1234,0);
        machine.pit.write(0x43,0,0);
        machine.pic.set_masks(0);
        machine
    }

    fn check_port(kvm: &Kvm, counter: &mut Counter, port: u16, write: bool) {
        let mut m=probe(kvm,port,write,0);
        let initial=m.vcpu.get_regs().unwrap();
        assert_eq!(initial.rip,0x100000);assert_eq!(initial.rsp,0x90000);assert_ne!(initial.rflags&0x200,0);
            let mut frames=Vec::new();
            assert_eq!(m.run_until(counter,500,0,&mut frames).unwrap(),Stop::Reached);
            assert_eq!(m.time,500);
            let live=m.vcpu.get_regs().unwrap();
            assert_eq!(live.rsp,initial.rsp);assert_ne!(live.rflags&0x200,0);
            assert_eq!(live.rip,0x100000+10);
            assert_eq!(m.stats.kvm_n[1],0,"unintended zero-memory boot execution");
            assert_eq!(m.branches,0);
            let p=m.pending_io.clone().unwrap();
            assert_eq!(p.remaining_ns,500);
            assert_eq!(p.port,port);assert_eq!(p.write,write);
            assert_eq!(p.width,if port==0x530 {2} else {1});assert_eq!(p.count,1);
            let clock=m.page_get(0);let window=m.page_get(2);let exits=m.stats.exits;
            let count=counter.read_fast();
            assert_eq!(m.run_until(counter,750,0,&mut frames).unwrap(),Stop::Reached);
            assert_eq!(m.pending_io.as_ref().unwrap().remaining_ns,250);
            assert_eq!(m.pending_io.as_ref().unwrap().bytes,p.bytes);
            assert_eq!(m.page_get(0),clock);assert_eq!(m.page_get(2),window);
            assert_eq!(m.stats.exits,exits);assert_eq!(m.stats.irqs,0);
            assert_eq!(&m.memory()[0x80000..0x80004],&[0,0,0,0]);
            // Completion changes the pending architectural I/O state once,
            // while neither counted guest branches nor virtual cost advance.
            assert_eq!(m.run_until(counter,1000,0,&mut frames).unwrap(),Stop::Reached);
            assert_eq!(m.time,1000);assert_eq!(m.branches,0);
            assert_eq!(counter.read_fast(),count);
            assert!(m.pending_io.is_none());assert_eq!(m.stats.exits,exits);
            let complete=m.vcpu.get_regs().unwrap();
            assert_eq!(complete.rsp,initial.rsp);assert_ne!(complete.rflags&0x200,0);
            assert_eq!(complete.rip,0x100000+10+if port==0x530 {2} else {1});
            assert_eq!(&m.memory()[0x80000..0x80004],&[0,0,0,0]);
            if !write {assert_eq!(m.vcpu.get_regs().unwrap().rax as u8,p.bytes[0]);}
            if port==0x40 && !write {assert_eq!(m.pit.read(0x40,1000),0x12);}
            assert_eq!(m.run_until(counter,2000,0,&mut frames).unwrap(),Stop::Halted);
            assert_eq!(m.branches,1);
            assert_eq!(&m.memory()[0x80000..0x80004],&[0x78,0x56,0x34,0x12]);
    }

    #[test]
    #[ignore = "first unchanged failed case; requires exclusive leased KVM lane"]
    fn first_case_preserves_boot_and_halts() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        check_port(&kvm,&mut counter,0x40,true);
    }

    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn charged_io_completes_once_after_debt_with_no_guest_progress() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        let ports=[(0x40,true),(0x41,true),(0x42,true),(0x43,true),(0x61,true),(0x530,true),(0x40,false),(0x41,false),(0x42,false),(0x61,false)];
        for (port,write) in ports {check_port(&kvm,&mut counter,port,write);}
        let mut pending=None;
        let mut completed=None;
        for undo in [0,16,256] {
            for exact in [false,true] {
                let mut m=probe(&kvm,0x530,true,undo);
                m.pit_exact=exact;
                m.require_io_checkpoint=undo>0;
                let mut frames=Vec::new();
                assert_eq!(m.run_until(&mut counter,839,0,&mut frames).unwrap(),Stop::Reached);
                assert_eq!(m.time,839);
                assert_eq!(m.pending_io.as_ref().unwrap().remaining_ns,161);
                assert_eq!(m.pic.pending(),Some(0));
                assert_eq!(m.pit.deadline,Some(1678));
                assert_eq!(m.stats.irqs,0);
                assert!(!m.undo.as_ref().is_some_and(|u|u.valid));
                if undo>0 {assert!(m.undo.as_ref().unwrap().stats.checkpoints>0);}
                let checkpoint_count=m.undo.as_ref().map_or(0,|u|u.stats.checkpoints);
                let evidence=(m.exit_witness(),m.state_digest());
                // Same registers/RAM, but pending response/debt must change
                // both the terminal exit witness and the named state part.
                let saved=m.pending_io.clone();
                for data_change in [false,true] {
                    let p=m.pending_io.as_mut().unwrap();
                    if data_change {p.bytes[0]^=1;} else {p.remaining_ns-=1;}
                    let changed=(m.exit_witness(),m.state_digest());
                    assert_ne!(changed.0,evidence.0);
                    assert_ne!(changed.1.0,evidence.1.0);
                    for ((name,hash),(other_name,other_hash)) in evidence.1.1.iter().zip(&changed.1.1) {
                        assert_eq!(name,other_name);
                        if *name=="pending_io" {assert_ne!(hash,other_hash);} else {assert_eq!(hash,other_hash);}
                    }
                    m.pending_io=saved.clone();
                }
                if let Some(want)=&pending {assert_eq!(&evidence,want,"pending undo={undo} exact={exact}");} else {pending=Some(evidence);}
                assert_eq!(m.run_until(&mut counter,1000,0,&mut frames).unwrap(),Stop::Reached);
                let evidence=(m.exit_witness(),m.state_digest());
                if let Some(want)=&completed {assert_eq!(&evidence,want,"completed undo={undo} exact={exact}");} else {completed=Some(evidence);}
                assert_eq!(m.time,1000);assert_eq!(m.branches,0);assert!(m.pending_io.is_none());
                assert_eq!(m.undo.as_ref().map_or(0,|u|u.stats.checkpoints),checkpoint_count);
            }
        }
    }

    fn halt_snapshot(m: &mut Machine, stage: &str) {
        let regs=m.vcpu.get_regs().unwrap();
        let sregs=m.vcpu.get_sregs().unwrap();
        let (sync,if_flag,exit_reason)={let run=m.vcpu.get_kvm_run();(unsafe{run.s.regs.regs},run.if_flag,run.exit_reason)};
        let marker=m.memory()[0x80000..0x80004].to_vec();
        let page:Vec<_>=(0..PAGE_FIELDS).map(|i|m.page_get(i)).collect();
        eprintln!("{}",serde_json::json!({"stage":stage,"state":format!("{:?}",m.state),"guest_exit":m.guest_exit_code,"guest_init_error":m.guest_init_error,"arch":{"rip":regs.rip,"rflags":regs.rflags,"rax":regs.rax,"rdx":regs.rdx,"rsp":regs.rsp,"cr0":sregs.cr0,"cs_base":sregs.cs.base},"sync":{"rip":sync.rip,"rflags":sync.rflags,"rax":sync.rax,"rdx":sync.rdx},"run_if_flag":if_flag,"raw_run_exit_reason":exit_reason,"pending_io":m.pending_io.as_ref().map(|p|serde_json::json!({"remaining_ns":p.remaining_ns,"port":p.port,"write":p.write,"width":p.width,"count":p.count,"bytes":p.bytes})),"time_ns":m.time,"branches":m.branches,"page":page,"marker":marker,"exits":m.stats.exits,"halts":m.stats.halts,"irqs":m.stats.irqs,"kvm_exits":m.stats.kvm_n}));
    }

    #[test]
    #[ignore = "single manager-authorized diagnostic of unchanged halt case"]
    fn diagnose_first_halt_case() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        let mut m=probe(&kvm,0x40,true,0);
        let mut kernel=vec![0u8;1024];kernel[0x1f1]=1;kernel.extend_from_slice(&m.memory()[0x100000..0x100000+26]);
        eprintln!("fixture_kernel_sha256={:x}",Sha256::digest(&kernel));
        halt_snapshot(&mut m,"before_first_run");
        let mut frames=Vec::new();
        for (limit,stage) in [(500,"after_partial_500"),(750,"after_partial_750"),(1000,"after_completion_1000")] {
            let stop=m.run_until(&mut counter,limit,0,&mut frames);
            eprintln!("limit={limit} stop={stop:?}");
            halt_snapshot(&mut m,stage);
            assert_eq!(stop.unwrap(),Stop::Reached);
        }
        let stop=m.run_until(&mut counter,2000,0,&mut frames);
        eprintln!("limit=2000 stop={stop:?}");
        halt_snapshot(&mut m,"after_final_resume");
        assert_eq!(stop.unwrap(),Stop::Halted);
    }

}
