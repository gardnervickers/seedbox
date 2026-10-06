//! One x86-64 Linux machine: a KVM VM with one vCPU, run only by the world
//! thread. Guest progress is measured in retired conditional branches; the
//! machine can be stopped at an exact branch count (precise preemption).

use crate::counter::Counter;
use crate::devices::{rtc_read, Pic, Pit, Uart};
use crate::stepper::Planner;
use crate::virtio_net::VirtioNet;
use kvm_bindings::*;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use sha2::{Digest, Sha256};
use std::os::fd::AsRawFd;
use std::time::Instant;

pub const TIME_PAGE: u64 = 0xd000_0000;
pub const NET_MMIO: u64 = 0xd000_1000;
pub const NET_IRQ: u8 = 5;
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
/// Pending interrupts are injected only when the next stop is further away
/// than this. Part of the execution semantics: changing it changes runs.
pub const INJECT_GUARD: u64 = 65536;
/// Virtual cost of one legacy timer port access.
const ISA_IO_NS: u64 = 1000;

pub struct BootSpec<'a> {
    pub kernel: &'a [u8],
    pub initrd: &'a [u8],
    pub cmdline: String,
    pub mem: usize,
    pub rng_seed: [u8; 32],
    pub mac: Option<[u8; 6]>,
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
    tsc_latch: u64,
    pub state: State,
    pic: Pic,
    pit: Pit,
    rtc_index: u8,
    pub com1: Uart,
    pub com2: Uart,
    pub net: Option<VirtioNet>,
    stepping: bool,
    planner: Option<Planner>,
    exit_req: bool,
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
    pub stats: Stats,
    pub witness: Sha256,
    pub trace: Option<std::io::BufWriter<std::fs::File>>,
}

fn put(m: &mut [u8], at: usize, v: u64, n: usize) {
    m[at..at + n].copy_from_slice(&v.to_le_bytes()[..n]);
}

