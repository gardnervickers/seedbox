//! One x86-64 Linux machine: a KVM VM with one vCPU, run only by the world
//! thread. Its timebase (see `timebase.rs`) says how guest progress becomes
//! virtual time: retired conditional branches with exact counted stops
//! (precise preemption), or the PV clock, where time moves only at exits.

use crate::boundary::{advance_time, hard_limit, io_charge, PendingIo};
use crate::timebase::{Judge, Prepared, Run, Timebase, Verdict};
#[cfg(test)]
use crate::counter::Counter;
use crate::profile::CounterProfile;
use crate::virtio_console::VirtioConsole;
use crate::devices::{rtc_read, Pic, Pit, Uart};
use crate::stepper::Planner;
use crate::undo::{Undo, UndoStats, VcpuState};
use crate::virtio_net::{ReceiveOutcome, VirtioNet};
use crate::virtio_blk::VirtioBlk;
use crate::disk::Disk;
use kvm_bindings::*;
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use sha2::{Digest, Sha256};
use std::os::fd::AsRawFd;
use std::time::Instant;

mod counted;
pub mod pv;
mod pv_plan;

pub const TIME_PAGE: u64 = 0xd000_0000;
pub const NET_MMIO: u64 = 0xd000_1000;
pub const NET_IRQ: u8 = 5;
pub const CHANNEL_MMIO: u64 = 0xd1000000;
pub const CHANNEL_IRQ: u8 = 6;
pub const BLK_MMIO: u64 = 0xd000_2000;
pub const BLK_IRQ: u8 = 7;
const INITRD_ADDR: usize = 0x0800_0000;
/// Pending interrupts are injected only when the next stop is further away
/// than this. Part of the execution semantics: changing it changes runs.
pub const INJECT_GUARD: u64 = 65536;
/// Time page fields (u64 index), see kernel/seedbox.h.
const PAGE_SYSCALLS: usize = 3;
const PAGE_PIC_IMR: usize = 4;
const PAGE_NET_ISR: usize = 5;
pub const PAGE_FIELDS: usize = 6;
/// Fork entropy (ABI 3): written by the VMM only while the world is parked,
/// so undo checkpoints need not carry them.
const PAGE_ENTROPY_GEN: usize = 9;
const PAGE_ENTROPY: usize = 10;
const PAGE_ENTROPY_FIELDS: usize = 4;
/// Guest ABI from which the kernel reseeds its CRNG from the page.
pub const ABI_PAGE_RESEED: u64 = 3;
/// PV clock fields, written by the VMM and outside `PAGE_FIELDS` (they are
/// zero in the counter timebase, whose checkpoints and digests skip them).
const PAGE_PV_CLOCK: usize = 6;
const PAGE_BOUND: usize = 7;
const PAGE_SYSCALL_TARGET: usize = 8;
const PAGE_TIMER_DEADLINE: usize = 14;
const PAGE_CAPABILITIES: usize = 15;
// Rewritten at every plan, outside the checkpointed PAGE_FIELDS.
const PAGE_SYSCALL_NS: usize = 16;
const PAGE_SYSCALLS0: usize = 17;
const PAGE_PLAN_TIME: usize = 18;
pub const ABI_PAGE_TIMER: u64 = 4;
const CAP_PAGE_TIMER: u64 = 1;
const TIMER_DISARMED: u64 = u64::MAX;

/// The last adopted page store. Kept separately so expiration cannot rearm
/// an unchanged word, and legacy PIT writes can still replace a page arm.
#[derive(Clone, Copy)]
struct PageTimer {
    enabled: bool,
    seen: u64,
}

impl PageTimer {
    fn new() -> Self { Self { enabled: false, seen: TIMER_DISARMED } }

    fn pickup(&mut self, word: u64, pit: &mut Pit, now: u64) -> Result<bool, String> {
        if self.enabled && word != self.seen {
            // Like the PV timer port: replacing an overdue arm cannot lose IRQ0.
            let fired = pit.expire_through(now)? > 0;
            self.seen = word;
            pit.program_deadline((word != TIMER_DISARMED).then_some(word));
            Ok(fired)
        } else {
            Ok(false)
        }
    }
}

/// Both timer transports use exactly the same latency stream and draw order.
fn timer_latency(pit: &mut Pit, exact: &mut bool, rng: Option<&mut crate::rng::Rng>, jitter: u64, ppm: u64) {
    if pit.fresh {
        pit.fresh = false;
        if let (Some(d), Some(rng)) = (pit.deadline, rng) {
            if jitter > 0 { pit.deadline = Some(d + rng.below(jitter + 1)); }
            *exact = ppm > 0 && rng.below(1_000_000) < ppm;
        }
    }
}

// Pin every extension word without extending the checkpoint/digest prefix.
const _: () = {
    assert!(PAGE_FIELDS == 6);
    assert!(PAGE_PV_CLOCK == 6);
    assert!(PAGE_BOUND == 7);
    assert!(PAGE_SYSCALL_TARGET == 8);
    assert!(PAGE_ENTROPY_GEN == 9);
    assert!(PAGE_ENTROPY == 10);
    assert!(PAGE_ENTROPY + 1 == 11);
    assert!(PAGE_ENTROPY + 2 == 12);
    assert!(PAGE_ENTROPY + 3 == 13);
    assert!(PAGE_ENTROPY_FIELDS == 4);
    assert!(ABI_PAGE_RESEED == 3);
    assert!(PAGE_TIMER_DEADLINE == 14);
    assert!(PAGE_CAPABILITIES == 15);
    assert!(PAGE_SYSCALL_NS == 16);
    assert!(PAGE_SYSCALLS0 == 17);
    assert!(PAGE_PLAN_TIME == 18);
    assert!(ABI_PAGE_TIMER == 4);
    assert!(CAP_PAGE_TIMER == 1);
};

pub struct BootSpec<'a> {
    pub kernel: &'a [u8],
    pub initrd: &'a [u8],
    pub cmdline: String,
    pub mem: usize,
    pub rng_seed: [u8; 32],
    pub mac: Option<[u8; 6]>,
    pub control_channel: bool,
    /// The retained disk this incarnation boots with, as virtio-blk.
    pub disk: Option<Disk>,
    /// Final overflow margin for undo-able precise stops; 0 disables them.
    pub undo_margin: u64,
    /// The guest parks at its launch barrier until the world releases it.
    pub barrier: bool,
    /// The machine runs under the PV clock timebase (no host perf counter).
    pub pv_clock: bool,
    pub page_timer: bool,
}

/// Port of the launch barrier, see guest/barrier.h.
const BARRIER_PORT: u16 = 0xf8;
pub const ENTROPY_WORDS: usize = 8;

/// Everything about one machine that a world fork puts back: vCPU, RAM, the
/// shared page, device models and the VMM's per-machine scalars.
pub struct MachineSnapshot {
    ram: Mapping,
    pending_io: Option<PendingIo>,
    timer_rng: Option<crate::rng::Rng>,
    paused: bool,
    down: bool,
    incarnation: u32,
    booted_at: u64,
    entropy: [u32; ENTROPY_WORDS],
    entropy_read: usize,
    page: Box<[u8; 4096]>,
    cpu: VcpuState,
    time: u64,
    branches: u64,
    tsc_latch: u64,
    state: State,
    guest_exit_code: Option<u8>,
    guest_init_error: Option<u8>,
    pic: Pic,
    pit: Pit,
    rtc_index: u8,
    com1: Uart,
    com2: Uart,
    log_order: u64,
    observe_console: bool,
    management_framer: crate::control::ManagementFramer,
    net: Option<VirtioNet>,
    channel: Option<VirtioConsole>,
    blk: Option<VirtioBlk>,
    control_dma_bytes: u64,
    control_tx_bytes: u64,
    pv: bool,
    syscall_ns: u64,
    abi: u64,
    page_timer: PageTimer,
    armed_wake: u64,
    pit_exact: bool,
    exit_req: bool,
    margin: u64,
    fm: u64,
    parked: bool,
    stats: Stats,
    witness: Sha256,
    undo_stats: Option<UndoStats>,
}

/// How a released guest receives its seed's entropy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reseed {
    /// The guest reads it at its launch barrier (guest/barrier.h).
    Barrier,
    /// Time fork: the kernel takes it from the time page (ABI 3).
    Page,
    /// Time fork on a kernel before ABI 3: the guest CRNG is not reseeded.
    Unsupported,
}

impl Reseed {
    pub fn as_str(self) -> &'static str {
        match self {
            Reseed::Barrier => "barrier",
            Reseed::Page => "time_page",
            Reseed::Unsupported => "unsupported",
        }
    }
}

/// The time page's entropy fields: the seed's 32 bytes, little-endian.
fn page_entropy(entropy: &[u32; ENTROPY_WORDS]) -> [u64; PAGE_ENTROPY_FIELDS] {
    std::array::from_fn(|i| entropy[2 * i] as u64 | (entropy[2 * i + 1] as u64) << 32)
}

/// An anonymous private mapping, unmapped on drop.
struct Mapping {
    ptr: *mut u8,
    size: usize,
}
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.size) };
    }
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
    /// The guest reached its launch barrier and is parked until released.
    Barrier,
    /// A committed management request/effect requires global causal replanning.
    Control,
}

#[derive(Default, Debug, Clone)]
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
    /// PV clock: watchdog checks of a run with no exit, those that proved a
    /// spin (a state cycle, then treated as idle), and the rest.
    pub spin_probes: u64,
    pub spin_idles: u64,
    pub spin_false: u64,
    /// Of the false alarms, probes abandoned as soon as the guest clock moved.
    pub spin_clock_progress: u64,
    /// Per state part, cycle checks that failed first on it.
    pub spin_mismatch: std::collections::BTreeMap<String, u64>,
    /// Virtual time advanced, by cause (see `Cause`).
    pub time_by: [u64; 5],
    /// Report-only timer diagnostics. Writes to the PIT or port 0x530, and
    /// of those, writes that found the old deadline already due (with how
    /// far past due, and the syscalls charged in that run).
    pub timer_writes: u64,
    pub timer_overdue: u64,
    pub timer_overdue_max_ns: u64,
    pub timer_write_syscalls_max: u64,
    /// Guest clock (`page.last`) against VMM time at timer writes, sends
    /// and halts (`Site`).
    pub skew: [Skew; 3],
}

/// Where `Stats::skew` was sampled.
pub enum Site {
    Timer,
    Sent,
    Halt,
}

impl Site {
    pub const NAMES: [&'static str; 3] = ["timer_write", "sent", "halt"];
}

/// Guest clock minus VMM time: how often and how far ahead or behind.
#[derive(Default, Debug, Clone)]
pub struct Skew {
    pub n: u64,
    pub ahead: u64,
    pub ahead_max_ns: u64,
    pub behind: u64,
    pub behind_max_ns: u64,
}

/// Why virtual time moved, for `Stats::time_by`.
#[derive(Clone, Copy)]
pub enum Cause {
    /// The timebase's progress: branches, or charged syscalls (PV).
    Progress,
    /// Guest clock reads (time page or port).
    ClockReads,
    /// The guest waited at a PV clock bound.
    ClockBound,
    /// I/O cost paid.
    Io,
    /// Halted, parked or waiting for the world.
    Idle,
}

impl Cause {
    pub const NAMES: [&'static str; 5] = ["progress", "clock_reads", "clock_bound", "io", "idle"];
}

pub struct Machine {
    pub name: String,
    profile: &'static CounterProfile,
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
    log_order: u64,
    pub fault_effects: Vec<(u32, u64, bool)>,
    pub control_requests: Vec<(u64, Vec<u8>)>,
    pub observe_console: bool,
    pub committed_console: Option<(u64,u8)>,
    management_framer: crate::control::ManagementFramer,
    pub net: Option<VirtioNet>,
    pub channel: Option<VirtioConsole>,
    pub blk: Option<VirtioBlk>,
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
    /// Guest ABI version announced on the time page port (0 before then).
    abi: u64,
    page_timer: PageTimer,
    armed_wake: u64,
    /// The pending timer deadline must be delivered at its exact time.
    pit_exact: bool,
    pub exact_timer_ppm: u64,
    /// Last breakpoint plan, for overshoot diagnostics.
    last_plan: (u64, Vec<u64>),
    pub tolerance: u64,
    pub timer_jitter: u64,
    /// PV clock: virtual time one syscall costs on this machine.
    pub syscall_ns: u64,
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
    /// Parked at the launch barrier: the world does not run this machine.
    pub parked: bool,
    /// Paused by a lifecycle fault: CPU and devices are frozen, any unpaid
    /// I/O cost stays owed, and the world does not run this machine.
    pub paused: bool,
    /// Built for a reboot that has not happened yet: the world does not run
    /// this machine. Its predecessor's VM is already gone.
    pub down: bool,
    /// Boots of this machine before this one: 0 for the first.
    pub incarnation: u32,
    /// Virtual time at which this incarnation started executing.
    pub booted_at: u64,
    barrier: bool,
    /// Per-seed words the guest reads after its release.
    entropy: [u32; ENTROPY_WORDS],
    entropy_read: usize,
    pv_clock: bool,
    page_timer_requested: bool,
    /// The MSR list a fork snapshot saves.
    msrs: Msrs,
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
        // The PV clock needs no counter, so no qualified host CPU; the
        // profile is then never consulted.
        let profile = if spec.pv_clock { &crate::profile::ZEN } else { crate::profile::host()? };
        if !kvm.check_extension(kvm_ioctls::Cap::ImmediateExit) {
            return Err("KVM immediate-exit capability required for I/O completion".into());
        }
        let mem_size = spec.mem;
        let mem = map(mem_size)?;
        let page = map(4096)?;
        if spec.pv_clock {
            unsafe { std::ptr::write_volatile(page.cast::<u64>().add(PAGE_PV_CLOCK), 1) };
        }
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
        let msrs = crate::undo::msr_list(kvm, &vcpu)?;
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
            profile,
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
            log_order: 0,
            fault_effects: Vec::new(),
            control_requests: Vec::new(),
            observe_console:false, committed_console:None,
            management_framer: crate::control::ManagementFramer::default(),
            net,
            channel: spec.control_channel.then(VirtioConsole::new),
            blk: spec.disk.clone().map(VirtioBlk::new),
            control_dma_bytes:0,
            control_tx_bytes:0,
            stepping: false,
            sync_regs_ready: false,
            #[cfg(test)]
            require_io_checkpoint: false,
            planner: None,
            exit_req: false,
            pv: false,
            abi: 0,
            page_timer: PageTimer::new(),
            armed_wake: 0,
            pit_exact: false,
            exact_timer_ppm: 0,
            last_plan: (0, Vec::new()),
            tolerance: 0,
            timer_jitter: 0,
            syscall_ns: pv::SYSCALL_NS,
            timer_rng: None,
            margin: profile.margin,
            adaptive_margin: false,
            step_started: None,
            undo,
            undo_margin: spec.undo_margin,
            fm: spec.undo_margin,
            parked: false,
            paused: false,
            down: false,
            incarnation: 0,
            booted_at: 0,
            barrier: spec.barrier,
            entropy: [0; ENTROPY_WORDS],
            entropy_read: 0,
            pv_clock: spec.pv_clock,
            page_timer_requested: spec.page_timer,
            msrs,
            stats: Stats::default(),
            // A PV clock run can never share a witness with a counter run.
            witness: if spec.pv_clock { Sha256::new_with_prefix(b"timebase=pv") } else { Sha256::new() },
            trace: None,
        })
    }

    /// Guest ABI announced on the time page port, 0 before the kernel has.
    pub fn abi(&self) -> u64 {
        self.abi
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

    /// Take the machine off its launch barrier at virtual time `at`, with the
    /// seed's entropy for the guest to read. A guest parked at a simulated
    /// time never reads the barrier port, so on ABI 3 the entropy goes in the
    /// time page instead and the kernel reseeds its CRNG before its next
    /// random output. Returns how the guest receives it.
    pub fn release(&mut self, at: u64, entropy: [u32; ENTROPY_WORDS]) -> Result<Reseed, String> {
        if !self.parked {
            return Err(format!("{}: released without reaching the launch barrier", self.name));
        }
        self.parked = false;
        // The parked interval passes like idle time: a timer that fell due
        // meanwhile fires now, late, so the armed deadline is ahead of the
        // clock again before the next slice computes its hard stop.
        self.advance_to(at, Cause::Idle);
        self.expire_timers()?;
        self.entropy = entropy;
        self.entropy_read = 0;
        self.record(b'R', self.time);
        if self.barrier {
            return Ok(Reseed::Barrier);
        }
        if self.abi < ABI_PAGE_RESEED {
            return Ok(Reseed::Unsupported);
        }
        self.reseed_time_page(entropy)?;
        Ok(Reseed::Page)
    }

    /// Publish fork entropy without parking, advancing clocks or touching I/O.
    /// ABI zero is pre-boot: retain the page until the kernel announces its ABI.
    pub fn reseed_time_page(&mut self, entropy: [u32; ENTROPY_WORDS]) -> Result<(), String> {
        if self.abi != 0 && self.abi < ABI_PAGE_RESEED {
            return Err(format!("{}: runtime checkpoint reseeding requires guest ABI 3; rebuild the kernel from kernel/", self.name));
        }
        self.entropy = entropy;
        self.entropy_read = 0;
        let gen = self.page_get(PAGE_ENTROPY_GEN) + 1;
        for (i, v) in page_entropy(&entropy).into_iter().enumerate() {
            self.page_set(PAGE_ENTROPY + i, v);
        }
        // Publish the generation last: the guest reads it first.
        self.page_set(PAGE_ENTROPY_GEN, gen);
        self.record(b'S', gen);
        Ok(())
    }

    #[cfg(test)]
    pub fn published_entropy(&self) -> [u64;5] {
        std::array::from_fn(|i|self.page_get(PAGE_ENTROPY_GEN+i))
    }

    /// Freeze the machine at a world boundary. Nothing about the vCPU changes:
    /// an exit KVM still has to complete, and the I/O cost owed for it, stay
    /// exactly as they are until the machine resumes.
    pub fn pause(&mut self) -> Result<(), String> {
        if self.paused || self.down || self.parked {
            return Err(format!("{}: cannot pause here", self.name));
        }
        self.paused = true;
        self.record(b'Z', 0);
        Ok(())
    }

    /// Continue a paused machine at virtual time `at`. The paused interval
    /// passes like idle time, as at a release: timers that fell due fire now,
    /// late and coalesced, keeping their phase.
    pub fn resume(&mut self, at: u64) -> Result<(), String> {
        if !self.paused {
            return Err(format!("{}: resumed without being paused", self.name));
        }
        self.paused = false;
        self.time = self.time.max(at);
        self.expire_timers()?;
        self.record(b'U', self.time);
        Ok(())
    }

    /// Take a machine built for a reboot to its first instruction at `at`.
    pub fn boot_at(&mut self, at: u64) -> Result<(), String> {
        if !self.down || at < self.time {
            return Err(format!("{}: cannot boot here", self.name));
        }
        self.down = false;
        self.time = at;
        self.booted_at = at;
        self.record(b'N', self.incarnation as u64);
        Ok(())
    }

    /// Carry what outlives a crash from the previous incarnation into this
    /// one, which is built but not booted: the console and management logs
    /// and their sinks, the trace, and the event witness, so the final
    /// witness covers every incarnation. Nothing else crosses: RAM, device
    /// queues, KVM's I/O state and the undo checkpoint die with the VM.
    /// Returns the number of volatile sectors lost, or None without a disk.
    pub fn succeed(&mut self, old: &mut Machine, at: u64, seed: u64, identity: Option<&str>) -> Result<Option<u64>, String> {
        self.incarnation = old.incarnation.checked_add(1).ok_or("incarnation overflow")?;
        self.down = true;
        self.time = at;
        for (new, old) in [(&mut self.com1, &mut old.com1), (&mut self.com2, &mut old.com2)] {
            new.out = std::mem::take(&mut old.out);
            new.output_file = old.output_file.take();
            new.output_limit = old.output_limit;
            new.chunks = std::mem::take(&mut old.chunks);
        }
        self.log_order = old.log_order;
        self.trace = old.trace.take();
        // The same machine: under the PV clock it keeps its speed, or members
        // restarted together would run in lockstep again.
        self.syscall_ns = old.syscall_ns;
        // Flushed data survives; the disk policy chooses unflushed survivors.
        let lost = if let (Some(new), Some(blk)) = (self.blk.as_mut(), old.blk.as_ref()) {
            let (disk, lost) = blk.disk.crashed_branch(seed, identity, &old.name, old.incarnation);
            *new = VirtioBlk::new(disk);
            Some(lost)
        } else { None };
        self.witness.update(old.exit_witness().as_bytes());
        self.record(b'C', at);
        Ok(lost)
    }

    /// Park the machine at a fork point the world chose (a simulated time),
    /// rather than one the guest announced. It must owe no I/O cost.
    pub fn park(&mut self) -> Result<(), String> {
        if self.parked || self.pending_io.is_some() {
            return Err(format!("{}: cannot park here", self.name));
        }
        self.set_stepping(false)?;
        // As at the launch barrier: finish any exit KVM still owes, so a cold
        // run and a restored fork resume from identical vCPU state.
        self.complete_io()?;
        self.parked = true;
        self.record(b'P', 0);
        Ok(())
    }

    /// Capture at a committed scheduler boundary, including unpaid I/O cost.
    pub fn snapshot(&mut self) -> Result<MachineSnapshot, String> {
        if self.stepping || !self.fault_effects.is_empty() || !self.control_requests.is_empty() || self.committed_console.is_some() {
            return Err(format!("{}: not at a fork point", self.name));
        }
        self.complete_io()?;
        let mut cpu = VcpuState::new(self.msrs.clone());
        cpu.save(&self.vcpu, "fork")?;
        let undo_stats = self.undo.as_ref().map(|u| u.stats.clone());
        let ram = Mapping { ptr: map(self.mem_size)?, size: self.mem_size };
        unsafe { std::ptr::copy_nonoverlapping(self.mem, ram.ptr, self.mem_size) };
        if let Some(u) = self.undo.as_mut() {
            u.fork_point(&self._vm, self.mem)?;
        }
        let mut page = Box::new([0u8; 4096]);
        unsafe { std::ptr::copy_nonoverlapping(self.page as *const u8, page.as_mut_ptr(), 4096) };
        Ok(MachineSnapshot {
            ram, page, cpu,
            pending_io: self.pending_io.clone(), timer_rng: self.timer_rng.clone(),
            paused: self.paused, down: self.down, incarnation: self.incarnation, booted_at: self.booted_at,
            entropy: self.entropy, entropy_read: self.entropy_read,
            time: self.time, branches: self.branches, tsc_latch: self.tsc_latch, state: self.state,
            guest_exit_code: self.guest_exit_code, guest_init_error: self.guest_init_error,
            pic: self.pic.clone(), pit: self.pit.clone(), rtc_index: self.rtc_index,
            com1: self.com1.snapshot(), com2: self.com2.snapshot(), log_order: self.log_order,
            observe_console:self.observe_console,
            management_framer: self.management_framer.clone(),
            net: self.net.clone(), channel: self.channel.clone(), blk: self.blk.clone(),
            control_dma_bytes: self.control_dma_bytes, control_tx_bytes: self.control_tx_bytes,
            pv: self.pv, syscall_ns: self.syscall_ns, abi: self.abi, page_timer: self.page_timer, armed_wake: self.armed_wake, pit_exact: self.pit_exact, exit_req: self.exit_req,
            margin: self.margin, fm: self.fm, parked: self.parked,
            stats: self.stats.clone(), witness: self.witness.clone(), undo_stats,
        })
    }

    /// Put the machine back to a snapshot taken by `snapshot`, from wherever
    /// the last run left it. Returns the number of RAM pages copied back.
    pub fn restore(&mut self, s: &MachineSnapshot) -> Result<u64, String> {
        // The last exit may still owe KVM its completion (an exit port write,
        // say); complete it so the vCPU accepts new state. Its effect is
        // overwritten below, and a vCPU that cannot run any more (shutdown)
        // is overwritten just the same.
        let _ = self.complete_io();
        self.set_stepping(false)?;
        let pages = match self.undo.as_mut() {
            // A machine rebuilt for this restore (the run rebooted it) has
            // held no fork point yet: copy everything, then hold one here.
            Some(u) if !u.holds_fork_point() => {
                unsafe { std::ptr::copy_nonoverlapping(s.ram.ptr, self.mem, self.mem_size) };
                u.mark(0, self.mem_size);
                u.fork_point(&self._vm, self.mem)?;
                u.stats = s.undo_stats.clone().unwrap_or_default();
                (self.mem_size / 4096) as u64
            }
            Some(u) => {
                let pages = u.return_to_fork(&self._vm, self.mem, s.ram.ptr)?;
                // Each seed reports its own undo work on top of the boot's.
                u.stats = s.undo_stats.clone().unwrap_or_default();
                pages
            }
            None => {
                unsafe { std::ptr::copy_nonoverlapping(s.ram.ptr, self.mem, self.mem_size) };
                (self.mem_size / 4096) as u64
            }
        };
        unsafe { std::ptr::copy_nonoverlapping(s.page.as_ptr(), self.page as *mut u8, 4096) };
        let (regs, sregs) = s.cpu.restore(&self.vcpu, "fork restore")?;
        // Refresh KVM's exit outputs (interrupt flag, injection readiness and
        // the synchronized registers) from the restored state, exactly as the
        // completion at the barrier did when the snapshot was taken.
        self.complete_io()?;
        let run = self.vcpu.get_kvm_run();
        run.request_interrupt_window = 0;
        run.s.regs.regs = regs;
        run.s.regs.sregs = sregs;
        self.sync_regs_ready = true;
        self.time = s.time;
        self.branches = s.branches;
        self.tsc_latch = s.tsc_latch;
        self.state = s.state;
        self.pending_io = s.pending_io.clone();
        self.timer_rng = s.timer_rng.clone();
        self.guest_exit_code = s.guest_exit_code;
        self.guest_init_error = s.guest_init_error;
        self.pic = s.pic.clone();
        self.pit = s.pit.clone();
        self.rtc_index = s.rtc_index;
        self.com1.restore(&s.com1);
        self.com2.restore(&s.com2);
        self.log_order = s.log_order;
        self.fault_effects.clear();
        self.control_requests.clear();
        self.observe_console=s.observe_console;self.committed_console=None;
        self.management_framer = s.management_framer.clone();
        self.net = s.net.clone();
        self.channel = s.channel.clone();
        self.blk = s.blk.clone();
        self.control_dma_bytes = s.control_dma_bytes;
        self.control_tx_bytes = s.control_tx_bytes;
        self.pv = s.pv;
        self.syscall_ns = s.syscall_ns;
        self.abi = s.abi;
        self.page_timer = s.page_timer;
        self.armed_wake = s.armed_wake;
        self.pit_exact = s.pit_exact;
        self.exit_req = s.exit_req;
        self.margin = s.margin;
        self.fm = s.fm;
        self.parked = s.parked;
        self.paused = s.paused;
        self.down = s.down;
        self.incarnation = s.incarnation;
        self.booted_at = s.booted_at;
        self.entropy = s.entropy;
        self.entropy_read = s.entropy_read;
        self.stats = s.stats.clone();
        self.witness = s.witness.clone();
        Ok(pages)
    }

    /// Resume a halted machine at virtual time `t`.
    pub fn wake(&mut self, t: u64) {
        self.advance_to(t, Cause::Idle);
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
    fn capture_output(&mut self) {
        for uart in [&mut self.com1,&mut self.com2] {
            uart.chunks.capture(uart.out.len() as u64,self.time,self.incarnation,&mut self.log_order);
        }
    }
    fn channel_output(&mut self,bytes:&[u8])->Result<bool,String> {
        if self.com2.output_limit.is_some_and(|limit|self.com2.out.len().checked_add(bytes.len()).is_none_or(|n|n>limit)){return Err("guest control output limit exceeded".into())}
        if let Some(file)=self.com2.output_file.as_mut() {use std::io::Write;file.write_all(bytes).map_err(|e|format!("control log: {e}"))?;}
        self.com2.out.extend_from_slice(bytes);self.control_tx_bytes+=bytes.len() as u64;
        self.capture_output();
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

    fn poll_net(&mut self, frames: &mut Vec<Vec<u8>>, boundary: bool) -> bool {
        let m = unsafe { std::slice::from_raw_parts_mut(self.mem, self.mem_size) };
        let Some(n) = self.net.as_mut() else { return false; };
        let out = n.poll_at(m, self.time, boundary);
        self.take_dma();
        if out.irq { self.raise_net(); }
        let sent = !out.frames.is_empty();
        frames.extend(out.frames);
        sent
    }

    /// Syscalls the guest has entered (counted in the time page).
    /// Move virtual time forward to `t` (never back) for `cause`.
    fn advance_to(&mut self, t: u64, cause: Cause) {
        if t > self.time {
            self.stats.time_by[cause as usize] += t - self.time;
            self.time = t;
        }
    }

    pub fn syscalls(&self) -> u64 {
        self.page_get(PAGE_SYSCALLS)
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
        if let Some(blk) = self.blk.as_mut() {
            match self.undo.as_mut() {
                Some(u) => {
                    for (a, l) in blk.dirty.drain(..) {
                        u.mark(a, l);
                    }
                }
                None => blk.dirty.clear(),
            }
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

    fn note_skew(&mut self, site: Site) {
        let last = self.page_get(1);
        let k = &mut self.stats.skew[site as usize];
        k.n += 1;
        if last > self.time {
            k.ahead += 1;
            k.ahead_max_ns = k.ahead_max_ns.max(last - self.time);
        } else if last < self.time {
            k.behind += 1;
            k.behind_max_ns = k.behind_max_ns.max(self.time - last);
        }
    }

    fn enable_page_timer(&mut self) {
        if self.page_timer_requested && self.abi >= ABI_PAGE_TIMER {
            self.page_set(PAGE_TIMER_DEADLINE, TIMER_DISARMED);
            self.page_set(PAGE_CAPABILITIES, CAP_PAGE_TIMER);
            self.page_timer.enabled = true;
        }
    }

    // Adoption is an observable effect: call only at a legal exit or a world
    // boundary, after accounting for that point's guest clock progress. Never
    // call from a timebase prepare/plan or a watchdog/probe landing.
    fn sync_page_timer(&mut self) -> Result<(), String> {
        if !self.page_timer.enabled { return Ok(()); }
        if self.page_timer.pickup(self.page_get(PAGE_TIMER_DEADLINE), &mut self.pit, self.time)? {
            self.pic.raise(0);
        }
        timer_latency(&mut self.pit, &mut self.pit_exact, self.timer_rng.as_mut(), self.timer_jitter, self.exact_timer_ppm);
        Ok(())
    }

    // Expiration consults adopted device state only, never a guest page store.
    fn expire_timers(&mut self) -> Result<(), String> {
        if self.pit.expire_through(self.time)? > 0 {
            self.pic.raise(0);
        }
        Ok(())
    }

    /// Device effects and the KVM response have already been committed once.
    /// No guest execution, publication, injection or speculative checkpoint until
    /// all cost is paid. A world snapshot may complete KVM I/O and retain the debt.
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
            let t = pending.advance(self.time, limit, self.pit.deadline)?;
            self.advance_to(t, Cause::Io);
            self.expire_timers()?;
        }
        Ok(true)
    }

    fn checkpoint(&mut self) -> Result<(), String> {
        let t = Instant::now();
        self.complete_io()?;
        let page: [u64; PAGE_FIELDS] = std::array::from_fn(|i| self.page_get(i));
        let timer_deadline = self.page_timer.enabled.then(|| self.page_get(PAGE_TIMER_DEADLINE));
        let u = self.undo.as_mut().unwrap();
        u.timer_deadline = timer_deadline;
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
        if let Some(deadline) = u.timer_deadline {
            unsafe { std::ptr::write_volatile(self.page.add(PAGE_TIMER_DEADLINE), deadline) };
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
        let t = self.checked_page_time(limit)?;
        self.advance_to(t, Cause::ClockReads);
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

    /// Arm breakpoints for one step: the next landing point the planner
    /// finds from the current registers. Returns whether that landing is
    /// only a cap on straight-line code, so depends on where stepping began.
    fn arm_step(&mut self) -> Result<bool, String> {
        self.set_stepping(true)?;
        let (r, sr) = {
            let run = self.vcpu.get_kvm_run();
            unsafe { (run.s.regs.regs, run.s.regs.sregs) }
        };
        let mem = unsafe { std::slice::from_raw_parts(self.mem, self.mem_size) };
        let planner = self.planner.as_mut().unwrap();
        planner.refresh_from(&sr);
        let (targets, capped) = planner
            .plan_capped(mem, &r)
            .map_err(|e| format!("{}: step plan: {e}", self.name))?;
        Planner::arm(&self.vcpu, &targets)?;
        self.last_plan = (r.rip, targets.clone());
        if targets.contains(&r.rip) {
            let mut resumed = r;
            resumed.rflags |= 1 << 16; // RF: do not re-trigger at the current instruction
            self.vcpu.set_regs(&resumed).map_err(|e| e.to_string())?;
        }
        Ok(capped)
    }

    /// The guest is in a state cycle (see `pv.rs`), at its canonical point:
    /// idle, like HLT, until an interrupt.
    fn spin_idle(&mut self) -> Result<Stop, String> {
        let (rip, if_flag) = {
            let run = self.vcpu.get_kvm_run();
            (unsafe { run.s.regs.regs.rip }, run.if_flag)
        };
        self.set_stepping(false)?;
        self.stats.spin_idles += 1;
        self.record(b'S', rip);
        if if_flag == 0 {
            return Err(format!(
                "{}: the guest spins with interrupts off at rip {rip:#x}; nothing can end the loop",
                self.name
            ));
        }
        self.state = State::Halted;
        Ok(Stop::Halted)
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
    pub fn run_until<T: Timebase>(
        &mut self,
        tb: &mut T,
        limit: u64,
        early: u64,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop, String> {
        let started = Instant::now();
        let r = self.run_inner(tb, limit, early, frames);
        self.capture_output();
        self.com1.chunks.finish();self.com2.chunks.finish();
        self.stats.run_ns += started.elapsed().as_nanos();
        r
    }

    fn run_inner<T: Timebase>(
        &mut self,
        tb: &mut T,
        limit: u64,
        early: u64,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop, String> {
        // Invariant: entered at a deterministic point. Guest-visible state (time
        // page, interrupts, timer expiry) changes only at deterministic points:
        // natural exits and exact counted stops, never
        // at overflow landings or intermediate single steps.
        let deferred = self.net.as_ref().is_some_and(|n| n.deferred_tx);
        let poll_end = crate::virtio_net::poll_limit(self.time, limit, deferred);
        // A new polling deadline is exact. The world's early-stop allowance
        // can exceed TX_POLL_NS; subtracting it here would create 1 ns slices.
        let early = if poll_end < limit { 0 } else { early };
        let limit = poll_end;
        let slice_end = limit
            .saturating_sub(early)
            .max(self.time.saturating_add(1))
            .min(limit);
        let mut legal = true;
        loop {
            if self.pending_io.is_some() {
                if !self.drain_io(limit)? {
                    self.set_stepping(false)?;
                    // Charged I/O can split at an exact slice boundary too.
                    if self.poll_net(frames, true) {
                        self.note_skew(Site::Sent);
                        return Ok(Stop::Sent);
                    }
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
                if self.page_timer.enabled { self.expire_timers()?; }
                self.sync_from_page(hard_limit(
                    limit,
                    self.pit.deadline,
                    self.pit_exact,
                    self.tolerance,
                ))?;
                self.sync_page_timer()?;
                self.expire_timers()?;
                // Also runs on slice entry after world RX delivery. Never poll
                // at overflow landings or intermediate precision steps.
                if self.poll_net(frames, self.time >= slice_end) {
                    self.note_skew(Site::Sent);
                    self.set_stepping(false)?;
                    return Ok(Stop::Sent);
                }
                if self.time >= slice_end {
                    self.set_stepping(false)?;
                    return Ok(Stop::Reached);
                }
                self.publish();
            }
            // Stop at the first natural exit at or after `soft` and below
            // `hard`, otherwise by an exact counted stop at `hard`. Two rules combine: the slice
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
                if remaining > tb.inject_guard() {
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
            let syscalls0 = self.page_get(PAGE_SYSCALLS);
            if let Prepared::Idle = tb.prepare(self, remaining, soft, legal)? {
                if self.page_timer.enabled {
                    // A proven cycle's canonical landing is deterministic, unlike the
                    // watchdog and intermediate probe steps that found it.
                    self.sync_from_page(hard)?;
                    self.sync_page_timer()?;
                    self.expire_timers()?;
                }
                // A proven PV cycle is deterministic; watchdog landings are not.
                // Complete pending TX before declaring a completion waiter idle,
                // including with interrupts disabled or suppressed by the guest.
                if deferred {
                    // Device DMA must not cross a checkpoint held during the
                    // cycle proof, just as at any other deterministic stop.
                    if let Some(u) = self.undo.as_mut() { u.valid = false; }
                    self.fm = self.undo_margin;
                    self.set_stepping(false)?;
                    let memory = unsafe { std::slice::from_raw_parts(self.mem, self.mem_size) };
                    let pending = self.net.as_ref().is_some_and(|n| n.pending_tx(memory));
                    if self.poll_net(frames, true) {
                        self.note_skew(Site::Sent);
                        return Ok(Stop::Sent);
                    }
                    if pending { legal = true;continue; }
                }
                return self.spin_idle();
            }
            self.vcpu.get_kvm_run().kvm_valid_regs =
                (KVM_SYNC_X86_REGS | KVM_SYNC_X86_SREGS) as u64;
            let c0 = tb.start(syscalls0);
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
            let syscalls1 = unsafe { std::ptr::read_volatile(self.page.add(PAGE_SYSCALLS)) };
            let c1 = tb.read(syscalls1);
            let delta = c1 - c0;
            let interrupted = matches!(&exit, Err(e) if e.errno() == libc::EINTR);
            let syscalled = syscalls1 != syscalls0;
            let run_syscalls = syscalls1.wrapping_sub(syscalls0);
            let run = Run {
                delta,
                remaining,
                rem_soft,
                windowed,
                window,
                interrupted,
                debug: matches!(exit, Ok(VcpuExit::Debug(_))),
                run_ns,
                syscalled,
            };
            let judge = Judge {
                name: &self.name,
                stepping: self.stepping,
                pv: self.pv,
                undo_valid: self.undo.as_ref().is_some_and(|u| u.valid),
                fm: &mut self.fm,
                margin: &mut self.margin,
                adaptive_margin: self.adaptive_margin,
                armed_wake: self.armed_wake,
                profile: self.profile,
                stats: &mut self.stats,
            };
            match tb.judge(judge, &run)? {
                Verdict::Advance { legal: l } => legal = l,
                Verdict::Rollback => {
                    drop(exit);
                    self.rollback()?;
                    legal = false;
                    continue;
                }
                Verdict::Overshoot => {
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
            }
            if T::BRANCHES {
                self.branches += delta;
            }
            self.stats.time_by[Cause::Progress as usize] += delta;
            self.time += delta;
            if legal {
                // Sent/Hlt/Exited can return before the next loop synchronizes
                // the page. Validate guest clock progress before exit effects.
                // The exit still borrows vcpu: use only the disjoint fields.
                let last = unsafe { std::ptr::read_volatile(self.page.add(1)) };
                let exit_time = advance_time(self.time, last.saturating_sub(self.time), hard)?;
                if self.page_timer.enabled {
                    // In particular HLT must adopt at its full exit time, not
                    // at a watchdog landing's partial syscall-only time.
                    self.stats.time_by[Cause::ClockReads as usize] += exit_time - self.time;
                    self.time = exit_time;
                    let word = unsafe { std::ptr::read_volatile(self.page.add(PAGE_TIMER_DEADLINE)) };
                    if self.page_timer.pickup(word, &mut self.pit, self.time)? { self.pic.raise(0); }
                    timer_latency(&mut self.pit, &mut self.pit_exact, self.timer_rng.as_mut(), self.timer_jitter, self.exact_timer_ppm);
                    if self.pit.expire_through(self.time)? > 0 { self.pic.raise(0); }
                }
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
                Ok(VcpuExit::IoOut(0x52c, data)) => {
                    // Requested yield: a stop if we reached the soft target,
                    // otherwise transparent (nothing observable happens).
                    // PV clock only: 1 means the guest's clock would pass
                    // the bound we published (`soft`); it waits for time to
                    // move, so move it there.
                    let v = data[0];
                    if v == 1 && self.pv_clock {
                        let bound = if self.page_timer.enabled { self.page_get(PAGE_BOUND).min(soft) } else { soft };
                        self.advance_to(bound, Cause::ClockBound);
                    }
                    self.stats.yields += 1;
                    if self.time >= soft {
                        self.stats.yield_stops += 1;
                        self.record(b'Y', v as u64);
                    } else {
                        // A page arm can lower the clock/syscall budget below
                        // the previous soft bound. This is a legal pickup.
                        legal = self.page_timer.enabled;
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
                    let console_before=self.com1.out.len();
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
                            self.abi = v as u64;
                            if self.abi < ABI_PAGE_RESEED && self.page_get(PAGE_ENTROPY_GEN) != 0 {
                                return Err(format!("{}: checkpoint entropy requires guest ABI 3", self.name));
                            }
                            if self.pv_clock && self.page_get(PAGE_PV_CLOCK) != 2 {
                                return Err(format!(
                                    "{}: the guest kernel lacks PV clock support; rebuild it from kernel/ for the pv timebase",
                                    self.name
                                ));
                            }
                            if self.page_timer_requested && self.abi < ABI_PAGE_TIMER {
                                return Err("the guest kernel lacks page timer support; rebuild it from kernel/ or drop --page-timer".into());
                            }
                            self.enable_page_timer();
                            self.pv = v >= 2;
                            if self.pv {
                                self.sync_pic();
                            }
                        }
                        0x40..=0x43 | 0x61 | 0x530 => {
                            let hi = data.get(1).copied().unwrap_or(0);
                            self.stats.timer_writes += 1;
                            self.note_skew(Site::Timer);
                            if let Some(d) = self.pit.deadline.filter(|&d| d <= now) {
                                // The run that ends here can pass the deadline
                                // (PV syscall charges, or counter tolerance).
                                // Rewriting the PIT would drop that tick.
                                let s = &mut self.stats;
                                s.timer_overdue += 1;
                                s.timer_overdue_max_ns = s.timer_overdue_max_ns.max(now - d);
                                s.timer_write_syscalls_max = s.timer_write_syscalls_max.max(run_syscalls);
                                // PV only, so counter witnesses don't move:
                                // deliver the due tick before the write.
                                if self.pv_clock {
                                    self.expire_timers()?;
                                }
                            }
                            if port == 0x530 {
                                let ticks = u16::from_le_bytes([v, hi]);
                                self.pit.program(ticks, now);
                            } else {
                                self.pit.write(port, v, now);
                            }
                            // A legacy write supersedes the last page request.
                            if self.page_timer.enabled {
                                self.page_timer.seen = self.page_get(PAGE_TIMER_DEADLINE);
                            }
                            timer_latency(&mut self.pit, &mut self.pit_exact, self.timer_rng.as_mut(), self.timer_jitter, self.exact_timer_ppm);
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
                        BARRIER_PORT if self.barrier => {
                            if self.parked || self.entropy_read != 0 {
                                return Err(format!("{}: launch barrier reached twice", self.name));
                            }
                            self.parked = true;
                            self.record(b'B', 0);
                            self.set_stepping(false)?;
                            // Finish the port write now, so the machine is
                            // parked in a state a fork can save and restore.
                            self.complete_io()?;
                            return Ok(Stop::Barrier);
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
                    self.capture_output();
                    if self.observe_console && self.com1.out.len()>console_before {
                        // Judge::Rollback discarded the exit before reaching here.
                        // Console ports have zero charge; the sink and pending KVM
                        // response are committed once at this deterministic exit.
                        self.committed_console=Some((after_io,v));control_stop=true;
                        if let Some(u)=self.undo.as_mut() {u.valid=false;}
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
                            self.stats.time_by[Cause::ClockReads as usize] += t - self.time;
                            self.time = t;
                            data.copy_from_slice(&(t as u32).to_le_bytes()[..data.len()]);
                        }
                        0x524 => data.copy_from_slice(
                            &((self.tsc_latch >> 32) as u32).to_le_bytes()[..data.len()],
                        ),
                        BARRIER_PORT if self.barrier => {
                            let word = self.entropy.get(self.entropy_read).copied().unwrap_or(0);
                            self.entropy_read += 1;
                            let len = data.len().min(4);
                            data[..len].copy_from_slice(&word.to_le_bytes()[..len]);
                        }
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
                    } else if (BLK_MMIO..BLK_MMIO + 0x1000).contains(&addr) {
                        self.blk.as_mut().map(|b| b.read(addr - BLK_MMIO)).unwrap_or(0)
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
                    if (BLK_MMIO..BLK_MMIO + 0x1000).contains(&addr) {
                        let m = unsafe { std::slice::from_raw_parts_mut(self.mem, self.mem_size) };
                        if let Some(b) = self.blk.as_mut() {
                            let done = b.write(m, addr - BLK_MMIO, v)?;
                            self.take_dma();
                            if done.completed > 0 {
                                // The disk changed outside guest RAM: no rollback
                                // may cross this point.
                                if let Some(u) = self.undo.as_mut() { u.valid = false; }
                            }
                            // Each request, in completion order: the trace shows
                            // where every FLUSH falls among the writes.
                            for (kind, sector, status) in done.log {
                                self.record(b'd', (kind as u64) << 56 | (status as u64) << 48 | sector);
                            }
                            if done.irq {
                                self.pic.raise(BLK_IRQ);
                            }
                        }
                    }
                    if (NET_MMIO..NET_MMIO + 0x1000).contains(&addr) {
                        let (mem, size) = (self.mem, self.mem_size);
                        let m = unsafe { std::slice::from_raw_parts_mut(mem, size) };
                        if let Some(n) = self.net.as_mut() {
                            let out = n.write_at(m, addr - NET_MMIO, v, self.time.max(unsafe { std::ptr::read_volatile(self.page.add(1)) }));
                            self.take_dma();
                            if out.irq {
                                self.raise_net();
                            }
                            if !out.frames.is_empty() {
                                frames.extend(out.frames);
                                self.note_skew(Site::Sent);
                                self.set_stepping(false)?;
                                return Ok(Stop::Sent);
                            }
                        }
                    }
                }
                Ok(VcpuExit::Hlt) => {
                    self.stats.halts += 1;
                    self.note_skew(Site::Halt);
                    self.record(b'H', 0);
                    // The world checks pending interrupts while halted.
                    self.sync_pic();
                    self.set_stepping(false)?;
                    if deferred && self.poll_net(frames, true) {
                        self.note_skew(Site::Sent);
                        return Ok(Stop::Sent);
                    }
                    if self.vcpu.get_kvm_run().if_flag == 0 {
                        self.state = State::Exited(0xfe);
                        return Ok(Stop::Exited);
                    }
                    // A guest may publish buffers and halt without a kick.
                    if self.poll_net(frames, true) {
                        self.note_skew(Site::Sent);
                        return Ok(Stop::Sent);
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
    /// page, RAM and any retained disk. The digest is the hash of the parts.
    /// Not covered: other MSRs, debug registers and other VMM device models.
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
        if self.page_timer.enabled {
            h.update(self.page_get(PAGE_TIMER_DEADLINE).to_le_bytes());
        }
        parts.push(("time_page", hex(h)));
        let mut h = Sha256::new();
        h.update(self.memory());
        parts.push(("ram", hex(h)));
        let mut h = Sha256::new();
        h.update(self.pending_io_bytes());
        parts.push(("pending_io", hex(h)));
        if let Some(b) = &self.blk {
            parts.push(("disk", b.disk.digest()));
        }
        let mut all = Sha256::new();
        for (_, d) in &parts {
            all.update(d.as_bytes());
        }
        (hex(all), parts)
    }

    /// Headerless little-endian u64 per 4 KiB guest RAM page, in PFN order.
    /// Each u64 is the first 8 SHA-256 bytes interpreted little-endian.
    /// Evidence only: reads RAM without changing it or consuming RNG draws.
    pub fn page_hashes(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for page in self.memory().chunks(4096) {
            let hash=u64::from_le_bytes(Sha256::digest(page)[..8].try_into().unwrap());
            out.extend_from_slice(&hash.to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
mod net_poll_tests {
    use super::*;

    // All exits in these tiny payloads are natural, with no PMU or host-time
    // stepping. Test the machine's service wiring, not counter qualification.
    struct Natural;
    impl Timebase for Natural {
        const BRANCHES: bool = false;
        fn prepare(&mut self, _: &mut Machine, _: u64, _: u64, _: bool) -> Result<Prepared, String> { Ok(Prepared::Run) }
        fn start(&mut self, _: u64) -> u64 { 0 }
        fn read(&mut self, _: u64) -> u64 { 0 }
        fn inject_guard(&self) -> u64 { INJECT_GUARD }
        fn judge(&mut self, _: Judge<'_>, _: &Run) -> Result<Verdict, String> { Ok(Verdict::Advance { legal: true }) }
    }

    #[test]
    #[ignore = "requires KVM on a Linux x86-64 host"]
    fn event_idx_polls_on_entry_slice_boundary_and_halt_without_kicks() {
        let kvm = Kvm::new().unwrap();
        for site in ["entry", "slice", "bounded_slice", "halt", "charged_io"] {
            for (event_idx, deferred) in [(false, false), (true, false), (false, true), (true, true)] {
                let case = format!("site={site} event_idx={event_idx} deferred={deferred}");
                // Publish a head, then yield to the slice end, halt, or start
                // charged port I/O. Entry starts with an already posted head.
                let mut code = vec![0xc7, 0x05, 0x02, 0x20, 0x08, 0x00, 1, 0, 0, 0];
                if site == "slice" || site == "bounded_slice" { code.extend_from_slice(&[0xb0, 1, 0xba, 0x2c, 0x05, 0, 0, 0xee]); }
                if site == "charged_io" { code.extend_from_slice(&[0xba, 0x30, 0x05, 0, 0, 0xee]); }
                code.push(0xf4);
                let mut kernel = vec![0; 1024];kernel[0x1f1] = 1;kernel.extend(code);
                let spec = BootSpec { kernel: &kernel, initrd: &[], cmdline: String::new(), mem: INITRD_ADDR + 0x100000,
                    rng_seed: [0; 32], mac: Some([2, 0, 0, 0, 0, 1]), control_channel: false, disk: None,
                    undo_margin: 0, barrier: false, pv_clock: true, page_timer:false};
                let mut m = Machine::new(&kvm, "net-poll", &spec).unwrap();
                let mut r = m.vcpu.get_regs().unwrap();r.rflags = 0x202;m.vcpu.set_regs(&r).unwrap();
                let memory = unsafe { std::slice::from_raw_parts_mut(m.mem, m.mem_size) };
                let net = m.net.as_mut().unwrap();
                net.deferred_tx = deferred;
                for (off, v) in [(0x020, (1 << 5) | if event_idx { 1 << 29 } else { 0 }), (0x024, 1), (0x020, 1), (0x070, 15),
                    (0x030, 1), (0x038, 1), (0x080, 0x81000), (0x090, 0x82000), (0x0a0, 0x83000), (0x044, 1)] {
                    net.write(memory, off, v);
                }
                memory[0x81000..0x81008].copy_from_slice(&0x84000u64.to_le_bytes());
                memory[0x81008..0x8100c].copy_from_slice(&17u32.to_le_bytes());
                memory[0x8400c..0x84011].copy_from_slice(b"frame");
                if site == "entry" { memory[0x82002..0x82004].copy_from_slice(&1u16.to_le_bytes()); }
                let mut frames = Vec::new();
                let (limit, early) = if site == "bounded_slice" { (1_000_000, 100_000) } else { (500, 0) };
                let stop = m.run_until(&mut Natural, limit, early, &mut frames).unwrap();
                if site == "bounded_slice" {
                    assert_eq!(m.time, if deferred { crate::virtio_net::TX_POLL_NS } else { limit - early }, "{case}");
                }
                if event_idx || deferred {
                    assert_eq!(stop, Stop::Sent, "{case}");
                    assert_eq!(frames, [b"frame".to_vec()], "{case}");
                    // used->idx is at used + 2; used + 12 is avail_event
                    // for this one-entry queue. Only one head was posted.
                    assert_eq!(u16::from_le_bytes(m.memory()[0x83002..0x83004].try_into().unwrap()), 1, "{case}");
                } else {
                    assert_eq!(stop, if site == "slice" || site == "bounded_slice" || site == "charged_io" { Stop::Reached } else { Stop::Halted }, "{case}");
                    assert!(frames.is_empty(), "{case}");
                }
                let net = m.net.as_ref().unwrap();
                assert_eq!(net.doorbell_exits, 0, "{case}");
                assert_eq!(net.tx_frames, u64::from(event_idx || deferred), "{case}");
                assert_eq!(net.tx_polled_at_boundary, u64::from((event_idx || deferred) && site != "entry"), "{case}");
                assert!(net.dirty.is_empty(), "{case}"); // Machine adopted all DMA writes.
            }
        }
    }
    #[test]
    #[ignore = "requires KVM on a Linux x86-64 host"]
    fn deferred_tx_completes_a_pv_spin_without_halt_kick_or_interrupts() {
        use std::time::Duration;
        use super::pv::PvClock;
        let kvm = Kvm::new().unwrap();
        for (event_idx, page_timer) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut first = None;
            for ms in [2, 7] {
                let case = format!("site=pv_spin event_idx={event_idx} page_timer={page_timer} deferred=true watchdog_ms={ms}");
                // Publish TX, then busy-wait for used->idx with interrupts off.
                // The stable cycle is serviced at its canonical point; never
                // service at the host-time watchdog or a probe landing.
                let mut code = vec![0xc7, 0x05, 0x02, 0x20, 0x08, 0x00, 1, 0, 0, 0,
                    0x66, 0x83, 0x3d, 0x02, 0x30, 0x08, 0, 0, 0x74, 0xf6, 0xf4];
                if page_timer {
                    // Store clock progress and an arm before publishing TX.
                    let mut stores = Vec::new();
                    for (address, value) in [(0xd0000008u32, 12300u32), (0xd0000070, 40000), (0xd0000074, 0)] {
                        stores.extend_from_slice(&[0xc7, 0x05]);
                        stores.extend_from_slice(&address.to_le_bytes());
                        stores.extend_from_slice(&value.to_le_bytes());
                    }
                    stores.extend(code);code = stores;
                }
                let mut kernel = vec![0; 1024];kernel[0x1f1] = 1;kernel.extend(code);
                let spec = BootSpec { kernel: &kernel, initrd: &[], cmdline: String::new(), mem: INITRD_ADDR + 0x100000,
                    rng_seed: [0; 32], mac: Some([2, 0, 0, 0, 0, 1]), control_channel: false, disk: None,
                    undo_margin: 0, barrier: false, pv_clock: true, page_timer};
                let mut m = Machine::new(&kvm, "tx-spin", &spec).unwrap();
                m.abi = ABI_PAGE_TIMER;m.enable_page_timer();
                let memory = unsafe { std::slice::from_raw_parts_mut(m.mem, m.mem_size) };
                let net = m.net.as_mut().unwrap();net.deferred_tx = true;
                for (off, v) in [(0x020, if event_idx { 1 << 29 } else { 0 }), (0x070, 15),
                    (0x030, 1), (0x038, 1), (0x080, 0x81000), (0x090, 0x82000), (0x0a0, 0x83000), (0x044, 1)] {
                    net.write(memory, off, v);
                }
                memory[0x81000..0x81008].copy_from_slice(&0x84000u64.to_le_bytes());
                memory[0x81008..0x8100c].copy_from_slice(&17u32.to_le_bytes());
                memory[0x8400c..0x84011].copy_from_slice(b"frame");
                memory[0x82000..0x82002].copy_from_slice(&1u16.to_le_bytes()); // NO_INTERRUPT
                memory[0x82006..0x82008].copy_from_slice(&1u16.to_le_bytes()); // used_event
                let mut tb = PvClock::new(Duration::from_millis(ms)).unwrap();tb.max_false_alarms = 10;
                let mut frames = Vec::new();
                assert_eq!(m.run_until(&mut tb, 1_000_000, 0, &mut frames).unwrap(), Stop::Sent, "{case}");
                assert_eq!(frames, [b"frame".to_vec()], "{case}");
                assert_eq!(m.time, if page_timer { 12300 } else { 0 }, "{case}");
                assert_eq!(m.pit.deadline, page_timer.then_some(40000), "{case}");
                assert_eq!(m.net.as_ref().unwrap().doorbell_exits, 0, "{case}");
                assert_eq!(m.run_until(&mut tb, 1_000_000, 0, &mut frames).unwrap(), Stop::Exited, "{case}");
                let evidence = (m.time, m.exit_witness(), m.state_digest());
                if let Some(want) = &first { assert_eq!(&evidence, want, "{case}"); } else { first = Some(evidence); }
            }
        }
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
        let spec=BootSpec {kernel:&kernel,initrd:&[],cmdline:String::new(),mem:INITRD_ADDR+0x100000,rng_seed:[0;32],mac:None,control_channel:false,disk:None,undo_margin:undo,barrier:false,pv_clock:false,page_timer:false};
        let mut machine=Machine::new(kvm,"io-probe",&spec).unwrap();
        let mut r=machine.vcpu.get_regs().unwrap();r.rflags=0x202;machine.vcpu.set_regs(&r).unwrap();
        machine.pit.write(0x43,0x34,0);
        machine.pit.program(0x1234,0);
        machine.pit.write(0x43,0,0);
        machine.pic.set_masks(0);
        machine
    }

    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn runtime_checkpoint_keeps_unpaid_io_without_repeating_its_effect() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        for undo in [0,256] {
            for (port,write) in [(0x40,false),(0x43,true),(0x530,true)] {
                let mut m=probe(&kvm,port,write,undo);
                let mut frames=Vec::new();
                assert_eq!(m.run_until(&mut counter,500,0,&mut frames).unwrap(),Stop::Reached);
                let pending=m.pending_io.clone();
                assert!(pending.is_some());
                let saved=m.snapshot().unwrap();
                let finish=|m: &mut Machine,counter: &mut Counter| {
                    let mut frames=Vec::new();
                    while !matches!(m.run_until(counter,10_000,0,&mut frames).unwrap(),Stop::Halted|Stop::Exited) {}
                    (m.exit_witness(),m.state_digest().0,m.time,m.branches)
                };
                let want=finish(&mut m,&mut counter);
                let mut fresh=probe(&kvm,port,write,undo);
                fresh.restore(&saved).unwrap();
                assert_eq!(fresh.pending_io,pending);
                assert_eq!(finish(&mut fresh,&mut counter),want,"port={port:#x}, write={write}, undo={undo}");
            }
        }
    }

    #[test]
    #[ignore = "requires KVM on the qualified Linux host"]
    fn page_capture_has_documented_format_and_preserves_ram_witness_state() {
        let kvm=Kvm::new().unwrap();
        let mut m=probe(&kvm,0x530,true,0);
        let before=(m.exit_witness(),m.state_digest(),m.memory().to_vec());
        let pages=m.page_hashes();
        assert_eq!(pages.len(),m.mem_size/4096*8);
        for (pfn,page) in before.2.chunks(4096).enumerate() {
            let hash=u64::from_le_bytes(Sha256::digest(page)[..8].try_into().unwrap());
            assert_eq!(&pages[pfn*8..pfn*8+8],&hash.to_le_bytes());
        }
        assert_eq!((m.exit_witness(),m.state_digest(),m.memory().to_vec()),before);
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

    /// A 32-bit loop of 1000 conditional branches, optionally followed by an
    /// uncharged OUT to port 0x80 (a natural exit), with interrupts masked.
    fn loop_probe(kvm: &Kvm, natural_exit: bool, undo: u64) -> Machine {
        // outer: mov ebx,1000; inner: dec ebx; jnz inner; [out 0x80,al]; jmp outer
        let mut code=vec![0xbb,0xe8,0x03,0x00,0x00,0x4b,0x75,0xfd];
        if natural_exit {code.extend_from_slice(&[0xe6,0x80]);}
        let back=-(code.len() as i8+2);
        code.extend_from_slice(&[0xeb,back as u8]);
        let mut kernel=vec![0u8;1024];kernel[0x1f1]=1;kernel.extend(code);
        let spec=BootSpec {kernel:&kernel,initrd:&[],cmdline:String::new(),mem:INITRD_ADDR+0x100000,rng_seed:[0;32],mac:None,control_channel:false,disk:None,undo_margin:undo,barrier:false,pv_clock:false,page_timer:false};
        let mut machine=Machine::new(kvm,"stop-probe",&spec).unwrap();
        let mut r=machine.vcpu.get_regs().unwrap();r.rflags=0x2;machine.vcpu.set_regs(&r).unwrap();
        machine.pic.set_masks(0xffff);
        machine
    }

    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn an_overdue_arm_replaced_by_a_page_store_does_not_depend_on_undo() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        // mov ecx,280000; dec ecx; jnz; store deadline 900000 to word 14; hlt
        let mut code = vec![0xb9, 0xc0, 0x45, 0x04, 0x00, 0x49, 0x75, 0xfd];
        for (address, value) in [(0xd0000070u32, 900_000u32), (0xd0000074, 0)] {
            code.extend_from_slice(&[0xc7, 0x05]);
            code.extend_from_slice(&address.to_le_bytes());
            code.extend_from_slice(&value.to_le_bytes());
        }
        code.push(0xf4);
        let mut want = None;
        for undo in [0, 16, 256] {
            let mut kernel = vec![0u8; 1024]; kernel[0x1f1] = 1; kernel.extend(&code);
            let spec = BootSpec { kernel: &kernel, initrd: &[], cmdline: String::new(), mem: INITRD_ADDR + 0x100000, rng_seed: [0; 32], mac: None, control_channel: false, disk: None, undo_margin: undo, barrier: false, pv_clock: false, page_timer: true };
            let mut m = Machine::new(&kvm, "page-rearm", &spec).unwrap();
            let mut r = m.vcpu.get_regs().unwrap(); r.rflags = 0x202; m.vcpu.set_regs(&r).unwrap();
            m.abi = ABI_PAGE_TIMER;
            m.enable_page_timer();
            m.tolerance = 200_000;
            m.pit.program_deadline(Some(100_000)); // D = 100k, hard = 300k, exit at ~280k
            m.pic.set_masks(0xffff);
            assert_eq!(m.run_until(&mut counter, 1_000_000, 0, &mut Vec::new()).unwrap(), Stop::Halted);
            m.pic.set_masks(0);
            let got = (m.time, m.pic.pending(), m.pit.deadline, m.exit_witness());
            if let Some(want) = &want { assert_eq!(&got, want, "undo={undo}"); } else { want = Some(got); }
        }
    }

    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn stops_at_natural_exits_or_exact_hard_counts_without_yields() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        // (exits, limit, early): no natural exit, so an exact counted stop at
        // the hard target; an exit every 1000 branches with soft below hard,
        // so the first exit at or after soft; and soft == hard with an exit
        // exactly on the hard count, which must stop before that exit.
        let cases=[(false,300_000,150_000),(true,300_000,150_000),(true,150_000,0)];
        for (exits,limit,early) in cases {
            let mut want=None;
            for undo in [0,16,256] {
                let mut m=loop_probe(&kvm,exits,undo);
                // The fixed margin can be shorter than a skid in this loop.
                m.adaptive_margin=true;
                let mut frames=Vec::new();
                assert_eq!(m.run_until(&mut counter,limit,early,&mut frames).unwrap(),Stop::Reached);
                let what=format!("exits={exits} limit={limit} early={early} undo={undo}");
                assert_eq!(m.page_get(2),0,"{what}: a yield window was requested");
                assert_eq!(m.stats.yields,0);
                match (exits,early) {
                    (false,_) => assert_eq!(m.time,limit,"{what}"),
                    (true,0) => {
                        assert_eq!(m.time,limit,"{what}");
                        assert_eq!(m.stats.pio,limit/1000-1,"{what}: ran the exit at the hard count");
                    }
                    (true,_) => {
                        assert!(m.time>=limit-early && m.time<limit-early+1001,"{what} time={}",m.time);
                        assert_eq!(m.stats.precise_stops,0,"{what}: counted stop despite a natural exit");
                    }
                }
                let rip=m.vcpu.get_regs().unwrap().rip;
                let evidence=(m.time,rip,m.exit_witness(),m.state_digest().0);
                if let Some(want)=&want {assert_eq!(&evidence,want,"{what}");} else {want=Some(evidence);}
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

    /// Pause with 161 ns of I/O cost owed and a periodic timer armed: while
    /// paused nothing runs and nothing is paid. Resumed later, the missed
    /// ticks fire once, late, in phase, and the debt is paid from the resume.
    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn a_pause_keeps_owed_io_cost_and_resumes_in_timer_phase() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        let mut want=None;
        for undo in [0,16,256] {
            let mut m=probe(&kvm,0x530,true,undo);
            let mut frames=Vec::new();
            assert_eq!(m.run_until(&mut counter,839,0,&mut frames).unwrap(),Stop::Reached);
            assert_eq!(m.pending_io.as_ref().unwrap().remaining_ns,161);
            assert_eq!(m.pit.deadline,Some(1678));
            m.pause().unwrap();
            assert!(m.pause().is_err());
            let rip=m.vcpu.get_regs().unwrap().rip;
            m.resume(5000).unwrap();
            assert!(m.resume(5000).is_err());
            assert_eq!(m.time,5000);
            assert_eq!(m.pit.deadline,Some(1678+4*839),"undo={undo}");
            assert_eq!(m.pending_io.as_ref().unwrap().remaining_ns,161,"debt kept while paused");
            assert_eq!((m.branches,m.vcpu.get_regs().unwrap().rip,m.stats.irqs),(0,rip,0));
            assert_eq!(m.run_until(&mut counter,5100,0,&mut frames).unwrap(),Stop::Reached);
            assert_eq!(m.pending_io.as_ref().unwrap().remaining_ns,61);
            assert_eq!(m.run_until(&mut counter,5161,0,&mut frames).unwrap(),Stop::Reached);
            assert!(m.pending_io.is_none());assert_eq!(m.branches,0);
            assert_eq!(m.run_until(&mut counter,7000,0,&mut frames).unwrap(),Stop::Halted);
            assert_eq!(m.branches,1);
            assert_eq!(&m.memory()[0x80000..0x80004],&[0x78,0x56,0x34,0x12]);
            let evidence=(m.time,m.exit_witness(),m.state_digest().0);
            if let Some(want)=&want {assert_eq!(&evidence,want,"undo={undo}");} else {want=Some(evidence);}
        }
    }

    /// Pause once the debt is paid but before the guest runs on: resuming
    /// later runs exactly what an unpaused machine runs, just later.
    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn a_pause_with_no_debt_runs_on_unchanged_after_resume() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        for undo in [0,256] {
            let mut m=probe(&kvm,0x61,false,undo);
            let mut frames=Vec::new();
            assert_eq!(m.run_until(&mut counter,1000,0,&mut frames).unwrap(),Stop::Reached);
            assert!(m.pending_io.is_none());
            let rax=m.vcpu.get_regs().unwrap().rax;
            m.pause().unwrap();
            m.resume(3000).unwrap();
            assert_eq!(m.vcpu.get_regs().unwrap().rax,rax);
            assert_eq!(m.run_until(&mut counter,5000,0,&mut frames).unwrap(),Stop::Halted);
            assert_eq!(m.branches,1);
            assert!(m.time>=3000 && m.time<5000);
            assert_eq!(&m.memory()[0x80000..0x80004],&[0x78,0x56,0x34,0x12]);
        }
    }

}

#[cfg(test)]
mod fork_tests {
    use super::*;

    #[test]
    fn page_entropy_carries_all_32_bytes_in_order() {
        let words: [u32; ENTROPY_WORDS] = std::array::from_fn(|i| 0x1111_1111 * (i as u32 + 1));
        let bytes: Vec<u8> = page_entropy(&words).iter().flat_map(|f| f.to_le_bytes()).collect();
        let want: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(bytes, want);
    }

    /// The field indices here must match struct seedbox_time_page in
    /// kernel/seedbox.h, which the guest kernel is built from.
    #[test]
    fn time_page_fields_match_the_kernel_header() {
        let header = include_str!("../../../kernel/seedbox.h");
        let body = header.split("struct seedbox_time_page {").nth(1).unwrap().split("};").next().unwrap();
        let mut index = 0;
        let mut fields = std::collections::BTreeMap::new();
        for line in body.lines().map(str::trim).filter(|l| l.starts_with("u64 ")) {
            let decl = line["u64 ".len()..].split(';').next().unwrap();
            let (name, count) = match decl.split_once('[') {
                Some((n, c)) => (n, c.trim_end_matches(']').parse::<usize>().unwrap()),
                None => (decl, 1),
            };
            fields.insert(name.to_string(), (index, count));
            index += count;
        }
        assert_eq!(fields["syscalls"], (PAGE_SYSCALLS, 1));
        assert_eq!(fields["pic_imr"], (PAGE_PIC_IMR, 1));
        assert_eq!(fields["net_isr"], (PAGE_NET_ISR, 1));
        assert_eq!(fields["pv_clock"], (PAGE_PV_CLOCK, 1));
        assert_eq!(fields["bound"], (PAGE_BOUND, 1));
        assert_eq!(fields["syscall_target"], (PAGE_SYSCALL_TARGET, 1));
        assert_eq!(fields["entropy_gen"], (PAGE_ENTROPY_GEN, 1));
        assert_eq!(fields["entropy"], (PAGE_ENTROPY, PAGE_ENTROPY_FIELDS));
        assert_eq!(fields["timer_deadline"], (PAGE_TIMER_DEADLINE, 1));
        assert_eq!(fields["capabilities"], (PAGE_CAPABILITIES, 1));
        assert_eq!(fields["syscall_ns"], (PAGE_SYSCALL_NS, 1));
        assert_eq!(fields["syscalls0"], (PAGE_SYSCALLS0, 1));
        assert_eq!(fields["plan_time"], (PAGE_PLAN_TIME, 1));
        assert_eq!(index, 19);
        assert_eq!(PAGE_FIELDS, PAGE_PV_CLOCK);
        assert_eq!(PAGE_ENTROPY_FIELDS * 8, ENTROPY_WORDS * 4);
        let abi: u64 = header.lines().find_map(|l| l.strip_prefix("#define SEEDBOX_ABI\t")).unwrap().trim().parse().unwrap();
        assert_eq!(abi, ABI_PAGE_TIMER);
    }

    #[test]
    #[ignore = "requires Linux x86-64 KVM"]
    fn page_timer_negotiation_and_undo_restore_the_separate_page_word() {
        let kvm = Kvm::new().unwrap();
        let mut m = probe_machine(&kvm, 256, false);
        for requested in [false, true] {
            for abi in 0..=ABI_PAGE_TIMER {
                m.page_timer_requested = requested;
                m.abi = abi;
                m.page_timer = PageTimer::new();
                m.page_set(PAGE_TIMER_DEADLINE, 0);
                m.page_set(PAGE_CAPABILITIES, 0);
                m.enable_page_timer();
                let active = requested && abi >= ABI_PAGE_TIMER;
                assert_eq!(m.page_timer.enabled, active);
                assert_eq!(m.page_get(PAGE_CAPABILITIES), if active { CAP_PAGE_TIMER } else { 0 });
                assert_eq!(m.page_get(PAGE_TIMER_DEADLINE), if active { TIMER_DISARMED } else { 0 });
            }
        }
        m.page_set(PAGE_TIMER_DEADLINE, 10000);
        m.sync_page_timer().unwrap();
        m.checkpoint().unwrap();
        m.page_set(PAGE_TIMER_DEADLINE, 20000);
        // Speculative guest stores are not adopted before a legal landing.
        m.rollback().unwrap();
        assert_eq!(m.page_get(PAGE_TIMER_DEADLINE), 10000);
        assert_eq!(m.next_deadline(), 10000);
        m.page_set(PAGE_TIMER_DEADLINE, TIMER_DISARMED);
        m.checkpoint().unwrap();
        m.page_set(PAGE_TIMER_DEADLINE, 30000);
        m.rollback().unwrap();
        assert_eq!(m.page_get(PAGE_TIMER_DEADLINE), TIMER_DISARMED);
        assert_eq!(m.next_deadline(), 10000, "unadopted disarm cannot change idle wake time");
        m.sync_page_timer().unwrap();
        assert_eq!(m.next_deadline(), TIMER_DISARMED);
        assert_eq!(m.pit.deadline, None);
    }

    // These tests create a vCPU but never enter the guest or open a perf event.
    #[test]
    #[ignore = "requires Linux x86-64 KVM"]
    fn abi_below_3_time_forks_do_not_write_extensions_or_add_reseed_witnesses() {
        let kvm=Kvm::new().unwrap();
        for abi in 0..ABI_PAGE_RESEED {
            let mut m=probe_machine(&kvm,0,false);
            assert!((6..14).all(|i|m.page_get(i)==0),"counter boot, ABI {abi}");
            for i in 6..14 {m.page_set(i,100+i as u64);}
            m.abi=abi;
            m.park().unwrap();
            let mut witness=m.witness.clone();
            witness.update([b'R']);
            witness.update(m.branches.to_le_bytes());
            witness.update(m.time.to_le_bytes());
            witness.update(m.time.to_le_bytes());
            // Pre-K2 exit_witness also covers pending I/O: none after park().
            witness.update(b"pending_io");
            witness.update([0]);
            assert_eq!(m.release(m.time,[7;ENTROPY_WORDS]).unwrap(),Reseed::Unsupported);
            assert!((6..14).all(|i|m.page_get(i)==100+i as u64),"ABI {abi}");
            assert_eq!(m.exit_witness(),format!("{:x}",witness.finalize()),"ABI {abi}");
        }
    }

    #[test]
    #[ignore = "requires Linux x86-64 KVM"]
    fn a_time_fork_writes_only_entropy_and_a_barrier_writes_none() {
        let kvm=Kvm::new().unwrap();
        for barrier in [false,true] {
            let mut m=probe_machine(&kvm,0,barrier);
            assert!((6..14).all(|i|m.page_get(i)==0),"counter boot");
            m.abi=ABI_PAGE_RESEED;
            for i in 0..14 {m.page_set(i,100+i as u64);}
            let before:Vec<_>=(0..14).map(|i|m.page_get(i)).collect();
            // Both release paths require a parked world, before any page write.
            assert!(m.release(m.time,[7;ENTROPY_WORDS]).is_err());
            assert_eq!((0..14).map(|i|m.page_get(i)).collect::<Vec<_>>(),before);
            m.park().unwrap();
            let how=m.release(m.time,[7;ENTROPY_WORDS]).unwrap();
            assert_eq!(how,if barrier {Reseed::Barrier} else {Reseed::Page});
            assert_eq!((0..9).map(|i|m.page_get(i)).collect::<Vec<_>>(),before[..9]);
            if barrier {
                assert_eq!((9..14).map(|i|m.page_get(i)).collect::<Vec<_>>(),before[9..14]);
            } else {
                assert_eq!(m.page_get(PAGE_ENTROPY_GEN),before[9]+1);
                assert_eq!((10..14).map(|i|m.page_get(i)).collect::<Vec<_>>(),page_entropy(&[7;ENTROPY_WORDS]));
            }
        }
    }

    /// 32-bit payload through the real boot handoff: announce the launch
    /// barrier, read one entropy word into a marker, spin 1000 counted
    /// branches, write a second page, halt.
    fn barrier_machine(kvm: &Kvm, undo: u64) -> Machine {
        probe_machine(kvm, undo, true)
    }

    fn probe_machine(kvm: &Kvm, undo: u64, barrier: bool) -> Machine {
        let mut code=vec![0xb8,1,0,0,0, 0xba,0xf8,0,0,0, 0xef, 0xed, 0xa3,0x00,0x00,0x08,0x00];
        code.extend_from_slice(&[0xb9,0xe8,0x03,0,0, 0x49, 0x75,0xfd]);
        code.extend_from_slice(&[0xc7,0x05,0x00,0x10,0x08,0x00,0xad,0xde,0,0, 0xf4]);
        let mut kernel=vec![0u8;1024];kernel[0x1f1]=1;kernel.extend(code);
        let spec=BootSpec {kernel:&kernel,initrd:&[],cmdline:String::new(),mem:INITRD_ADDR+0x100000,rng_seed:[0;32],mac:None,control_channel:false,disk:None,undo_margin:undo,barrier,pv_clock:false,page_timer:false};
        Machine::new(kvm,"fork-probe",&spec).unwrap()
    }

    fn to_barrier(m: &mut Machine, counter: &mut Counter) {
        let mut frames=Vec::new();
        assert_eq!(m.run_until(counter,1_000_000,0,&mut frames).unwrap(),Stop::Barrier);
        assert!(m.parked);
    }

    fn released(m: &mut Machine, counter: &mut Counter, word: u32) -> (String, String, u64, u64, Vec<u8>) {
        let at=m.time+10;
        m.release(at,[word;ENTROPY_WORDS]).unwrap();
        let mut frames=Vec::new();
        loop {
            match m.run_until(counter,10_000_000,0,&mut frames).unwrap() {
                Stop::Exited|Stop::Halted => break,
                Stop::Reached => {},
                other => panic!("unexpected stop {other:?}"),
            }
        }
        assert_eq!(&m.memory()[0x80000..0x80004],&word.to_le_bytes());
        assert_eq!(&m.memory()[0x81000..0x81004],&[0xad,0xde,0,0]);
        (m.exit_witness(),m.state_digest().0,m.time,m.branches,m.memory()[0x80000..0x80004].to_vec())
    }

    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn a_restored_fork_matches_a_cold_run_and_seeds_differ() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        for undo in [0,256] {
            let mut cold=barrier_machine(&kvm,undo);
            to_barrier(&mut cold,&mut counter);
            let want=released(&mut cold,&mut counter,7);
            assert!(want.3>=1000,"spin loop branches counted");

            let mut m=barrier_machine(&kvm,undo);
            to_barrier(&mut m,&mut counter);
            let snapshot=m.snapshot().unwrap();
            assert_eq!(released(&mut m,&mut counter,7),want,"first seed after taking a snapshot, undo={undo}");
            let pages=m.restore(&snapshot).unwrap();
            assert!(pages>=2,"marker pages copied back, undo={undo}");
            assert!(m.parked);
            assert_eq!(&m.memory()[0x80000..0x80004],&[0,0,0,0]);
            let other=released(&mut m,&mut counter,9);
            // Both seeds take the same path, and the event witness records
            // port reads without their values, so only the state differs.
            assert_eq!(other.0,want.0);assert_ne!(other.1,want.1);
            assert_eq!(other.4,9u32.to_le_bytes());
            m.restore(&snapshot).unwrap();
            assert_eq!(released(&mut m,&mut counter,7),want,"seed restored from the fork, undo={undo}");
        }
    }

    /// A machine released later than it parked (another machine parked
    /// later, or a release skew), with its timer due in between: the timer
    /// fires late at the release instead of leaving the next slice a hard
    /// stop behind its clock.
    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn a_release_past_an_armed_timer_fires_it_and_runs_on() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        for undo in [0,256] {
            let mut m=barrier_machine(&kvm,undo);
            to_barrier(&mut m,&mut counter);
            let parked=m.time;
            // Periodic channel 0 armed at the barrier, as a guest's tick is.
            m.pit.write(0x43,0x34,parked);
            m.pit.program(12,parked);
            let period=m.pit.deadline.unwrap()-parked;
            m.release(parked+4*period+period/2,[7;ENTROPY_WORDS]).unwrap();
            // Four ticks fell due while parked; they fire as one late IRQ0
            // (masked in this probe) and the next is armed past the release.
            assert_eq!(m.pit.deadline,Some(parked+5*period),"undo={undo}");
            let mut frames=Vec::new();
            while !matches!(m.run_until(&mut counter,parked+10_000_000,0,&mut frames).unwrap(),Stop::Exited|Stop::Halted) {}
            assert_eq!(&m.memory()[0x81000..0x81004],&[0xad,0xde,0,0]);
        }
    }

    /// Fork at a simulated time inside the spin loop instead of at the guest's
    /// barrier (the port write and read are then ordinary unknown ports).
    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn a_fork_at_a_simulated_time_matches_a_cold_run() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        let finish=|m: &mut Machine, counter: &mut Counter| {
            let at=m.time+10;
            m.release(at,[0;ENTROPY_WORDS]).unwrap();
            let mut frames=Vec::new();
            while !matches!(m.run_until(counter,10_000_000,0,&mut frames).unwrap(),Stop::Exited|Stop::Halted) {}
            (m.exit_witness(),m.state_digest().0,m.time,m.branches)
        };
        for undo in [0,256] {
            let mut cold=probe_machine(&kvm,undo,false);
            let mut frames=Vec::new();
            assert_eq!(cold.run_until(&mut counter,500,0,&mut frames).unwrap(),Stop::Reached);
            assert_eq!(cold.time,500);
            cold.park().unwrap();
            let want=finish(&mut cold,&mut counter);

            let mut m=probe_machine(&kvm,undo,false);
            assert_eq!(m.run_until(&mut counter,500,0,&mut frames).unwrap(),Stop::Reached);
            m.park().unwrap();
            let snapshot=m.snapshot().unwrap();
            assert_eq!(finish(&mut m,&mut counter),want,"first run after a snapshot, undo={undo}");
            m.restore(&snapshot).unwrap();
            assert_eq!(m.time,500);
            assert_eq!(finish(&mut m,&mut counter),want,"restored fork, undo={undo}");
        }
    }

    /// A seed that crashed a machine leaves a later incarnation in a new VM,
    /// so the next seed restores into a freshly built machine that holds no
    /// fork point: a full copy, after which it holds one and later restores
    /// take the cheap path again. Every restore must match the cold run.
    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn a_fork_restores_into_a_freshly_built_machine() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        for undo in [0,256] {
            let mut cold=barrier_machine(&kvm,undo);
            to_barrier(&mut cold,&mut counter);
            let want=released(&mut cold,&mut counter,7);

            let mut m=barrier_machine(&kvm,undo);
            to_barrier(&mut m,&mut counter);
            let snapshot=m.snapshot().unwrap();
            drop(m);
            let mut fresh=barrier_machine(&kvm,undo);
            if undo>0 {assert!(!fresh.undo.as_ref().unwrap().holds_fork_point());}
            let pages=fresh.restore(&snapshot).unwrap();
            assert_eq!(pages,(fresh.mem_size/4096) as u64,"full copy, undo={undo}");
            assert!(fresh.parked);
            assert_eq!(released(&mut fresh,&mut counter,7),want,"restored into a fresh machine, undo={undo}");
            if undo>0 {assert!(fresh.undo.as_ref().unwrap().holds_fork_point());}
            let pages=fresh.restore(&snapshot).unwrap();
            if undo>0 {assert!(pages<(fresh.mem_size/4096) as u64,"only written pages copied back");}
            assert_eq!(released(&mut fresh,&mut counter,7),want,"second restore, undo={undo}");
        }
    }

    fn to_halt(m: &mut Machine, counter: &mut Counter, limit: u64) {
        let mut frames=Vec::new();
        while !matches!(m.run_until(counter,limit,0,&mut frames).unwrap(),Stop::Exited|Stop::Halted) {}
    }

    /// Crash a machine and boot its next incarnation from the same inputs:
    /// fresh RAM, the program runs again from its first instruction at the
    /// reboot time, and the logs and witness carry across.
    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn a_crashed_machine_reboots_fresh_from_its_boot_inputs() {
        let kvm=Kvm::new().unwrap();
        let mut counter=Counter::new().unwrap();
        let mut want=None;
        for undo in [0,256] {
            let mut old=probe_machine(&kvm,undo,false);
            to_halt(&mut old,&mut counter,1_000_000);
            assert_eq!(&old.memory()[0x81000..0x81004],&[0xad,0xde,0,0]);
            let first=(old.branches,old.exit_witness());
            old.com1.out.extend_from_slice(b"first boot\n");
            old.syscall_ns=937;
            let crash=old.time+100;
            let mut new=probe_machine(&kvm,undo,false);
            new.succeed(&mut old,crash,1009,None).unwrap();
            assert!(new.down && !new.paused);
            assert_eq!(new.syscall_ns,937,"the next incarnation keeps the PV syscall cost");
            assert_eq!((new.incarnation,new.time),(1,crash));
            assert_eq!(&new.memory()[0x80000..0x80004],&[0,0,0,0]);
            assert_eq!(&new.memory()[0x81000..0x81004],&[0,0,0,0]);
            assert_eq!(new.com1.out,b"first boot\n");
            assert!(old.com1.out.is_empty());
            drop(old);
            assert!(new.boot_at(crash-1).is_err());
            new.boot_at(crash+1000).unwrap();
            assert!(new.boot_at(crash+1000).is_err());
            let rip=new.vcpu.get_regs().unwrap().rip;
            assert_eq!(rip,0x100000,"the new incarnation starts at the kernel entry");
            to_halt(&mut new,&mut counter,crash+1_000_000);
            assert_eq!(&new.memory()[0x81000..0x81004],&[0xad,0xde,0,0]);
            assert_eq!(new.branches,first.0,"same program, counted from zero");
            assert!(new.time>crash+1000);
            assert_ne!(new.exit_witness(),first.1,"the witness covers both incarnations");
            let evidence=(first,new.time,new.exit_witness(),new.state_digest().0);
            if let Some(want)=&want {assert_eq!(&evidence,want,"undo={undo}");} else {want=Some(evidence);}
        }
    }

    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn a_reboot_applies_seeded_sector_survival_and_keeps_the_default_witness() {
        let kvm=Kvm::new().unwrap();
        for fault_model in [crate::disk::DiskFaultModel { sector_bytes: crate::disk::SECTOR, survival_probability: 0.0 },crate::disk::DiskFaultModel { sector_bytes: crate::disk::SECTOR, survival_probability: 0.5 }] {
            let run=|seed| {
                let mut old=probe_machine(&kvm,256,false);
                let mut disk=Disk::new(None,Some(32)).unwrap().with_fault_model(fault_model);
                disk.write(0,&[b'A';32*512]).unwrap();
                disk.flush();
                disk.write(0,&[b'B';32*512]).unwrap();
                let expected=disk.crashed_seeded(seed,&old.name,old.incarnation);
                old.blk=Some(VirtioBlk::new(disk));
                let mut new=probe_machine(&kvm,256,false);
                new.blk=Some(VirtioBlk::new(Disk::new(None,Some(32)).unwrap().with_fault_model(fault_model)));
                // The pre-policy successor witness: old witness followed by C.
                let mut witness=new.witness.clone();
                witness.update(old.exit_witness().as_bytes());
                witness.update([b'C']);
                witness.update(new.branches.to_le_bytes());
                witness.update(1000u64.to_le_bytes());
                witness.update(1000u64.to_le_bytes());
                witness.update(b"pending_io");
                witness.update(new.pending_io_bytes());
                assert_eq!(new.succeed(&mut old,1000,seed,None).unwrap(),Some(expected.1));
                assert_eq!(new.exit_witness(),format!("{:x}",witness.finalize()));
                let disk=&new.blk.as_ref().unwrap().disk;
                assert_eq!(disk.digest(),expected.0.digest());
                assert_eq!(disk.volatile_sectors(),0);
                assert_eq!(new.blk.as_ref().unwrap().stats.writes,0);
                (new.exit_witness(),disk.digest())
            };
            let a=run(1009);
            assert_eq!(a,run(1009));
            let b=run(1010);
            assert_eq!(a.0,b.0);
            if fault_model.survival_probability>0.0 {assert_ne!(a.1,b.1);} else {assert_eq!(a.1,b.1);}
        }
    }

    /// A crash keeps exactly the flushed disk writes: the next incarnation
    /// boots with the stable layer, a fresh virtio-blk transport, and none of
    /// the volatile writes. A fork restore puts the whole disk back.
    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn a_reboot_keeps_flushed_disk_writes_only() {
        let kvm=Kvm::new().unwrap();
        let with_disk=|m: &mut Machine| m.blk=Some(VirtioBlk::new(Disk::new(None,Some(8)).unwrap()));
        let mut counter=Counter::new().unwrap();
        let mut old=probe_machine(&kvm,256,false);
        with_disk(&mut old);
        to_halt(&mut old,&mut counter,1_000_000);
        let snap=old.snapshot().unwrap();
        let disk=&mut old.blk.as_mut().unwrap().disk;
        disk.write(0,&[b'A';512]).unwrap();
        disk.flush();
        disk.write(0,&[b'B';512]).unwrap();
        disk.write(1,&[b'C';512]).unwrap();
        let mut new=probe_machine(&kvm,256,false);
        with_disk(&mut new);
        assert_eq!(new.succeed(&mut old,1000,1009,None).unwrap(),Some(2));
        let disk=&new.blk.as_ref().unwrap().disk;
        let mut buf=[0u8;1024];
        disk.read(0,&mut buf).unwrap();
        assert_eq!((buf[0],buf[512]),(b'A',0));
        assert_eq!((disk.stable_sectors(),disk.volatile_sectors()),(1,0));
        assert_eq!(new.blk.as_ref().unwrap().stats.writes,0,"a fresh transport");
        assert_eq!(old.blk.as_ref().unwrap().disk.volatile_sectors(),2,"the old incarnation is untouched");
        old.restore(&snap).unwrap();
        assert_eq!(old.blk.as_ref().unwrap().disk.digest(),Disk::new(None,Some(8)).unwrap().digest());
    }

    #[test]
    #[ignore = "requires Linux x86-64 KVM"]
    fn runtime_checkpoint_restores_pv_fields_and_seeded_syscall_cost() {
        let kvm=Kvm::new().unwrap();
        let mut m=probe_machine(&kvm,0,false);
        m.pv_clock=true;m.syscall_ns=937;m.abi=ABI_PAGE_RESEED;
        for i in 6..14 {m.page_set(i,100+i as u64);}
        let saved=m.snapshot().unwrap();
        m.syscall_ns=1000;for i in 6..14 {m.page_set(i,0);}
        m.restore(&saved).unwrap();
        assert_eq!(m.syscall_ns,937);
        assert_eq!((6..14).map(|i|m.page_get(i)).collect::<Vec<_>>(),(6..14).map(|i|100+i as u64).collect::<Vec<_>>());
        let pv_fields=(6..9).map(|i|m.page_get(i)).collect::<Vec<_>>();
        m.reseed_time_page([42;ENTROPY_WORDS]).unwrap();
        assert_eq!((6..9).map(|i|m.page_get(i)).collect::<Vec<_>>(),pv_fields);
        assert_eq!(m.page_get(9),110);
        assert_eq!((10..14).map(|i|m.page_get(i)).collect::<Vec<_>>(),page_entropy(&[42;ENTROPY_WORDS]));
    }

    #[test]
    #[ignore = "requires KVM"]
    fn runtime_reseed_publishes_page_without_advancing_or_unpausing() {
        let kvm=Kvm::new().unwrap();
        let mut m=probe_machine(&kvm,0,false);
        m.abi=ABI_PAGE_RESEED;
        m.pause().unwrap();
        let saved=m.snapshot().unwrap();
        let before=m.published_entropy();
        m.reseed_time_page([42;ENTROPY_WORDS]).unwrap();
        assert!(m.paused);assert_eq!(m.time,0);assert!(!m.parked);
        assert_eq!(m.page_get(PAGE_ENTROPY_GEN),1);
        let page=page_entropy(&[42;ENTROPY_WORDS]);
        for (i,word) in page.into_iter().enumerate() {assert_eq!(m.page_get(PAGE_ENTROPY+i),word);}
        let branch=m.published_entropy();
        assert_ne!(branch,before);
        m.restore(&saved).unwrap();
        m.reseed_time_page([43;ENTROPY_WORDS]).unwrap();
        assert_ne!(m.published_entropy(),branch);
        m.restore(&saved).unwrap();
        m.reseed_time_page([42;ENTROPY_WORDS]).unwrap();
        assert_eq!(m.published_entropy(),branch);
        m.abi=2;
        assert!(m.reseed_time_page([44;ENTROPY_WORDS]).unwrap_err().contains("requires guest ABI 3"));
        m.abi=0;
        m.reseed_time_page([45;ENTROPY_WORDS]).unwrap();
        assert_eq!(m.page_get(PAGE_ENTROPY_GEN),2,"pre-boot entropy is published too");
    }
}

#[cfg(test)]
mod timed_log_tests {
    use super::*;

    fn output_machine(kvm:&Kvm,undo:u64)->Machine {
        // Two console bytes and one management byte, with no intervening
        // conditional branch: all three must have the same committed time.
        let code=[0xb0,0x00,0xe6,0xe9,0xba,0xf8,0x02,0,0,0xb0,0xff,0xee,
            0xb0,b'A',0xe6,0xe9,0xbb,0xe8,0x03,0,0,0x4b,0x75,0xfd,
            0xb0,b'B',0xe6,0xe9,0xb0,0,0xe6,0xf4];
        let mut kernel=vec![0;1024];kernel[0x1f1]=1;kernel.extend_from_slice(&code);
        let spec=BootSpec {kernel:&kernel,initrd:&[],cmdline:String::new(),mem:INITRD_ADDR+0x100000,rng_seed:[0;32],mac:None,control_channel:false,disk:None,undo_margin:undo,barrier:false,pv_clock:false,page_timer:false};
        let mut m=Machine::new(kvm,"timed",&spec).unwrap();
        let mut r=m.vcpu.get_regs().unwrap();r.rflags=2;m.vcpu.set_regs(&r).unwrap();m.pic.set_masks(0xffff);m
    }

    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn timed_logs_commit_order_and_observation_controls() {
        let kvm=Kvm::new().unwrap();let mut counter=Counter::new().unwrap();let mut expected=None;
        for undo in [0,16,256] {
            for cap in [0,2,65_536] {
                let mut m=output_machine(&kvm,undo);m.com1.chunks.limit=Some(cap);m.com2.chunks.limit=Some(cap);
                let mut frames=Vec::new();
                // Force an exact counted boundary inside the branch loop.
                assert_eq!(m.run_until(&mut counter,500,0,&mut frames).unwrap(),Stop::Reached);
                assert_eq!(m.com1.out,b"\0A");assert_eq!(m.com2.out,b"\xff");
                if cap>0 {
                    assert_eq!(m.com1.chunks.records,vec![[0,0,0,0,1],[0,0,2,1,1]]);
                    assert_eq!(m.com2.chunks.records,vec![[0,0,1,0,1]]);
                }
                // Reading metadata and taking a full-state digest are observation only.
                let before=m.exit_witness();if cap>0 {let _=m.state_digest();}assert_eq!(before,m.exit_witness());
                assert_eq!(m.run_until(&mut counter,2000,0,&mut frames).unwrap(),Stop::Exited);
                assert_eq!(m.com1.out,b"\0AB");assert_eq!(m.com2.out,b"\xff");
                if cap>2 {assert_eq!(m.com1.chunks.records[2],[1000,0,3,2,1]);}
                let evidence=(m.time,m.exit_witness(),m.state_digest().0,m.com1.out.clone(),m.com2.out.clone());
                if let Some(want)=&expected {assert_eq!(&evidence,want,"undo={undo} cap={cap}");} else {expected=Some(evidence);}
            }
        }
    }

    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn timed_logs_survive_machine_incarnations() {
        let kvm=Kvm::new().unwrap();let mut counter=Counter::new().unwrap();
        let mut old=output_machine(&kvm,256);let mut frames=Vec::new();
        assert_eq!(old.run_until(&mut counter,2000,0,&mut frames).unwrap(),Stop::Exited);
        let mut new=output_machine(&kvm,256);new.succeed(&mut old,5000,1009,None).unwrap();new.boot_at(5000).unwrap();
        assert_eq!(new.run_until(&mut counter,7000,0,&mut frames).unwrap(),Stop::Exited);
        assert_eq!(new.com1.out,b"\0AB\0AB");assert_eq!(new.com2.out,b"\xff\xff");
        assert_eq!(new.com1.chunks.records,vec![[0,0,0,0,1],[0,0,2,1,2],[5000,1,3,3,1],[5000,1,5,4,2]]);
        assert_eq!(new.com2.chunks.records,vec![[0,0,1,0,1],[5000,1,4,1,1]]);
    }

    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn runtime_checkpoint_restores_timed_log_cursor_and_order() {
        let kvm=Kvm::new().unwrap();let mut counter=Counter::new().unwrap();
        let mut m=output_machine(&kvm,256);let mut frames=Vec::new();
        m.run_until(&mut counter,500,0,&mut frames).unwrap();
        let saved=m.snapshot().unwrap();
        m.run_until(&mut counter,2000,0,&mut frames).unwrap();
        let want=(m.com1.out.clone(),m.com2.out.clone(),m.com1.chunks.records.clone(),m.com2.chunks.records.clone(),m.log_order);
        for _ in 0..2 {
            let mut fresh=output_machine(&kvm,256);fresh.restore(&saved).unwrap();
            assert_eq!(fresh.com1.chunks.seen,saved.com1.chunks.seen);
            assert_eq!(fresh.com2.chunks.seen,saved.com2.chunks.seen);
            fresh.run_until(&mut counter,2000,0,&mut frames).unwrap();
            assert_eq!((fresh.com1.out.clone(),fresh.com2.out.clone(),fresh.com1.chunks.records.clone(),fresh.com2.chunks.records.clone(),fresh.log_order),want);
        }
    }

    // Inject a judge decision at a real KVM output exit to exercise the commit
    // gate deterministically. This is not evidence of actual PMU skid behavior.
    struct RejectOnce {counter:Counter,rollback:bool,first:bool}
    impl Timebase for RejectOnce {
        const BRANCHES:bool=true;
        fn prepare(&mut self,m:&mut Machine,remaining:u64,soft:u64,legal:bool)->Result<Prepared,String> {
            let prepared=self.counter.prepare(m,remaining,soft,legal)?;
            if self.first && self.rollback {m.checkpoint()?;}
            Ok(prepared)
        }
        fn start(&mut self,syscalls:u64)->u64 {self.counter.start(syscalls)}
        fn read(&mut self,_:u64)->u64 {if self.first && !self.rollback {self.counter.read()+1_000_000} else {self.counter.read()}}
        fn inject_guard(&self)->u64 {INJECT_GUARD}
        fn judge(&mut self,j:Judge<'_>,run:&Run)->Result<Verdict,String> {
            if self.first {self.first=false;return Ok(if self.rollback {assert!(j.undo_valid);Verdict::Rollback} else {Verdict::Overshoot});}
            self.counter.judge(j,run)
        }
    }
    #[test]
    #[ignore = "requires exclusive CPU/SMT lease, Zen MSR workaround and KVM"]
    fn timed_logs_commit_gate_rejection_and_rollback() {
        let kvm=Kvm::new().unwrap();
        for rollback in [false,true] {
            let mut m=output_machine(&kvm,256);
            let mut tb=RejectOnce {counter:Counter::new().unwrap(),rollback,first:true};
            let mut frames=Vec::new();let result=m.run_until(&mut tb,2000,0,&mut frames);
            if rollback {
                assert_eq!(result.unwrap(),Stop::Exited);
                assert!(m.undo.as_ref().unwrap().stats.rollbacks>0);
                assert_eq!(m.com1.out,b"\0AB");assert_eq!(m.com1.chunks.records,vec![[0,0,0,0,1],[0,0,2,1,2]]);
                assert_eq!(m.com1.chunks.records[0],[0,0,0,0,1]);
            } else {
                assert!(result.unwrap_err().contains("overshoot"));
                assert!(m.com1.out.is_empty());assert!(m.com1.chunks.records.is_empty());
                assert_eq!(m.log_order,0);
            }
        }
    }
}

#[cfg(test)]
mod committed_trigger_tests {
    use super::*;
    /// Force a real KVM UART candidate through the actual rollback branch.
    /// This is a commitment control, not a hardware counter qualification.
    struct DiscardFirst {prepared:bool,discarded:bool}
    impl Timebase for DiscardFirst {
        const BRANCHES:bool=true;
        fn prepare(&mut self,m:&mut Machine,_:u64,_:u64,_:bool)->Result<Prepared,String> {
            if !self.prepared {m.checkpoint()?;self.prepared=true;}
            Ok(Prepared::Run)
        }
        fn start(&mut self,_:u64)->u64 {0}
        fn read(&mut self,_:u64)->u64 {0}
        fn inject_guard(&self)->u64 {INJECT_GUARD}
        fn judge(&mut self,_:Judge<'_>,_:&Run)->Result<Verdict,String> {
            if !self.discarded {self.discarded=true;Ok(Verdict::Rollback)} else {Ok(Verdict::Advance{legal:true})}
        }
    }
    #[test]
    #[ignore = "requires KVM"]
    fn committed_console_discards_rolled_back_candidate_before_matcher_feed() {
        let kvm=Kvm::new().unwrap();
        let mut kernel=vec![0;1024];kernel[0x1f1]=1;kernel.extend_from_slice(&[0xb0,b'A',0xe6,0xe9,0xf4]);
        let spec=BootSpec {kernel:&kernel,initrd:&[],cmdline:String::new(),mem:INITRD_ADDR+0x100000,rng_seed:[0;32],mac:None,control_channel:false,disk:None,undo_margin:256,barrier:false,pv_clock:false,page_timer:false};
        let mut m=Machine::new(&kvm,"source",&spec).unwrap();m.observe_console=true;
        let mut tb=DiscardFirst{prepared:false,discarded:false};let mut frames=Vec::new();
        assert_eq!(m.run_until(&mut tb,10,0,&mut frames).unwrap(),Stop::Control);
        assert!(tb.discarded);assert_eq!(m.undo.as_ref().unwrap().stats.rollbacks,1);
        assert_eq!(m.com1.out,b"A");assert_eq!(m.committed_console.take(),Some((0,b'A')));
        // A trigger stop must retain exactly the accepted byte's timed chunk
        // and invalidate the checkpoint before returning control to the world.
        assert!(!m.undo.as_ref().unwrap().valid);
        assert_eq!(m.com1.chunks.records,vec![[0,0,0,0,1]]);assert_eq!(m.log_order,1);
        assert_eq!(m.run_until(&mut tb,10,0,&mut frames).unwrap(),Stop::Exited);
        assert_eq!(m.committed_console,None);assert_eq!(m.com1.out,b"A");
        assert_eq!(m.com1.chunks.records,vec![[0,0,0,0,1]]);assert_eq!(m.log_order,1);
    }
}

#[cfg(test)]
mod page_timer_tests {
    use super::*;

    #[test]
    fn page_deadline_is_picked_up_once_and_sentinel_disarms() {
        let mut page = PageTimer { enabled: true, seen: TIMER_DISARMED };
        let mut pit = Pit::default();
        pit.write(0x43, 0x38, 0);
        page.pickup(12345, &mut pit, 0).unwrap();
        assert_eq!(pit.deadline, Some(12345));
        assert!(!pit.expire(12344).unwrap());
        assert!(pit.expire(12345).unwrap());
        page.pickup(12345, &mut pit, 0).unwrap();
        assert_eq!(pit.deadline, None, "an expired word must not rearm");
        page.pickup(20000, &mut pit, 0).unwrap();
        page.pickup(TIMER_DISARMED, &mut pit, 0).unwrap();
        assert_eq!(pit.deadline, None);
        assert!(!pit.expire(u64::MAX).unwrap());
    }

    #[test]
    fn page_and_port_arms_share_expiration_and_latency_draws() {
        let mut port = Pit::default();
        port.write(0x43, 0x38, 0);
        port.program(1193, 100);
        let word = port.deadline.unwrap();
        let mut pit = Pit::default();
        let mut page = PageTimer { enabled: true, seen: TIMER_DISARMED };
        page.pickup(word, &mut pit, 0).unwrap();
        let mut a = crate::rng::Rng::new(4000, "timer/test");
        let mut b = crate::rng::Rng::new(4000, "timer/test");
        let (mut exact_a, mut exact_b) = (false, false);
        timer_latency(&mut port, &mut exact_a, Some(&mut a), 1000000, 500000);
        timer_latency(&mut pit, &mut exact_b, Some(&mut b), 1000000, 500000);
        assert_eq!(pit.deadline, port.deadline);
        assert_eq!(exact_a, exact_b);
        assert_eq!(a.below(1000000), b.below(1000000));
        let due = port.deadline.unwrap();
        for now in [due - 1, due, due + 1] {
            assert_eq!(pit.expire_through(now).unwrap(), port.expire_through(now).unwrap());
            page.pickup(word, &mut pit, 0).unwrap();
            assert_eq!(pit.deadline, port.deadline);
        }
    }

    #[test]
    fn a_page_rearm_preserves_an_overdue_interrupt() {
        let mut page = PageTimer { enabled: true, seen: TIMER_DISARMED };
        let mut pit = Pit::default();
        assert!(!page.pickup(100, &mut pit, 0).unwrap());
        assert!(page.pickup(200, &mut pit, 100).unwrap());
        assert_eq!(pit.deadline, Some(200));
        assert!(!page.pickup(200, &mut pit, 100).unwrap());
    }

    #[test]
    fn disabled_page_timer_ignores_page_words_and_preserves_port_arms() {
        let mut page = PageTimer::new();
        let mut pit = Pit::default();
        pit.program(100, 0);
        let deadline = pit.deadline;
        for word in [0, 1, TIMER_DISARMED] {
            page.pickup(word, &mut pit, 0).unwrap();
            assert_eq!(pit.deadline, deadline);
        }
    }
}