fn map(size: usize) -> Result<*mut u8, String> {
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
                flags: 0,
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
        Ok(Self {
            name: name.to_string(),
            _vm: vm,
            vcpu,
            mem,
            mem_size,
            page: page.cast(),
            time: 0,
            branches: 0,
            tsc_latch: 0,
            state: State::Running,
            pic: Pic::new(),
            pit: Pit::default(),
            rtc_index: 0,
            com1: Uart::default(),
            com2: Uart::default(),
            net,
            stepping: false,
            planner: None,
            exit_req: false,
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

    pub fn deliver_frame(&mut self, frame: &[u8]) -> bool {
        let (mem, size) = (self.mem, self.mem_size);
        let m = unsafe { std::slice::from_raw_parts_mut(mem, size) };
        let irq = self.net.as_mut().map(|n| n.receive(m, frame));
        if irq == Some(true) {
            self.pic.raise(NET_IRQ);
        }
        irq.is_some()
    }

    fn sync_from_page(&mut self) {
        let last = unsafe { std::ptr::read_volatile(self.page.add(1)) };
        if last > self.time {
            self.time = last;
        }
    }

    fn publish(&mut self) {
        unsafe { std::ptr::write_volatile(self.page, self.time) };
    }

    fn set_stepping(&mut self, on: bool) -> Result<(), String> {
        if on == self.stepping {
            return Ok(());
        }
        if on {
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
        let slice_end = limit.saturating_sub(early).max(self.time + 1).min(limit);
        let mut legal = true;
        loop {
            if legal {
                self.sync_from_page();
                while self.pit.expire(self.time) {
                    self.pic.raise(0);
                }
                if self.time >= slice_end {
                    self.set_stepping(false)?;
                    return Ok(Stop::Reached);
                }
                self.publish();
            }
            // Stop at the first deterministic point at or after `soft`, at the
            // latest by an exact counted stop at `hard`. Two rules combine: the
            // slice ends in [slice_end, limit], and a timer due at `pit` fires
            // in [pit, pit + tolerance] (exactly at `pit` if so drawn). Neither
            // passes `limit`, the world's causal boundary.
            let pit = self.pit.deadline.unwrap_or(u64::MAX);
            let pit_late = if self.pit_exact {
                pit
            } else {
                pit.saturating_add(self.tolerance)
            };
            let soft = slice_end.min(pit);
            let hard = limit.min(pit_late);
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
            // Near a soft stop, ask the guest kernel to exit at its next syscall:
            // the first such exit at or after `soft` is a deterministic stopping
            // point, much cheaper than counting our way to `hard`. Earlier yield
            // exits are transparent.
            let rem_soft = soft.saturating_sub(self.time);
            let windowed = soft < hard;
            let window = windowed && rem_soft <= self.margin;
            if window != self.exit_req {
                unsafe { std::ptr::write_volatile(self.page.add(2), window as u64) };
                self.exit_req = window;
            }
            if remaining > self.margin {
                self.set_stepping(false)?;
                let now = counter.last();
                let wake = if rem_soft > self.margin {
                    rem_soft - self.margin
                } else if self.adaptive_margin && remaining > APPROACH {
                    // Approach a hard stop in two overflows: the first measures
                    // the branch rate of the code actually running near it.
                    remaining - APPROACH
                } else {
                    remaining - self.margin
                };
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
            let c0 = counter.last();
            let t_run = Instant::now();
            let exit = self.vcpu.run();
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
            if delta > remaining || (interrupted && !self.stepping && delta == remaining) {
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
            // With the window closed, a run must not pass `soft`: a syscall
            // there would have been the stop. (Changing the margin must never
            // change where a stop lands.)
            if windowed && !window && delta > rem_soft {
                return Err(format!(
                    "soft overshoot on {}: {} branches past a soft stop before its syscall window opened",
                    self.name,
                    delta - rem_soft
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
                    let mut charge = 0;
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
                            self.com1.write(port - 0x3f8, v)
                        }
                        0x2f8..=0x2ff => self.com2.write(port - 0x2f8, v),
                        0x20 | 0x21 | 0xa0 | 0xa1 => self.pic.write(port, v),
                        0x40..=0x43 | 0x61 => {
                            self.pit.write(port, v, now);
                            charge = ISA_IO_NS;
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
                        0xe9 => self.com1.out.push(v),
                        0xf4 => {
                            self.state = State::Exited(v);
                            self.record(b'X', v as u64);
                            self.set_stepping(false)?;
                            return Ok(Stop::Exited);
                        }
                        _ => {}
                    }
                    self.record(b'O', (port as u64) << 8 | v as u64);
                    self.serial_edges();
                    self.time += charge;
                }
                Ok(VcpuExit::IoIn(port, data)) => {
                    self.stats.pio += 1;
                    let mut charge = 0;
                    match port {
                        0x520 => {
                            let t = self.time.max(self.tsc_latch + 1);
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
                                0x40..=0x42 | 0x61 => {
                                    charge = ISA_IO_NS;
                                    self.pit.read(port, now)
                                }
                                0x71 => rtc_read(self.rtc_index),
                                _ => 0xff,
                            };
                            data.fill(v);
                        }
                    }
                    self.record(b'i', port as u64);
                    self.serial_edges();
                    self.time += charge;
                }
                Ok(VcpuExit::MmioRead(addr, data)) => {
                    self.stats.mmio += 1;
                    let v = match self.net.as_mut() {
                        Some(n) if (NET_MMIO..NET_MMIO + 0x1000).contains(&addr) => {
                            n.read(addr - NET_MMIO)
                        }
                        _ => 0,
                    };
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
                    if (NET_MMIO..NET_MMIO + 0x1000).contains(&addr) {
                        let (mem, size) = (self.mem, self.mem_size);
                        let m = unsafe { std::slice::from_raw_parts_mut(mem, size) };
                        if let Some(n) = self.net.as_mut() {
                            let out = n.write(m, addr - NET_MMIO, v);
                            if out.irq {
                                self.pic.raise(NET_IRQ);
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
    pub fn state_digest(&mut self) -> String {
        let r = self.vcpu.get_regs().unwrap();
        let s = self.vcpu.get_sregs().unwrap();
        let mut h = Sha256::new();
        for v in [
            r.rax, r.rbx, r.rcx, r.rdx, r.rsi, r.rdi, r.rsp, r.rbp, r.r8, r.r9, r.r10, r.r11,
            r.r12, r.r13, r.r14, r.r15, r.rip, r.rflags, s.cr0, s.cr3, s.cr4, s.efer,
        ] {
            h.update(v.to_le_bytes());
        }
        h.update(self.memory());
        format!("{:x}", h.finalize())
    }
}
