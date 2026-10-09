//! One x86-64 Linux machine: a KVM VM with one vCPU, run only by the world
//! thread. Its timebase (see `timebase.rs`) says how guest progress becomes
//! virtual time: retired conditional branches with exact counted stops
//! (precise preemption), or the PV clock, where time moves only at exits.

use crate::boundary::{advance_time, hard_limit, io_charge, PendingIo};
#[cfg(test)]
use crate::counter::Counter;
use crate::devices::{rtc_read, Pic, Pit, Uart};
use crate::disk::Disk;
use crate::profile::CounterProfile;
use crate::stepper::Planner;
use crate::timebase::{Judge, Prepared, Run, Timebase, Verdict};
use crate::undo::{Undo, UndoStats, VcpuState};
use crate::virtio_blk::VirtioBlk;
use crate::virtio_console::VirtioConsole;
use crate::virtio_net::{ReceiveOutcome, VirtioNet};
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
/// Guest physical layout at boot (Linux x86 boot protocol).
const ZERO_PAGE: usize = 0x10000;
const CMDLINE_ADDR: usize = 0x20000;
const SETUP_DATA_ADDR: usize = 0x30000;
const BOOT_STACK: u64 = 0x90000;
const KERNEL_ADDR: usize = 0x10_0000;
const INITRD_ADDR: usize = 0x0800_0000;
/// Pending interrupts are injected only when the next stop is further away
/// than this. Part of the execution semantics: changing it changes runs.
pub const INJECT_GUARD: u64 = 65536;
/// Time page fields (u64 index), see kernel/seedbox.h.
/// Virtual time the VMM publishes at each deterministic point.
const PAGE_TSC: usize = 0;
/// The guest clock: the last time the guest kernel read.
const PAGE_LAST: usize = 1;
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
    fn new() -> Self {
        Self {
            enabled: false,
            seen: TIMER_DISARMED,
        }
    }

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
fn timer_latency(
    pit: &mut Pit,
    exact: &mut bool,
    rng: Option<&mut crate::rng::Rng>,
    jitter: u64,
    ppm: u64,
) {
    if pit.fresh {
        pit.fresh = false;
        if let (Some(d), Some(rng)) = (pit.deadline, rng) {
            if jitter > 0 {
                pit.deadline = Some(d + rng.below(jitter + 1));
            }
            *exact = ppm > 0 && rng.below(1_000_000) < ppm;
        }
    }
}

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

/// I/O ports of the guest kernel's time page protocol (kernel/seedbox.h).
const PORT_TIME_LO: u16 = 0x520;
const PORT_TIME_HI: u16 = 0x524;
const PORT_TIME_READY: u16 = 0x528;
const PORT_YIELD: u16 = 0x52c;
pub(crate) const PORT_TIMER: u16 = 0x530;
/// Supervisor ports the guest init writes (guest/io.h, guest/control.h).
const PORT_EXIT: u16 = 0xf4;
const PORT_INIT_ERROR: u16 = 0xf5;
const PORT_EFFECT: u16 = 0xf6;
const PORT_FAILURE: u16 = 0xf7;
/// Port of the launch barrier, see guest/barrier.h.
const BARRIER_PORT: u16 = 0xf8;
/// Bochs-style debug console, an alias for COM1 output.
const PORT_DEBUGCON: u16 = 0xe9;
/// Legacy PC devices.
const COM1: u16 = 0x3f8;
const COM1_LAST: u16 = COM1 + 7;
const COM2: u16 = 0x2f8;
const COM2_LAST: u16 = COM2 + 7;
const RTC_INDEX: u16 = 0x70;
const RTC_DATA: u16 = 0x71;
/// Legacy PC interrupt lines.
const TIMER_IRQ: u8 = 0;
const COM2_IRQ: u8 = 3;
const COM1_IRQ: u8 = 4;
/// KVM_INTERRUPT (linux/kvm.h): `_IOW(KVMIO, 0x86, struct kvm_interrupt)`;
/// kvm-ioctls has no x86 wrapper for it.
const KVM_INTERRUPT: libc::c_ulong = 0x4004_ae86;
pub const ENTROPY_WORDS: usize = 8;

/// Everything about one machine that a world fork puts back: vCPU, RAM, the
/// shared page, device models and the VMM's per-machine scalars.
pub struct MachineSnapshot {
    world: MachineState,
    ram: Mapping,
    pending_io: Option<PendingIo>,
    timer_rng: Option<crate::rng::Rng>,
    entropy: [u32; ENTROPY_WORDS],
    entropy_read: usize,
    page: Box<[u8; 4096]>,
    cpu: VcpuState,
    branches: u64,
    tsc_latch: u64,
    guest_exit_code: Option<u8>,
    guest_init_error: Option<u8>,
    pic: Pic,
    pit: Pit,
    rtc_index: u8,
    com1: Uart,
    com2: Uart,
    log_order: u64,
    management_framer: crate::control::ManagementFramer,
    net: Option<VirtioNet>,
    channel: Option<VirtioConsole>,
    blk: Option<VirtioBlk>,
    control_dma_bytes: u64,
    control_tx_bytes: u64,
    pv: bool,
    syscall_ns: u64,
    throttle_permille: Option<u64>,
    abi: u64,
    page_timer: PageTimer,
    armed_wake: u64,
    pit_exact: bool,
    margin: u64,
    fm: u64,
    stats: Stats,
    witness: Sha256,
    undo_stats: Option<UndoStats>,
}

use seedbox_engine_core::world::MachineState;
pub use seedbox_engine_core::world::{Reseed, State, Stop};

/// The time page's entropy fields: the seed's 32 bytes, little-endian.
fn page_entropy(entropy: &[u32; ENTROPY_WORDS]) -> [u64; PAGE_ENTROPY_FIELDS] {
    std::array::from_fn(|i| entropy[2 * i] as u64 | (entropy[2 * i + 1] as u64) << 32)
}

/// An anonymous private mapping, unmapped on drop. Its owner is its only
/// user, so moving the owner to another thread moves the mapping with it.
pub(crate) struct Mapping {
    ptr: *mut u8,
    size: usize,
}

// SAFETY: the mapping is reached only through its owner (`&`/`&mut` borrows
// of the `Mapping`), never shared, so it moves between threads with it. Only
// owning fields are `Mapping`s, so any other non-`Send` field added to a
// machine is still caught by the compiler.
unsafe impl Send for Mapping {}

impl Mapping {
    /// `size` zeroed bytes, reserved lazily.
    pub(crate) fn new(size: usize) -> Result<Self, String> {
        // SAFETY: a fresh anonymous mapping aliases no Rust object.
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
        Ok(Self {
            ptr: p.cast(),
            size,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.size
    }

    /// The host address, for registering the mapping with KVM.
    fn host_address(&self) -> u64 {
        self.ptr as u64
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` maps `size` readable bytes for the life of `self`,
        // and the vCPU never runs while the VMM holds a borrow of guest memory.
        unsafe { std::slice::from_raw_parts(self.ptr, self.size) }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as in `as_slice`; `&mut self` makes the borrow unique.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`size` came from `mmap` in `new` and nothing borrows
        // the mapping once its owner drops it.
        unsafe { libc::munmap(self.ptr.cast(), self.size) };
    }
}

/// The time page shared with the guest kernel: u64 words (kernel/seedbox.h)
/// that both the guest and the VMM read and write.
struct TimePage(Mapping);

impl TimePage {
    const BYTES: usize = 4096;

    fn new() -> Result<Self, String> {
        Mapping::new(Self::BYTES).map(Self)
    }

    fn get(&self, i: usize) -> u64 {
        assert!(i < Self::BYTES / 8);
        // SAFETY: in bounds of the page-aligned mapping. Volatile: the guest
        // also writes the page.
        unsafe { std::ptr::read_volatile(self.0.ptr.cast::<u64>().add(i)) }
    }

    fn set(&mut self, i: usize, v: u64) {
        assert!(i < Self::BYTES / 8);
        // SAFETY: as in `get`.
        unsafe { std::ptr::write_volatile(self.0.ptr.cast::<u64>().add(i), v) }
    }

    fn copy(&self) -> Box<[u8; Self::BYTES]> {
        let mut page = Box::new([0u8; Self::BYTES]);
        page.copy_from_slice(self.0.as_slice());
        page
    }

    fn load(&mut self, bytes: &[u8; Self::BYTES]) {
        self.0.as_mut_slice().copy_from_slice(bytes);
    }
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
    /// Host time in KVM_RUN and run count, by `ExitKind`.
    pub kvm_ns: [u128; 8],
    pub kvm_n: [u64; 8],
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

/// How a KVM_RUN ended, for `Stats::kvm_ns` and `Stats::kvm_n`.
#[derive(Clone, Copy)]
pub enum ExitKind {
    Eintr,
    Debug,
    Pio,
    Mmio,
    Hlt,
    Window,
    Other,
}

impl ExitKind {
    pub const NAMES: [&'static str; 7] =
        ["eintr", "debug", "pio", "mmio", "hlt", "window", "other"];

    fn of(exit: &Result<VcpuExit<'_>, kvm_ioctls::Error>) -> Self {
        match exit {
            Err(_) => Self::Eintr,
            Ok(VcpuExit::Debug(_)) => Self::Debug,
            Ok(VcpuExit::IoOut(..)) | Ok(VcpuExit::IoIn(..)) => Self::Pio,
            Ok(VcpuExit::MmioRead(..)) | Ok(VcpuExit::MmioWrite(..)) => Self::Mmio,
            Ok(VcpuExit::Hlt) => Self::Hlt,
            Ok(VcpuExit::IrqWindowOpen) => Self::Window,
            _ => Self::Other,
        }
    }
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
    pub world: MachineState,
    profile: &'static CounterProfile,
    _vm: VmFd,
    vcpu: VcpuFd,
    /// Guest RAM, from guest physical address 0.
    mem: Mapping,
    page: TimePage,
    pub branches: u64,
    pub pending_io: Option<PendingIo>,
    tsc_latch: u64,
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
    /// A throttle fault's factor on `syscall_ns`, in thousandths. `None`
    /// until the first throttle, so unthrottled runs charge exactly as before.
    pub throttle_permille: Option<u64>,
    /// PV watchdog and spin-probe state. It belongs to the machine, not the
    /// thread's clock, so it never carries between machines.
    pub(crate) pv_progress: pv::Progress,
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
    /// The initial guest uses the launch-barrier boot protocol.
    barrier: bool,
    /// Per-seed words the guest reads after its release.
    entropy: [u32; ENTROPY_WORDS],
    entropy_read: usize,
    pub(crate) pv_clock: bool,
    page_timer_requested: bool,
    /// The MSR list a fork snapshot saves.
    msrs: Msrs,
    pub stats: Stats,
    pub witness: Sha256,
    pub trace: Option<std::io::BufWriter<std::fs::File>>,
}

impl std::ops::Deref for Machine {
    type Target = MachineState;
    fn deref(&self) -> &MachineState {
        &self.world
    }
}
impl std::ops::DerefMut for Machine {
    fn deref_mut(&mut self) -> &mut MachineState {
        &mut self.world
    }
}

fn put(m: &mut [u8], at: usize, v: u64, n: usize) {
    m[at..at + n].copy_from_slice(&v.to_le_bytes()[..n]);
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
        // The PV clock needs no counter, so no host PMU; the
        // profile is then never consulted.
        let profile = if spec.pv_clock {
            &crate::profile::ZEN
        } else {
            crate::profile::host()?
        };
        if !kvm.check_extension(kvm_ioctls::Cap::ImmediateExit) {
            return Err("KVM immediate-exit capability required for I/O completion".into());
        }
        let mem_size = spec.mem;
        let k = spec.kernel;
        // The bzImage boot header: setup sectors, then the protected-mode kernel.
        let setup = k
            .get(0x1f1)
            .map(|&sects| (sects as usize + 1) * 512)
            .filter(|&setup| k.len() >= setup.max(0x290))
            .ok_or("kernel: not a bzImage")?;
        if INITRD_ADDR + spec.initrd.len() > mem_size {
            return Err("initrd does not fit".into());
        }
        if KERNEL_ADDR + (k.len() - setup) > mem_size {
            return Err("kernel does not fit".into());
        }
        let mut mem = Mapping::new(mem_size)?;
        let mut page = TimePage::new()?;
        if spec.pv_clock {
            page.set(PAGE_PV_CLOCK, 1);
        }
        let m = mem.as_mut_slice();
        m[KERNEL_ADDR..KERNEL_ADDR + k.len() - setup].copy_from_slice(&k[setup..]);
        let zp = ZERO_PAGE;
        m[zp + 0x1f1..zp + 0x290].copy_from_slice(&k[0x1f1..0x290]);
        put(m, zp + 0x210, 0xff, 1); // type_of_loader
        put(m, zp + 0x211, 0x01, 1); // loadflags: LOADED_HIGH
        put(m, zp + 0x218, INITRD_ADDR as u64, 4);
        put(m, zp + 0x21c, spec.initrd.len() as u64, 4);
        put(m, zp + 0x228, CMDLINE_ADDR as u64, 4);
        put(m, zp + 0x1e8, 2, 1); // e820 entries
        for (i, (addr, size)) in [
            (0u64, 0x9fc00u64),
            (KERNEL_ADDR as u64, (mem_size - KERNEL_ADDR) as u64),
        ]
        .iter()
        .enumerate()
        {
            let at = zp + 0x2d0 + i * 20;
            put(m, at, *addr, 8);
            put(m, at + 8, *size, 8);
            put(m, at + 16, 1, 4);
        }
        put(m, zp + 0x250, SETUP_DATA_ADDR as u64, 8);
        put(m, SETUP_DATA_ADDR + 8, 9, 4); // SETUP_RNG_SEED
        put(m, SETUP_DATA_ADDR + 12, 32, 4);
        m[SETUP_DATA_ADDR + 16..SETUP_DATA_ADDR + 48].copy_from_slice(&spec.rng_seed);
        let cmd = format!("{}\0", spec.cmdline);
        m[CMDLINE_ADDR..CMDLINE_ADDR + cmd.len()].copy_from_slice(cmd.as_bytes());
        m[INITRD_ADDR..INITRD_ADDR + spec.initrd.len()].copy_from_slice(spec.initrd);

        let vm = kvm.create_vm().map_err(|e| format!("create_vm: {e}"))?;
        // SAFETY: both regions are mappings this machine owns; it keeps them
        // until after the VM and vCPU file descriptors are closed (field order).
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
                userspace_addr: mem.host_address(),
            })
            .map_err(|e| format!("ram slot: {e}"))?;
            vm.set_user_memory_region(kvm_userspace_memory_region {
                slot: 1,
                flags: 0,
                guest_phys_addr: TIME_PAGE,
                memory_size: TimePage::BYTES as u64,
                userspace_addr: page.0.host_address(),
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
            rip: KERNEL_ADDR as u64,
            rsi: zp as u64,
            rsp: BOOT_STACK,
            rflags: 2,
            ..Default::default()
        })
        .map_err(|e| e.to_string())?;
        let net = spec.mac.map(VirtioNet::new);
        let msrs = crate::undo::msr_list(kvm, &vcpu)?;
        let undo = if spec.undo_margin > 0 {
            let mut u = Undo::new(kvm, &vcpu, mem_size)?;
            // Everything loaded so far was written by the VMM.
            u.mark(0, KERNEL_ADDR);
            u.mark(KERNEL_ADDR, k.len() - setup);
            u.mark(INITRD_ADDR, spec.initrd.len());
            Some(Box::new(u))
        } else {
            None
        };
        Ok(Self {
            world: MachineState::new(name.to_string()),
            profile,
            _vm: vm,
            vcpu,
            mem,
            page,
            branches: 0,
            pending_io: None,
            tsc_latch: 0,
            guest_exit_code: None,
            guest_init_error: None,
            pic: Pic::new(),
            pit: Pit::default(),
            rtc_index: 0,
            com1: Uart::default(),
            com2: Uart::default(),
            log_order: 0,
            management_framer: crate::control::ManagementFramer::default(),
            net,
            channel: spec.control_channel.then(VirtioConsole::new),
            blk: spec.disk.clone().map(VirtioBlk::new),
            control_dma_bytes: 0,
            control_tx_bytes: 0,
            stepping: false,
            sync_regs_ready: false,
            #[cfg(test)]
            require_io_checkpoint: false,
            planner: None,
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
            throttle_permille: None,
            pv_progress: pv::Progress::new(),
            timer_rng: None,
            margin: profile.margin,
            adaptive_margin: false,
            step_started: None,
            undo,
            undo_margin: spec.undo_margin,
            fm: spec.undo_margin,
            barrier: spec.barrier,
            entropy: [0; ENTROPY_WORDS],
            entropy_read: 0,
            pv_clock: spec.pv_clock,
            page_timer_requested: spec.page_timer,
            msrs,
            stats: Stats::default(),
            // A PV clock run can never share a witness with a counter run.
            witness: if spec.pv_clock {
                Sha256::new_with_prefix(b"timebase=pv")
            } else {
                Sha256::new()
            },
            trace: None,
        })
    }

    /// Guest ABI announced on the time page port, 0 before the kernel has.
    pub fn abi(&self) -> u64 {
        self.abi
    }

    pub fn memory(&mut self) -> &mut [u8] {
        self.mem.as_mut_slice()
    }

    fn record(&mut self, kind: u8, extra: u64) {
        if let Some(t) = self.trace.as_mut() {
            use std::io::Write;
            let rip = self.vcpu.get_regs().map(|r| r.rip).unwrap_or(0);
            let _ = writeln!(
                t,
                "{} {} {} {:x} {:x}",
                kind as char, self.branches, self.world.time, extra, rip
            );
        }
        self.witness.update([kind]);
        self.witness.update(self.branches.to_le_bytes());
        self.witness.update(self.world.time.to_le_bytes());
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
        self.world.begin_release()?;
        // The parked interval passes like idle time: a timer that fell due
        // meanwhile fires now, late, so the armed deadline is ahead of the
        // clock again before the next slice computes its hard stop.
        self.advance_to(at, Cause::Idle);
        self.expire_timers()?;
        self.entropy = entropy;
        self.entropy_read = 0;
        self.record(b'R', self.world.time);
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
            return Err(format!("{}: runtime checkpoint reseeding requires guest ABI 3; rebuild the kernel from kernel/", self.world.name));
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
    pub fn published_entropy(&self) -> [u64; 5] {
        std::array::from_fn(|i| self.page_get(PAGE_ENTROPY_GEN + i))
    }

    /// Freeze the machine at a world boundary. Nothing about the vCPU changes:
    /// an exit KVM still has to complete, and the I/O cost owed for it, stay
    /// exactly as they are until the machine resumes.
    pub fn pause(&mut self) -> Result<(), String> {
        self.world.pause()?;
        self.record(b'Z', 0);
        Ok(())
    }

    /// Continue a paused machine at virtual time `at`. The paused interval
    /// passes like idle time, as at a release: timers that fell due fire now,
    /// late and coalesced, keeping their phase.
    pub fn resume(&mut self, at: u64) -> Result<(), String> {
        self.world.resume(at)?;
        self.expire_timers()?;
        self.record(b'U', self.world.time);
        Ok(())
    }

    /// Take a machine built for a reboot to its first instruction at `at`.
    pub fn boot_at(&mut self, at: u64) -> Result<(), String> {
        self.world.boot_at(at)?;
        self.record(b'N', self.world.incarnation as u64);
        Ok(())
    }

    /// Carry what outlives a crash from the previous incarnation into this
    /// one, which is built but not booted: the console and management logs
    /// and their sinks, the trace, and the event witness, so the final
    /// witness covers every incarnation. Nothing else crosses: RAM, device
    /// queues, KVM's I/O state and the undo checkpoint die with the VM.
    /// Returns the number of volatile sectors lost, or None without a disk.
    pub fn succeed(
        &mut self,
        old: &mut Machine,
        at: u64,
        seed: u64,
        identity: Option<&str>,
    ) -> Result<Option<u64>, String> {
        self.world.succeed(&old.world, at)?;
        for (new, old) in [
            (&mut self.com1, &mut old.com1),
            (&mut self.com2, &mut old.com2),
        ] {
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
            let (disk, lost) =
                blk.disk()
                    .crashed_branch(seed, identity, &old.world.name, old.world.incarnation);
            *new = VirtioBlk::new(disk);
            Some(lost)
        } else {
            None
        };
        self.witness.update(old.exit_witness().as_bytes());
        self.record(b'C', at);
        Ok(lost)
    }

    /// Park the machine at a fork point the world chose (a simulated time),
    /// rather than one the guest announced. It must owe no I/O cost.
    pub fn park(&mut self) -> Result<(), String> {
        self.world.check_park(self.pending_io.is_some())?;
        self.set_stepping(false)?;
        // As at the launch barrier: finish any exit KVM still owes, so a cold
        // run and a restored fork resume from identical vCPU state.
        self.complete_io()?;
        self.world.commit_park();
        self.record(b'P', 0);
        Ok(())
    }

    /// Capture at a committed scheduler boundary, including unpaid I/O cost.
    pub fn snapshot(&mut self) -> Result<MachineSnapshot, String> {
        if self.stepping
            || !self.world.fault_effects.is_empty()
            || !self.world.control_requests.is_empty()
            || self.world.committed_console.is_some()
        {
            return Err(format!("{}: not at a fork point", self.world.name));
        }
        // No architecture probe may survive a committed checkpoint boundary.
        if self.pv_progress.hold_watchdog() {
            return Err("PV checkpoint inside a spin probe".into());
        }
        self.complete_io()?;
        let mut cpu = VcpuState::new(self.msrs.clone());
        cpu.save(&self.vcpu, "fork")?;
        let undo_stats = self.undo.as_ref().map(|u| u.stats.clone());
        let mut ram = Mapping::new(self.mem.len())?;
        ram.as_mut_slice().copy_from_slice(self.mem.as_slice());
        if let Some(u) = self.undo.as_mut() {
            u.fork_point(&self._vm, self.mem.as_slice())?;
        }
        let page = self.page.copy();
        Ok(MachineSnapshot {
            ram,
            page,
            cpu,
            world: self.world.clone(),
            branches: self.branches,
            tsc_latch: self.tsc_latch,
            pending_io: self.pending_io.clone(),
            timer_rng: self.timer_rng.clone(),
            entropy: self.entropy,
            entropy_read: self.entropy_read,
            guest_exit_code: self.guest_exit_code,
            guest_init_error: self.guest_init_error,
            pic: self.pic.clone(),
            pit: self.pit.clone(),
            rtc_index: self.rtc_index,
            com1: self.com1.snapshot(),
            com2: self.com2.snapshot(),
            log_order: self.log_order,
            management_framer: self.management_framer.clone(),
            net: self.net.clone(),
            channel: self.channel.clone(),
            blk: self.blk.clone(),
            control_dma_bytes: self.control_dma_bytes,
            control_tx_bytes: self.control_tx_bytes,
            pv: self.pv,
            syscall_ns: self.syscall_ns,
            throttle_permille: self.throttle_permille,
            abi: self.abi,
            page_timer: self.page_timer,
            armed_wake: self.armed_wake,
            pit_exact: self.pit_exact,
            margin: self.margin,
            fm: self.fm,
            stats: self.stats.clone(),
            witness: self.witness.clone(),
            undo_stats,
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
        // Host-only watchdog and probe scratch starts over at the fork point.
        self.pv_progress = pv::Progress::new();
        let pages = match self.undo.as_mut() {
            // A machine rebuilt for this restore (the run rebooted it) has
            // held no fork point yet: copy everything, then hold one here.
            Some(u) if !u.holds_fork_point() => {
                self.mem.as_mut_slice().copy_from_slice(s.ram.as_slice());
                u.mark(0, self.mem.len());
                u.fork_point(&self._vm, self.mem.as_slice())?;
                u.stats = s.undo_stats.clone().unwrap_or_default();
                (self.mem.len() / 4096) as u64
            }
            Some(u) => {
                let pages =
                    u.return_to_fork(&self._vm, self.mem.as_mut_slice(), s.ram.as_slice())?;
                // Each seed reports its own undo work on top of the boot's.
                u.stats = s.undo_stats.clone().unwrap_or_default();
                pages
            }
            None => {
                self.mem.as_mut_slice().copy_from_slice(s.ram.as_slice());
                (self.mem.len() / 4096) as u64
            }
        };
        self.page.load(&s.page);
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
        self.world = s.world.clone();
        self.branches = s.branches;
        self.tsc_latch = s.tsc_latch;
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
        self.world.clear_effects();
        self.management_framer = s.management_framer.clone();
        self.net = s.net.clone();
        self.channel = s.channel.clone();
        self.blk = s.blk.clone();
        self.control_dma_bytes = s.control_dma_bytes;
        self.control_tx_bytes = s.control_tx_bytes;
        self.pv = s.pv;
        self.syscall_ns = s.syscall_ns;
        self.throttle_permille = s.throttle_permille;
        self.abi = s.abi;
        self.page_timer = s.page_timer;
        self.armed_wake = s.armed_wake;
        self.pit_exact = s.pit_exact;
        self.margin = s.margin;
        self.fm = s.fm;
        self.entropy = s.entropy;
        self.entropy_read = s.entropy_read;
        self.stats = s.stats.clone();
        self.witness = s.witness.clone();
        Ok(pages)
    }

    /// Resume a halted machine at virtual time `t`.
    pub fn wake(&mut self, t: u64) {
        self.advance_to(t, Cause::Idle);
        self.world.wake();
    }

    pub fn deliver_serial(&mut self, bytes: &[u8]) {
        self.com2.rx.extend(bytes);
        if self.com2.edge() {
            self.pic.raise(COM2_IRQ);
        }
    }

    /// Strict runtime delivery requires all bytes DMA into posted guest buffers.
    pub fn deliver_control(&mut self, bytes: &[u8]) -> Result<(), String> {
        let channel = self
            .channel
            .as_mut()
            .ok_or("runtime control channel absent")?;
        let received = channel.receive(self.mem.as_mut_slice(), bytes)?;
        let complete = received.accepted && channel.pending_rx_bytes() == 0;
        self.take_dma();
        if received.irq {
            self.pic.raise(CHANNEL_IRQ);
        }
        if !complete {
            return Err(
                "control transport receive capacity insufficient at exact delivery time".into(),
            );
        }
        self.control_dma_bytes += bytes.len() as u64;
        Ok(())
    }
    fn capture_output(&mut self) {
        for uart in [&mut self.com1, &mut self.com2] {
            uart.chunks.capture(
                uart.out.len() as u64,
                self.world.time,
                self.world.incarnation,
                &mut self.log_order,
            );
        }
    }
    fn channel_output(&mut self, bytes: &[u8]) -> Result<bool, String> {
        if self.com2.output_limit.is_some_and(|limit| {
            self.com2
                .out
                .len()
                .checked_add(bytes.len())
                .is_none_or(|n| n > limit)
        }) {
            return Err("guest control output limit exceeded".into());
        }
        if let Some(file) = self.com2.output_file.as_mut() {
            use std::io::Write;
            file.write_all(bytes)
                .map_err(|e| format!("control log: {e}"))?;
        }
        self.com2.out.extend_from_slice(bytes);
        self.control_tx_bytes += bytes.len() as u64;
        self.capture_output();
        let frames = self.management_framer.receive(bytes)?;
        let stop = !frames.is_empty();
        self.world
            .control_requests
            .extend(frames.into_iter().map(|f| (self.world.time, f)));
        Ok(stop)
    }

    pub fn deliver_frame(&mut self, frame: &[u8]) -> Result<ReceiveOutcome, String> {
        let m = self.mem.as_mut_slice();
        let outcome = self.net.as_mut().map(|n| n.receive(m, frame)).transpose();
        self.take_dma();
        let outcome = outcome?.unwrap_or_default();
        if outcome.irq {
            self.raise_net();
        }
        Ok(outcome)
    }

    fn poll_net(&mut self, frames: &mut Vec<Vec<u8>>, boundary: bool) -> Result<bool, String> {
        let Some(n) = self.net.as_mut() else {
            return Ok(false);
        };
        let out = n.poll_at(self.mem.as_mut_slice(), self.world.time, boundary);
        self.take_dma();
        let out = out?;
        if out.irq {
            self.raise_net();
        }
        let sent = !out.frames.is_empty();
        frames.extend(out.frames);
        Ok(sent)
    }

    /// Move virtual time forward to `t` (never back) for `cause`.
    fn advance_to(&mut self, t: u64, cause: Cause) {
        if t > self.world.time {
            self.stats.time_by[cause as usize] += t - self.world.time;
            self.world.time = t;
        }
    }

    /// Syscalls the guest has entered (counted in the time page).
    pub fn syscalls(&self) -> u64 {
        self.page_get(PAGE_SYSCALLS)
    }

    fn page_get(&self, i: usize) -> u64 {
        self.page.get(i)
    }

    fn page_set(&mut self, i: usize, v: u64) {
        self.page.set(i, v)
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
        if let Some(channel) = self.channel.as_mut() {
            match self.undo.as_mut() {
                Some(undo) => {
                    for (address, length) in channel.dirty.drain(..) {
                        undo.mark(address, length);
                    }
                }
                None => channel.dirty.clear(),
            }
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
        // The exit borrows the vCPU: settle the outcome before resetting it.
        let outcome = match self.vcpu.run() {
            Err(e) if e.errno() == libc::EINTR => Ok(()),
            Err(e) => Err(format!("{}: completing I/O: {e}", self.world.name)),
            Ok(e) => Err(format!(
                "{}: exit while completing I/O: {e:?}",
                self.world.name
            )),
        };
        self.vcpu.set_kvm_immediate_exit(0);
        outcome
    }

    fn pending_io_bytes(&self) -> Vec<u8> {
        self.pending_io
            .as_ref()
            .map(PendingIo::canonical)
            .unwrap_or_else(|| vec![0])
    }

    pub fn exit_witness(&self) -> String {
        let mut h = self.witness.clone();
        h.update(b"pending_io");
        h.update(self.pending_io_bytes());
        format!("{:x}", h.finalize())
    }

    fn note_skew(&mut self, site: Site) {
        let last = self.page_get(PAGE_LAST);
        let k = &mut self.stats.skew[site as usize];
        k.n += 1;
        if last > self.world.time {
            k.ahead += 1;
            k.ahead_max_ns = k.ahead_max_ns.max(last - self.world.time);
        } else if last < self.world.time {
            k.behind += 1;
            k.behind_max_ns = k.behind_max_ns.max(self.world.time - last);
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
        if !self.page_timer.enabled {
            return Ok(());
        }
        if self.page_timer.pickup(
            self.page_get(PAGE_TIMER_DEADLINE),
            &mut self.pit,
            self.world.time,
        )? {
            self.pic.raise(TIMER_IRQ);
        }
        timer_latency(
            &mut self.pit,
            &mut self.pit_exact,
            self.timer_rng.as_mut(),
            self.timer_jitter,
            self.exact_timer_ppm,
        );
        Ok(())
    }

    // Expiration consults adopted device state only, never a guest page store.
    fn expire_timers(&mut self) -> Result<(), String> {
        if self.pit.expire_through(self.world.time)? > 0 {
            self.pic.raise(TIMER_IRQ);
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
            if self.world.time == limit {
                return Ok(false);
            }
            let t = pending.advance(self.world.time, limit, self.pit.deadline)?;
            self.advance_to(t, Cause::Io);
            self.expire_timers()?;
        }
        Ok(true)
    }

    fn checkpoint(&mut self) -> Result<(), String> {
        let t = Instant::now();
        self.complete_io()?;
        let page: [u64; PAGE_FIELDS] = std::array::from_fn(|i| self.page_get(i));
        let timer_deadline = self
            .page_timer
            .enabled
            .then(|| self.page_get(PAGE_TIMER_DEADLINE));
        let u = self.undo.as_mut().unwrap();
        u.timer_deadline = timer_deadline;
        u.save(&self._vm, &self.vcpu, self.mem.as_slice())?;
        u.page = page;
        u.time = self.world.time;
        u.branches = self.branches;
        u.stats.checkpoint_ns += t.elapsed().as_nanos();
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), String> {
        let t = Instant::now();
        self.complete_io()?;
        let u = self.undo.as_mut().unwrap();
        let (regs, sregs) = u.restore(&self._vm, &self.vcpu, self.mem.as_mut_slice())?;
        for i in 0..PAGE_FIELDS {
            self.page.set(i, u.page[i]);
        }
        if let Some(deadline) = u.timer_deadline {
            self.page.set(PAGE_TIMER_DEADLINE, deadline);
        }
        self.world.time = u.time;
        self.branches = u.branches;
        u.stats.rollback_ns += t.elapsed().as_nanos();
        // The planner reads registers from the run structure.
        let run = self.vcpu.get_kvm_run();
        run.s.regs.regs = regs;
        run.s.regs.sregs = sregs;
        self.sync_regs_ready = true;
        Ok(())
    }

    fn checked_page_time(&self, limit: u64) -> Result<u64, String> {
        let last = self.page_get(PAGE_LAST);
        advance_time(self.world.time, last.saturating_sub(self.world.time), limit)
    }

    fn sync_from_page(&mut self, limit: u64) -> Result<(), String> {
        let t = self.checked_page_time(limit)?;
        self.advance_to(t, Cause::ClockReads);
        self.sync_pic();
        Ok(())
    }

    fn publish(&mut self) {
        self.page.set(PAGE_TSC, self.world.time);
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
                let regs = self
                    .vcpu
                    .get_regs()
                    .map_err(|e| format!("initial step registers: {e}"))?;
                let sregs = self
                    .vcpu
                    .get_sregs()
                    .map_err(|e| format!("initial step system registers: {e}"))?;
                let run = self.vcpu.get_kvm_run();
                run.s.regs.regs = regs;
                run.s.regs.sregs = sregs;
                self.sync_regs_ready = true;
            }
            self.planner = Some(Planner::load(&self.vcpu, self.mem.as_slice())?);
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
            // SAFETY: `kvm_sync_regs` is plain data; `sync_regs_ready` (set by
            // `set_stepping` or a successful run) means KVM or the VMM filled it.
            unsafe { (run.s.regs.regs, run.s.regs.sregs) }
        };
        let planner = self.planner.as_mut().unwrap();
        planner.refresh_from(&sr);
        let (targets, capped) = planner
            .plan_capped(self.mem.as_slice(), &r)
            .map_err(|e| format!("{}: step plan: {e}", self.world.name))?;
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
            // SAFETY: `kvm_sync_regs` is plain data; `sync_regs_ready` (set by
            // `set_stepping` or a successful run) means KVM or the VMM filled it.
            (unsafe { run.s.regs.regs.rip }, run.if_flag)
        };
        self.set_stepping(false)?;
        self.stats.spin_idles += 1;
        self.record(b'S', rip);
        if if_flag == 0 {
            return Err(format!(
                "{}: the guest spins with interrupts off at rip {rip:#x}; nothing can end the loop",
                self.world.name
            ));
        }
        self.world.state = State::Halted;
        Ok(Stop::Halted)
    }

    fn inject(&mut self) -> Result<(), String> {
        let run = self.vcpu.get_kvm_run();
        match self.pic.pending() {
            Some(irq) if run.ready_for_interrupt_injection != 0 && run.if_flag != 0 => {
                let vector = self.pic.take(irq);
                // SAFETY: KVM_INTERRUPT reads one kvm_interrupt that outlives the call.
                let rc = unsafe {
                    libc::ioctl(
                        self.vcpu.as_raw_fd(),
                        KVM_INTERRUPT,
                        &kvm_interrupt { irq: vector },
                    )
                };
                if rc != 0 {
                    return Err(format!(
                        "{}: KVM_INTERRUPT: {}",
                        self.world.name,
                        std::io::Error::last_os_error()
                    ));
                }
                self.vcpu.get_kvm_run().request_interrupt_window = 0;
                self.stats.irqs += 1;
                self.record(b'I', vector as u64);
            }
            Some(_) => run.request_interrupt_window = 1,
            None => run.request_interrupt_window = 0,
        }
        Ok(())
    }

    fn serial_edges(&mut self) {
        if self.com1.edge() {
            self.pic.raise(COM1_IRQ);
        }
        if self.com2.edge() {
            self.pic.raise(COM2_IRQ);
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
        self.com1.chunks.finish();
        self.com2.chunks.finish();
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
        let deferred = self.net.as_ref().is_some_and(|n| n.deferred_tx());
        let poll_end = crate::virtio_net::poll_limit(self.world.time, limit, deferred);
        // A new polling deadline is exact. The world's early-stop allowance
        // can exceed TX_POLL_NS; subtracting it here would create 1 ns slices.
        let early = if poll_end < limit { 0 } else { early };
        let limit = poll_end;
        let slice_end = limit
            .saturating_sub(early)
            .max(self.world.time.saturating_add(1))
            .min(limit);
        let mut legal = true;
        loop {
            if self.pending_io.is_some() {
                if !self.drain_io(limit)? {
                    self.set_stepping(false)?;
                    // Charged I/O can split at an exact slice boundary too.
                    if self.poll_net(frames, true)? {
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
                if self.page_timer.enabled {
                    self.expire_timers()?;
                }
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
                if self.poll_net(frames, self.world.time >= slice_end)? {
                    self.note_skew(Site::Sent);
                    self.set_stepping(false)?;
                    return Ok(Stop::Sent);
                }
                if self.world.time >= slice_end {
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
            let remaining = hard - self.world.time;
            // Interrupt injection depends only on the distance to the stop, never
            // on the stepping margin, so it is deterministic.
            if legal {
                if remaining > tb.inject_guard() {
                    self.inject()?;
                } else {
                    self.vcpu.get_kvm_run().request_interrupt_window = 0;
                }
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
                    if let Some(u) = self.undo.as_mut() {
                        u.valid = false;
                    }
                    self.fm = self.undo_margin;
                    self.set_stepping(false)?;
                    let pending = self
                        .net
                        .as_ref()
                        .map(|n| n.pending_tx(self.mem.as_slice()))
                        .transpose()?
                        .unwrap_or(false);
                    if self.poll_net(frames, true)? {
                        self.note_skew(Site::Sent);
                        return Ok(Stop::Sent);
                    }
                    if pending {
                        legal = true;
                        continue;
                    }
                }
                return self.spin_idle();
            }
            self.vcpu.get_kvm_run().kvm_valid_regs =
                (KVM_SYNC_X86_REGS | KVM_SYNC_X86_SREGS) as u64;
            let c0 = tb.start(syscalls0)?;
            let t_run = Instant::now();
            let run_ptr = self.vcpu.get_kvm_run() as *mut kvm_run;
            let exit = self.vcpu.run();
            // Requested validity bits alone are not initialized output. A
            // successful run with both bits set provides the synchronized state.
            if exit.is_ok() {
                self.sync_regs_ready = true;
            }
            let run_ns = t_run.elapsed().as_nanos();
            let kind = ExitKind::of(&exit) as usize;
            self.stats.kvm_ns[kind] += run_ns;
            self.stats.kvm_n[kind] += 1;
            let syscalls1 = self.page.get(PAGE_SYSCALLS);
            let c1 = tb.read(syscalls1)?;
            let delta = c1 - c0;
            let interrupted = matches!(&exit, Err(e) if e.errno() == libc::EINTR);
            let run_syscalls = syscalls1.wrapping_sub(syscalls0);
            let run = Run {
                delta,
                remaining,
                interrupted,
                debug: matches!(exit, Ok(VcpuExit::Debug(_))),
                run_ns,
            };
            let judge = Judge {
                name: &self.world.name,
                stepping: self.stepping,
                undo_valid: self.undo.as_ref().is_some_and(|u| u.valid),
                fm: &mut self.fm,
                margin: &mut self.margin,
                adaptive_margin: self.adaptive_margin,
                armed_wake: self.armed_wake,
                profile: self.profile,
                stats: &mut self.stats,
                pv_progress: &mut self.pv_progress,
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
                        self.world.name,
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
            self.world.time += delta;
            if legal {
                // Sent/Hlt/Exited can return before the next loop synchronizes
                // the page. Validate guest clock progress before exit effects.
                // The exit still borrows vcpu: use only the disjoint fields.
                let last = self.page.get(PAGE_LAST);
                let exit_time =
                    advance_time(self.world.time, last.saturating_sub(self.world.time), hard)?;
                if self.pv_clock || self.page_timer.enabled {
                    // In particular HLT must adopt at its full exit time, not
                    // at a watchdog landing's partial syscall-only time. A
                    // stop that left the guest's clock ahead of ours could be
                    // given a next limit below time the guest already read;
                    // a timer armed here must count from that clock too.
                    self.stats.time_by[Cause::ClockReads as usize] += exit_time - self.world.time;
                    self.world.time = exit_time;
                }
                if self.page_timer.enabled {
                    let word = self.page.get(PAGE_TIMER_DEADLINE);
                    if self
                        .page_timer
                        .pickup(word, &mut self.pit, self.world.time)?
                    {
                        self.pic.raise(TIMER_IRQ);
                    }
                    timer_latency(
                        &mut self.pit,
                        &mut self.pit_exact,
                        self.timer_rng.as_mut(),
                        self.timer_jitter,
                        self.exact_timer_ppm,
                    );
                    if self.pit.expire_through(self.world.time)? > 0 {
                        self.pic.raise(TIMER_IRQ);
                    }
                }
            }
            self.stats.exits += 1;
            if self.stepping {
                self.stats.steps += 1;
            }
            let now = self.world.time;
            match exit {
                Err(e) if e.errno() == libc::EINTR => {
                    self.stats.eintr += 1;
                }
                Ok(VcpuExit::Debug(_)) => {}
                Ok(VcpuExit::IoOut(PORT_YIELD, data)) => {
                    // Requested yield: a stop if we reached the soft target,
                    // otherwise transparent (nothing observable happens).
                    // PV clock only: 1 means the guest's clock would pass
                    // the bound we published (`soft`); it waits for time to
                    // move, so move it there.
                    let v = data[0];
                    if v == 1 && self.pv_clock {
                        let bound = if self.page_timer.enabled {
                            self.page_get(PAGE_BOUND).min(soft)
                        } else {
                            soft
                        };
                        self.advance_to(bound, Cause::ClockBound);
                    }
                    self.stats.yields += 1;
                    if self.world.time >= soft {
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
                    advance_time(self.world.time, charge, u64::MAX)?;
                    // SAFETY: `run_ptr` is the vCPU's live kvm_run mapping, and
                    // this is a KVM_EXIT_IO exit, so the `io` union member is set.
                    let descriptor = unsafe { (*run_ptr).__bindgen_anon_1.io };
                    let pending = if charge > 0 {
                        Some(PendingIo::new(
                            self.world.time,
                            port,
                            true,
                            descriptor.size,
                            descriptor.count,
                            data.to_vec(),
                        )?)
                    } else {
                        None
                    };
                    #[cfg(test)]
                    if charge > 0 && self.require_io_checkpoint {
                        assert!(
                            self.undo.as_ref().is_some_and(|u| u.valid),
                            "charged access must begin with the real held checkpoint"
                        );
                    }
                    let after_io = self.world.time;
                    let mut control_stop = false;
                    let console_before = self.com1.out.len();
                    match port {
                        COM1..=COM1_LAST => {
                            if port == COM1 && v == b'\n' && crate::console_trace_enabled() {
                                let line_start = self
                                    .com1
                                    .out
                                    .iter()
                                    .rposition(|&b| b == b'\n')
                                    .map(|p| p + 1)
                                    .unwrap_or(0);
                                crate::emit_console_trace(
                                    self.world.time,
                                    self.branches,
                                    &self.com1.out[line_start..],
                                );
                            }
                            self.com1.write(port - COM1, v)?
                        }
                        COM2..=COM2_LAST => {
                            if self.channel.is_some() {
                                self.com2.write(port - COM2, v)?;
                            } else if let Some(line) = crate::management_uart::write(
                                &mut self.management_framer,
                                &mut self.com2,
                                port - COM2,
                                v,
                            )? {
                                self.world.control_requests.push((after_io, line));
                                control_stop = true;
                            }
                        }
                        PORT_EFFECT | PORT_FAILURE => {
                            let id = u32::from_le_bytes(
                                data.try_into()
                                    .map_err(|_| "fault effect must be a 32-bit I/O write")?,
                            );
                            self.world
                                .fault_effects
                                .push((id, after_io, port == PORT_EFFECT));
                            control_stop = true;
                        }
                        0x20 | 0x21 | 0xa0 | 0xa1 => {
                            self.pic.write(port, v);
                            if self.pv {
                                let m = self.pic.masks() as u64;
                                self.page_set(PAGE_PIC_IMR, m);
                            }
                        }
                        PORT_TIME_READY => {
                            self.abi = v as u64;
                            if self.abi < ABI_PAGE_RESEED && self.page_get(PAGE_ENTROPY_GEN) != 0 {
                                return Err(format!(
                                    "{}: checkpoint entropy requires guest ABI 3",
                                    self.world.name
                                ));
                            }
                            if self.pv_clock && self.page_get(PAGE_PV_CLOCK) != 2 {
                                return Err(format!(
                                    "{}: the guest kernel lacks PV clock support; rebuild it from kernel/ for the pv timebase",
                                    self.world.name
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
                        0x40..=0x43 | 0x61 | PORT_TIMER => {
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
                                s.timer_write_syscalls_max =
                                    s.timer_write_syscalls_max.max(run_syscalls);
                                // PV only, so counter witnesses don't move:
                                // deliver the due tick before the write.
                                if self.pv_clock {
                                    self.expire_timers()?;
                                }
                            }
                            if port == PORT_TIMER {
                                let ticks = u16::from_le_bytes([v, hi]);
                                self.pit.program(ticks, now);
                            } else {
                                self.pit.write(port, v, now);
                            }
                            // A legacy write supersedes the last page request.
                            if self.page_timer.enabled {
                                self.page_timer.seen = self.page_get(PAGE_TIMER_DEADLINE);
                            }
                            timer_latency(
                                &mut self.pit,
                                &mut self.pit_exact,
                                self.timer_rng.as_mut(),
                                self.timer_jitter,
                                self.exact_timer_ppm,
                            );
                        }
                        RTC_INDEX => self.rtc_index = v & 0x7f,
                        PORT_DEBUGCON => self.com1.transmit(v)?,
                        PORT_EXIT => {
                            self.guest_exit_code = Some(v);
                            self.world.state = State::Exited(v);
                            self.record(b'X', v as u64);
                            self.set_stepping(false)?;
                            return Ok(Stop::Exited);
                        }
                        BARRIER_PORT if self.barrier => {
                            if self.world.parked || self.entropy_read != 0 {
                                return Err(format!(
                                    "{}: launch barrier reached twice",
                                    self.world.name
                                ));
                            }
                            self.world.parked = true;
                            self.record(b'B', 0);
                            self.set_stepping(false)?;
                            // Finish the port write now, so the machine is
                            // parked in a state a fork can save and restore.
                            self.complete_io()?;
                            return Ok(Stop::Barrier);
                        }
                        PORT_INIT_ERROR => {
                            self.guest_init_error = Some(v);
                            self.world.state = State::Exited(v);
                            self.record(b'E', v as u64);
                            self.set_stepping(false)?;
                            return Ok(Stop::Exited);
                        }
                        _ => {}
                    }
                    self.capture_output();
                    if self.world.observe_console && self.com1.out.len() > console_before {
                        // Judge::Rollback discarded the exit before reaching here.
                        // Console ports have zero charge; the sink and pending KVM
                        // response are committed once at this deterministic exit.
                        self.world.committed_console = Some((after_io, v));
                        control_stop = true;
                        if let Some(u) = self.undo.as_mut() {
                            u.valid = false;
                        }
                    }
                    self.record(b'O', (port as u64) << 8 | v as u64);
                    self.serial_edges();
                    self.pending_io = pending;
                    if charge > 0 {
                        if let Some(u) = self.undo.as_mut() {
                            u.valid = false;
                        }
                    }
                    if control_stop {
                        self.set_stepping(false)?;
                        return Ok(Stop::Control);
                    }
                }
                Ok(VcpuExit::IoIn(port, data)) => {
                    self.stats.pio += 1;
                    let charge = io_charge(port, false);
                    advance_time(self.world.time, charge, u64::MAX)?;
                    // SAFETY: `run_ptr` is the vCPU's live kvm_run mapping, and
                    // this is a KVM_EXIT_IO exit, so the `io` union member is set.
                    let descriptor = unsafe { (*run_ptr).__bindgen_anon_1.io };
                    match port {
                        PORT_TIME_LO => {
                            let t = self.world.time.max(advance_time(self.tsc_latch, 1, hard)?);
                            self.tsc_latch = t;
                            self.stats.time_by[Cause::ClockReads as usize] += t - self.world.time;
                            self.world.time = t;
                            data.copy_from_slice(&(t as u32).to_le_bytes()[..data.len()]);
                        }
                        PORT_TIME_HI => data.copy_from_slice(
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
                                COM1..=COM1_LAST => self.com1.read(port - COM1),
                                COM2..=COM2_LAST => self.com2.read(port - COM2),
                                0x20 | 0x21 | 0xa0 | 0xa1 => self.pic.read(port),
                                0x40..=0x42 | 0x61 => self.pit.read(port, now),
                                RTC_DATA => rtc_read(self.rtc_index),
                                _ => 0xff,
                            };
                            data.fill(v);
                        }
                    }
                    let pending = if charge > 0 {
                        Some(PendingIo::new(
                            self.world.time,
                            port,
                            false,
                            descriptor.size,
                            descriptor.count,
                            data.to_vec(),
                        )?)
                    } else {
                        None
                    };
                    self.record(b'i', port as u64);
                    self.serial_edges();
                    self.pending_io = pending;
                    if charge > 0 {
                        if let Some(u) = self.undo.as_mut() {
                            u.valid = false;
                        }
                    }
                }
                Ok(VcpuExit::MmioRead(addr, data)) => {
                    self.stats.mmio += 1;
                    let v = if (CHANNEL_MMIO..CHANNEL_MMIO + 0x1000).contains(&addr) {
                        self.channel
                            .as_mut()
                            .map(|channel| channel.read(addr - CHANNEL_MMIO))
                            .unwrap_or(0)
                    } else if (BLK_MMIO..BLK_MMIO + 0x1000).contains(&addr) {
                        self.blk
                            .as_mut()
                            .map(|b| b.read(addr - BLK_MMIO))
                            .unwrap_or(0)
                    } else {
                        match self.net.as_mut() {
                            Some(n) if (NET_MMIO..NET_MMIO + 0x1000).contains(&addr) => {
                                n.read(addr - NET_MMIO)
                            }
                            _ => 0,
                        }
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
                    if (CHANNEL_MMIO..CHANNEL_MMIO + 0x1000).contains(&addr) {
                        let memory = self.mem.as_mut_slice();
                        if let Some(channel) = self.channel.as_mut() {
                            let output = channel.write(memory, addr - CHANNEL_MMIO, v)?;
                            self.take_dma();
                            if output.irq {
                                self.pic.raise(CHANNEL_IRQ);
                            }
                            if self.channel_output(&output.bytes)? {
                                self.set_stepping(false)?;
                                return Ok(Stop::Control);
                            }
                        }
                    }
                    if (BLK_MMIO..BLK_MMIO + 0x1000).contains(&addr) {
                        let m = self.mem.as_mut_slice();
                        if let Some(b) = self.blk.as_mut() {
                            let done = b.write(m, addr - BLK_MMIO, v)?;
                            self.take_dma();
                            if done.completed > 0 {
                                // The disk changed outside guest RAM: no rollback
                                // may cross this point.
                                if let Some(u) = self.undo.as_mut() {
                                    u.valid = false;
                                }
                            }
                            // Each request, in completion order: the trace shows
                            // where every FLUSH falls among the writes.
                            for (kind, sector, status) in done.log {
                                self.record(
                                    b'd',
                                    (kind as u64) << 56 | (status as u64) << 48 | sector,
                                );
                            }
                            if done.irq {
                                self.pic.raise(BLK_IRQ);
                            }
                        }
                    }
                    if (NET_MMIO..NET_MMIO + 0x1000).contains(&addr) {
                        if let Some(n) = self.net.as_mut() {
                            let out = n.write_at(
                                self.mem.as_mut_slice(),
                                addr - NET_MMIO,
                                v,
                                self.world.time.max(self.page.get(PAGE_LAST)),
                            );
                            self.take_dma();
                            let out = out?;
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
                    if deferred && self.poll_net(frames, true)? {
                        self.note_skew(Site::Sent);
                        return Ok(Stop::Sent);
                    }
                    if self.vcpu.get_kvm_run().if_flag == 0 {
                        self.world.state = State::Exited(0xfe);
                        return Ok(Stop::Exited);
                    }
                    // A guest may publish buffers and halt without a kick.
                    if self.poll_net(frames, true)? {
                        self.note_skew(Site::Sent);
                        return Ok(Stop::Sent);
                    }
                    self.world.state = State::Halted;
                    return Ok(Stop::Halted);
                }
                Ok(VcpuExit::IrqWindowOpen) => {
                    self.stats.window_exits += 1;
                }
                Ok(VcpuExit::Shutdown) => {
                    self.world.state = State::Exited(0xff);
                    self.set_stepping(false)?;
                    return Ok(Stop::Exited);
                }
                Ok(other) => return Err(format!("{}: unexpected exit {other:?}", self.world.name)),
                Err(e) => return Err(format!("{}: KVM_RUN: {e}", self.world.name)),
            }
        }
    }

    /// Hashes of guest RAM and the vCPU state a checkpoint restores, by part:
    /// general and control registers with segment bases, FPU/XSAVE, XCRs,
    /// pending exception/interrupt/NMI state, syscall/TLS MSRs, the shared time
    /// page, RAM and any retained disk. The digest is the hash of the parts.
    /// Not covered: other MSRs, debug registers and other VMM device models.
    pub fn state_digest(&mut self) -> Result<(String, Vec<(&'static str, String)>), String> {
        let hex = |h: Sha256| format!("{:x}", h.finalize());
        let e = |part: &str, err: kvm_ioctls::Error| format!("state digest {part}: {err}");
        let mut parts = Vec::new();
        let r = self.vcpu.get_regs().map_err(|x| e("regs", x))?;
        let s = self.vcpu.get_sregs().map_err(|x| e("sregs", x))?;
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
        for w in self.vcpu.get_xsave().map_err(|x| e("xsave", x))?.region {
            h.update(w.to_le_bytes());
        }
        parts.push(("xsave", hex(h)));
        let mut h = Sha256::new();
        let xcrs = self.vcpu.get_xcrs().map_err(|x| e("xcrs", x))?;
        for x in &xcrs.xcrs[..xcrs.nr_xcrs as usize] {
            h.update(x.xcr.to_le_bytes());
            h.update(x.value.to_le_bytes());
        }
        parts.push(("xcrs", hex(h)));
        // Event fields count only while live: KVM keeps the last exception's
        // vector/error code and the last interrupt's vector after delivery, so
        // they differ after a rollback without any guest-visible effect.
        let ev = self.vcpu.get_vcpu_events().map_err(|x| e("events", x))?;
        let exception = if ev.exception.injected != 0 || ev.exception.pending != 0 {
            format!(
                "{}/{}/{}/{}/{}",
                ev.exception.injected,
                ev.exception.pending,
                ev.exception.nr,
                ev.exception.has_error_code,
                ev.exception.error_code
            )
        } else {
            "none".into()
        };
        let interrupt = if ev.interrupt.injected != 0 {
            format!("{}/{}", ev.interrupt.nr, ev.interrupt.soft)
        } else {
            "none".into()
        };
        parts.push((
            "events",
            format!(
                "exc:{exception} int:{interrupt} shadow:{} nmi:{}/{}/{}",
                ev.interrupt.shadow, ev.nmi.injected, ev.nmi.pending, ev.nmi.masked,
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
        let mut msrs =
            Msrs::from_entries(&entries).map_err(|x| format!("state digest msrs: {x:?}"))?;
        let n = self.vcpu.get_msrs(&mut msrs).map_err(|x| e("msrs", x))?;
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
            parts.push(("disk", b.disk().digest()));
        }
        let mut all = Sha256::new();
        for (_, d) in &parts {
            all.update(d.as_bytes());
        }
        Ok((hex(all), parts))
    }

    /// Headerless little-endian u64 per 4 KiB guest RAM page, in PFN order.
    /// Each u64 is the first 8 SHA-256 bytes interpreted little-endian.
    /// Evidence only: reads RAM without changing it or consuming RNG draws.
    pub fn page_hashes(&mut self) -> Vec<u8> {
        ram_page_hashes(self.memory())
    }
}

fn ram_page_hashes(memory: &[u8]) -> Vec<u8> {
    let zero = [0u8; 4096];
    let zero_hash = Sha256::digest(zero);
    let mut out = Vec::with_capacity(memory.len().div_ceil(4096) * 8);
    for page in memory.chunks(4096) {
        // Verify the bytes, rather than inferring zeroes from a dirty log.
        // A short final page has a different digest even when it is all zero.
        let hash = if page == zero {
            zero_hash
        } else {
            Sha256::digest(page)
        };
        out.extend_from_slice(&hash[..8]);
    }
    out
}

#[test]
fn ram_page_hashes_match_full_hashes_for_zero_changed_and_short_pages() {
    let mut memory = vec![0u8; 4096 * 5 + 17];
    for changed in [false, true, false] {
        memory[4096] = u8::from(changed);
        memory[4096 * 4 - 1] = u8::from(changed);
        let before = memory.clone();
        let expected: Vec<u8> = memory
            .chunks(4096)
            .flat_map(|page| Sha256::digest(page)[..8].to_vec())
            .collect();
        assert_eq!(ram_page_hashes(&memory), expected);
        assert_eq!(memory, before);
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
        fn prepare(
            &mut self,
            _: &mut Machine,
            _: u64,
            _: u64,
            _: bool,
        ) -> Result<Prepared, String> {
            Ok(Prepared::Run)
        }
        fn start(&mut self, _: u64) -> Result<u64, String> {
            Ok(0)
        }
        fn read(&mut self, _: u64) -> Result<u64, String> {
            Ok(0)
        }
        fn inject_guard(&self) -> u64 {
            INJECT_GUARD
        }
        fn judge(&mut self, _: Judge<'_>, _: &Run) -> Result<Verdict, String> {
            Ok(Verdict::Advance { legal: true })
        }
    }

    #[test]
    #[cfg_attr(
        not(feature = "kvm-tests"),
        ignore = "requires KVM on a Linux x86-64 host"
    )]
    fn event_idx_polls_on_entry_slice_boundary_and_halt_without_kicks() {
        let kvm = Kvm::new().unwrap();
        for site in ["entry", "slice", "bounded_slice", "halt", "charged_io"] {
            for (event_idx, deferred) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let case = format!("site={site} event_idx={event_idx} deferred={deferred}");
                // Publish a head, then yield to the slice end, halt, or start
                // charged port I/O. Entry starts with an already posted head.
                let mut code = vec![0xc7, 0x05, 0x02, 0x20, 0x08, 0x00, 1, 0, 0, 0];
                if site == "slice" || site == "bounded_slice" {
                    code.extend_from_slice(&[0xb0, 1, 0xba, 0x2c, 0x05, 0, 0, 0xee]);
                }
                if site == "charged_io" {
                    code.extend_from_slice(&[0xba, 0x30, 0x05, 0, 0, 0xee]);
                }
                code.push(0xf4);
                let mut kernel = vec![0; 1024];
                kernel[0x1f1] = 1;
                kernel.extend(code);
                let spec = BootSpec {
                    kernel: &kernel,
                    initrd: &[],
                    cmdline: String::new(),
                    mem: INITRD_ADDR + 0x100000,
                    rng_seed: [0; 32],
                    mac: Some([2, 0, 0, 0, 0, 1]),
                    control_channel: false,
                    disk: None,
                    undo_margin: 0,
                    barrier: false,
                    pv_clock: true,
                    page_timer: false,
                };
                let mut m = Machine::new(&kvm, "net-poll", &spec).unwrap();
                let mut r = m.vcpu.get_regs().unwrap();
                r.rflags = 0x202;
                m.vcpu.set_regs(&r).unwrap();
                let memory = m.mem.as_mut_slice();
                let net = m.net.as_mut().unwrap();
                net.set_deferred_tx(deferred);
                for (off, v) in [
                    (0x020, (1 << 5) | if event_idx { 1 << 29 } else { 0 }),
                    (0x024, 1),
                    (0x020, 1),
                    (0x070, 15),
                    (0x030, 1),
                    (0x038, 1),
                    (0x080, 0x81000),
                    (0x090, 0x82000),
                    (0x0a0, 0x83000),
                    (0x044, 1),
                ] {
                    net.write(memory, off, v).unwrap();
                }
                memory[0x81000..0x81008].copy_from_slice(&0x84000u64.to_le_bytes());
                memory[0x81008..0x8100c].copy_from_slice(&17u32.to_le_bytes());
                memory[0x8400c..0x84011].copy_from_slice(b"frame");
                if site == "entry" {
                    memory[0x82002..0x82004].copy_from_slice(&1u16.to_le_bytes());
                }
                let mut frames = Vec::new();
                let (limit, early) = if site == "bounded_slice" {
                    (1_000_000, 100_000)
                } else {
                    (500, 0)
                };
                let stop = m
                    .run_until(&mut Natural, limit, early, &mut frames)
                    .unwrap();
                if site == "bounded_slice" {
                    assert_eq!(
                        m.time,
                        if deferred {
                            seedbox_engine_core::virtio_network::TX_POLL_NS
                        } else {
                            limit - early
                        },
                        "{case}"
                    );
                }
                if event_idx || deferred {
                    assert_eq!(stop, Stop::Sent, "{case}");
                    assert_eq!(frames, [b"frame".to_vec()], "{case}");
                    // used->idx is at used + 2; used + 12 is avail_event
                    // for this one-entry queue. Only one head was posted.
                    assert_eq!(
                        u16::from_le_bytes(m.memory()[0x83002..0x83004].try_into().unwrap()),
                        1,
                        "{case}"
                    );
                } else {
                    assert_eq!(
                        stop,
                        if site == "slice" || site == "bounded_slice" || site == "charged_io" {
                            Stop::Reached
                        } else {
                            Stop::Halted
                        },
                        "{case}"
                    );
                    assert!(frames.is_empty(), "{case}");
                }
                let net = m.net.as_ref().unwrap();
                assert_eq!(net.stats().doorbell_exits, 0, "{case}");
                assert_eq!(
                    net.stats().tx_frames,
                    u64::from(event_idx || deferred),
                    "{case}"
                );
                assert_eq!(
                    net.stats().tx_polled_at_boundary,
                    u64::from((event_idx || deferred) && site != "entry"),
                    "{case}"
                );
                assert!(net.dirty.is_empty(), "{case}"); // Machine adopted all DMA writes.
            }
        }
    }
    #[test]
    #[cfg_attr(
        not(feature = "kvm-tests"),
        ignore = "requires KVM on a Linux x86-64 host"
    )]
    fn pv_stop_adopts_the_guest_clock() {
        let kvm = Kvm::new().unwrap();
        for arm in [false, true] {
            // Read the clock 5 ms ahead of the VMM, optionally arm one PIT
            // tick through port 0x530, halt. The stop's time, which the world
            // plans the next limit from, and any deadline follow that clock.
            let mut code = vec![0xc7, 0x05, 0x08, 0x00, 0x00, 0xd0];
            code.extend_from_slice(&5_000_000u32.to_le_bytes());
            if arm {
                code.extend_from_slice(&[0xb8, 1, 0, 0, 0, 0xba, 0x30, 0x05, 0, 0, 0x66, 0xef]);
            }
            code.push(0xf4);
            let mut kernel = vec![0; 1024];
            kernel[0x1f1] = 1;
            kernel.extend(code);
            let spec = BootSpec {
                kernel: &kernel,
                initrd: &[],
                cmdline: String::new(),
                mem: INITRD_ADDR + 0x100000,
                rng_seed: [0; 32],
                mac: None,
                control_channel: false,
                disk: None,
                undo_margin: 0,
                barrier: false,
                pv_clock: true,
                page_timer: false,
            };
            let mut m = Machine::new(&kvm, "pv-stop", &spec).unwrap();
            let mut frames = Vec::new();
            let mut stop = m
                .run_until(&mut Natural, 10_000_000, 0, &mut frames)
                .unwrap();
            while stop == Stop::Reached {
                stop = m
                    .run_until(&mut Natural, 10_000_000, 0, &mut frames)
                    .unwrap();
            }
            assert!(m.time >= 5_000_000, "arm={arm} time {}", m.time);
            assert!(
                m.pit.deadline.is_none_or(|d| d > 5_000_000),
                "arm={arm} {:?}",
                m.pit.deadline
            );
        }
    }
    #[test]
    #[cfg_attr(
        not(feature = "kvm-tests"),
        ignore = "requires KVM on a Linux x86-64 host"
    )]
    fn deferred_tx_completes_a_pv_spin_without_halt_kick_or_interrupts() {
        use super::pv::PvClock;
        use std::time::Duration;
        let kvm = Kvm::new().unwrap();
        for (event_idx, page_timer) in [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut first = None;
            for ms in [2, 7] {
                let case = format!("site=pv_spin event_idx={event_idx} page_timer={page_timer} deferred=true watchdog_ms={ms}");
                // Publish TX, then busy-wait for used->idx with interrupts off.
                // The stable cycle is serviced at its canonical point; never
                // service at the host-time watchdog or a probe landing.
                let mut code = vec![
                    0xc7, 0x05, 0x02, 0x20, 0x08, 0x00, 1, 0, 0, 0, 0x66, 0x83, 0x3d, 0x02, 0x30,
                    0x08, 0, 0, 0x74, 0xf6, 0xf4,
                ];
                if page_timer {
                    // Store clock progress and an arm before publishing TX.
                    let mut stores = Vec::new();
                    for (address, value) in [
                        (0xd0000008u32, 12300u32),
                        (0xd0000070, 40000),
                        (0xd0000074, 0),
                    ] {
                        stores.extend_from_slice(&[0xc7, 0x05]);
                        stores.extend_from_slice(&address.to_le_bytes());
                        stores.extend_from_slice(&value.to_le_bytes());
                    }
                    stores.extend(code);
                    code = stores;
                }
                let mut kernel = vec![0; 1024];
                kernel[0x1f1] = 1;
                kernel.extend(code);
                let spec = BootSpec {
                    kernel: &kernel,
                    initrd: &[],
                    cmdline: String::new(),
                    mem: INITRD_ADDR + 0x100000,
                    rng_seed: [0; 32],
                    mac: Some([2, 0, 0, 0, 0, 1]),
                    control_channel: false,
                    disk: None,
                    undo_margin: 0,
                    barrier: false,
                    pv_clock: true,
                    page_timer,
                };
                let mut m = Machine::new(&kvm, "tx-spin", &spec).unwrap();
                m.abi = ABI_PAGE_TIMER;
                m.enable_page_timer();
                let memory = m.mem.as_mut_slice();
                let net = m.net.as_mut().unwrap();
                net.set_deferred_tx(true);
                for (off, v) in [
                    (0x020, if event_idx { 1 << 29 } else { 0 }),
                    (0x070, 15),
                    (0x030, 1),
                    (0x038, 1),
                    (0x080, 0x81000),
                    (0x090, 0x82000),
                    (0x0a0, 0x83000),
                    (0x044, 1),
                ] {
                    net.write(memory, off, v).unwrap();
                }
                memory[0x81000..0x81008].copy_from_slice(&0x84000u64.to_le_bytes());
                memory[0x81008..0x8100c].copy_from_slice(&17u32.to_le_bytes());
                memory[0x8400c..0x84011].copy_from_slice(b"frame");
                memory[0x82000..0x82002].copy_from_slice(&1u16.to_le_bytes()); // NO_INTERRUPT
                memory[0x82006..0x82008].copy_from_slice(&1u16.to_le_bytes()); // used_event
                let mut tb = PvClock::new(Duration::from_millis(ms)).unwrap();
                tb.max_false_alarms = 10;
                let mut frames = Vec::new();
                assert_eq!(
                    m.run_until(&mut tb, 1_000_000, 0, &mut frames).unwrap(),
                    Stop::Sent,
                    "{case}"
                );
                assert_eq!(frames, [b"frame".to_vec()], "{case}");
                assert_eq!(m.time, if page_timer { 12300 } else { 0 }, "{case}");
                assert_eq!(m.pit.deadline, page_timer.then_some(40000), "{case}");
                assert_eq!(m.net.as_ref().unwrap().stats().doorbell_exits, 0, "{case}");
                assert_eq!(
                    m.run_until(&mut tb, 1_000_000, 0, &mut frames).unwrap(),
                    Stop::Exited,
                    "{case}"
                );
                let evidence = (m.time, m.exit_witness(), m.state_digest().unwrap());
                if let Some(want) = &first {
                    assert_eq!(&evidence, want, "{case}");
                } else {
                    first = Some(evidence);
                }
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
        let value = if port == 0x530 { 1u32 } else { 0x1234u32 };
        let mut code = vec![0xb8];
        code.extend_from_slice(&value.to_le_bytes());
        code.push(0xba);
        code.extend_from_slice(&(port as u32).to_le_bytes());
        if port == 0x530 {
            code.extend_from_slice(&[0x66, 0xef]);
        } else {
            code.push(if write { 0xee } else { 0xec });
        }
        code.extend_from_slice(&[
            0x85, 0xc0, 0x74, 0x00, 0xc7, 0x05, 0x00, 0x00, 0x08, 0x00, 0x78, 0x56, 0x34, 0x12,
            0xf4,
        ]);
        let mut kernel = vec![0u8; 1024];
        kernel[0x1f1] = 1;
        kernel.extend(code);
        let spec = BootSpec {
            kernel: &kernel,
            initrd: &[],
            cmdline: String::new(),
            mem: INITRD_ADDR + 0x100000,
            rng_seed: [0; 32],
            mac: None,
            control_channel: false,
            disk: None,
            undo_margin: undo,
            barrier: false,
            pv_clock: false,
            page_timer: false,
        };
        let mut machine = Machine::new(kvm, "io-probe", &spec).unwrap();
        let mut r = machine.vcpu.get_regs().unwrap();
        r.rflags = 0x202;
        machine.vcpu.set_regs(&r).unwrap();
        machine.pit.write(0x43, 0x34, 0);
        machine.pit.program(0x1234, 0);
        machine.pit.write(0x43, 0, 0);
        machine.pic.set_masks(0);
        machine
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn runtime_checkpoint_keeps_unpaid_io_without_repeating_its_effect() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        for undo in [0, 256] {
            for (port, write) in [(0x40, false), (0x43, true), (0x530, true)] {
                let mut m = probe(&kvm, port, write, undo);
                let mut frames = Vec::new();
                assert_eq!(
                    m.run_until(&mut counter, 500, 0, &mut frames).unwrap(),
                    Stop::Reached
                );
                let pending = m.pending_io.clone();
                assert!(pending.is_some());
                let saved = m.snapshot().unwrap();
                let finish = |m: &mut Machine, counter: &mut Counter| {
                    let mut frames = Vec::new();
                    while !matches!(
                        m.run_until(counter, 10_000, 0, &mut frames).unwrap(),
                        Stop::Halted | Stop::Exited
                    ) {}
                    (
                        m.exit_witness(),
                        m.state_digest().unwrap().0,
                        m.time,
                        m.branches,
                    )
                };
                let want = finish(&mut m, &mut counter);
                let mut fresh = probe(&kvm, port, write, undo);
                fresh.restore(&saved).unwrap();
                assert_eq!(fresh.pending_io, pending);
                assert_eq!(
                    finish(&mut fresh, &mut counter),
                    want,
                    "port={port:#x}, write={write}, undo={undo}"
                );
            }
        }
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM on a Linux host")]
    fn page_capture_has_documented_format_and_preserves_ram_witness_state() {
        let kvm = Kvm::new().unwrap();
        let mut m = probe(&kvm, 0x530, true, 0);
        let before = (
            m.exit_witness(),
            m.state_digest().unwrap(),
            m.memory().to_vec(),
        );
        let pages = m.page_hashes();
        assert_eq!(pages.len(), m.mem.len() / 4096 * 8);
        for (pfn, page) in before.2.chunks(4096).enumerate() {
            let hash = u64::from_le_bytes(Sha256::digest(page)[..8].try_into().unwrap());
            assert_eq!(&pages[pfn * 8..pfn * 8 + 8], &hash.to_le_bytes());
        }
        assert_eq!(
            (
                m.exit_witness(),
                m.state_digest().unwrap(),
                m.memory().to_vec()
            ),
            before
        );
    }

    fn check_port(kvm: &Kvm, counter: &mut Counter, port: u16, write: bool) {
        let mut m = probe(kvm, port, write, 0);
        let initial = m.vcpu.get_regs().unwrap();
        assert_eq!(initial.rip, 0x100000);
        assert_eq!(initial.rsp, 0x90000);
        assert_ne!(initial.rflags & 0x200, 0);
        let mut frames = Vec::new();
        assert_eq!(
            m.run_until(counter, 500, 0, &mut frames).unwrap(),
            Stop::Reached
        );
        assert_eq!(m.time, 500);
        let live = m.vcpu.get_regs().unwrap();
        assert_eq!(live.rsp, initial.rsp);
        assert_ne!(live.rflags & 0x200, 0);
        assert_eq!(live.rip, 0x100000 + 10);
        assert_eq!(m.stats.kvm_n[1], 0, "unintended zero-memory boot execution");
        assert_eq!(m.branches, 0);
        let p = m.pending_io.clone().unwrap();
        assert_eq!(p.remaining_ns, 500);
        assert_eq!(p.port, port);
        assert_eq!(p.write, write);
        assert_eq!(p.width, if port == 0x530 { 2 } else { 1 });
        assert_eq!(p.count, 1);
        let clock = m.page_get(0);
        let window = m.page_get(2);
        let exits = m.stats.exits;
        let count = counter.read_fast().unwrap();
        assert_eq!(
            m.run_until(counter, 750, 0, &mut frames).unwrap(),
            Stop::Reached
        );
        assert_eq!(m.pending_io.as_ref().unwrap().remaining_ns, 250);
        assert_eq!(m.pending_io.as_ref().unwrap().bytes, p.bytes);
        assert_eq!(m.page_get(0), clock);
        assert_eq!(m.page_get(2), window);
        assert_eq!(m.stats.exits, exits);
        assert_eq!(m.stats.irqs, 0);
        assert_eq!(&m.memory()[0x80000..0x80004], &[0, 0, 0, 0]);
        // Completion changes the pending architectural I/O state once,
        // while neither counted guest branches nor virtual cost advance.
        assert_eq!(
            m.run_until(counter, 1000, 0, &mut frames).unwrap(),
            Stop::Reached
        );
        assert_eq!(m.time, 1000);
        assert_eq!(m.branches, 0);
        assert_eq!(counter.read_fast().unwrap(), count);
        assert!(m.pending_io.is_none());
        assert_eq!(m.stats.exits, exits);
        let complete = m.vcpu.get_regs().unwrap();
        assert_eq!(complete.rsp, initial.rsp);
        assert_ne!(complete.rflags & 0x200, 0);
        assert_eq!(
            complete.rip,
            0x100000 + 10 + if port == 0x530 { 2 } else { 1 }
        );
        assert_eq!(&m.memory()[0x80000..0x80004], &[0, 0, 0, 0]);
        if !write {
            assert_eq!(m.vcpu.get_regs().unwrap().rax as u8, p.bytes[0]);
        }
        if port == 0x40 && !write {
            assert_eq!(m.pit.read(0x40, 1000), 0x12);
        }
        assert_eq!(
            m.run_until(counter, 2000, 0, &mut frames).unwrap(),
            Stop::Halted
        );
        assert_eq!(m.branches, 1);
        assert_eq!(&m.memory()[0x80000..0x80004], &[0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    #[ignore = "first unchanged failed case; requires exclusive leased KVM lane"]
    fn first_case_preserves_boot_and_halts() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        check_port(&kvm, &mut counter, 0x40, true);
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn charged_io_completes_once_after_debt_with_no_guest_progress() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        let ports = [
            (0x40, true),
            (0x41, true),
            (0x42, true),
            (0x43, true),
            (0x61, true),
            (0x530, true),
            (0x40, false),
            (0x41, false),
            (0x42, false),
            (0x61, false),
        ];
        for (port, write) in ports {
            check_port(&kvm, &mut counter, port, write);
        }
        let mut pending = None;
        let mut completed = None;
        for undo in [0, 16, 256] {
            for exact in [false, true] {
                let mut m = probe(&kvm, 0x530, true, undo);
                m.pit_exact = exact;
                m.require_io_checkpoint = undo > 0;
                let mut frames = Vec::new();
                assert_eq!(
                    m.run_until(&mut counter, 839, 0, &mut frames).unwrap(),
                    Stop::Reached
                );
                assert_eq!(m.time, 839);
                assert_eq!(m.pending_io.as_ref().unwrap().remaining_ns, 161);
                assert_eq!(m.pic.pending(), Some(0));
                assert_eq!(m.pit.deadline, Some(1678));
                assert_eq!(m.stats.irqs, 0);
                assert!(!m.undo.as_ref().is_some_and(|u| u.valid));
                if undo > 0 {
                    assert!(m.undo.as_ref().unwrap().stats.checkpoints > 0);
                }
                let checkpoint_count = m.undo.as_ref().map_or(0, |u| u.stats.checkpoints);
                let evidence = (m.exit_witness(), m.state_digest().unwrap());
                // Same registers/RAM, but pending response/debt must change
                // both the terminal exit witness and the named state part.
                let saved = m.pending_io.clone();
                for data_change in [false, true] {
                    let p = m.pending_io.as_mut().unwrap();
                    if data_change {
                        p.bytes[0] ^= 1;
                    } else {
                        p.remaining_ns -= 1;
                    }
                    let changed = (m.exit_witness(), m.state_digest().unwrap());
                    assert_ne!(changed.0, evidence.0);
                    assert_ne!(changed.1 .0, evidence.1 .0);
                    for ((name, hash), (other_name, other_hash)) in
                        evidence.1 .1.iter().zip(&changed.1 .1)
                    {
                        assert_eq!(name, other_name);
                        if *name == "pending_io" {
                            assert_ne!(hash, other_hash);
                        } else {
                            assert_eq!(hash, other_hash);
                        }
                    }
                    m.pending_io = saved.clone();
                }
                if let Some(want) = &pending {
                    assert_eq!(&evidence, want, "pending undo={undo} exact={exact}");
                } else {
                    pending = Some(evidence);
                }
                assert_eq!(
                    m.run_until(&mut counter, 1000, 0, &mut frames).unwrap(),
                    Stop::Reached
                );
                let evidence = (m.exit_witness(), m.state_digest().unwrap());
                if let Some(want) = &completed {
                    assert_eq!(&evidence, want, "completed undo={undo} exact={exact}");
                } else {
                    completed = Some(evidence);
                }
                assert_eq!(m.time, 1000);
                assert_eq!(m.branches, 0);
                assert!(m.pending_io.is_none());
                assert_eq!(
                    m.undo.as_ref().map_or(0, |u| u.stats.checkpoints),
                    checkpoint_count
                );
            }
        }
    }

    /// A 32-bit loop of 1000 conditional branches, optionally followed by an
    /// uncharged OUT to port 0x80 (a natural exit), with interrupts masked.
    fn loop_probe(kvm: &Kvm, natural_exit: bool, undo: u64) -> Machine {
        // outer: mov ebx,1000; inner: dec ebx; jnz inner; [out 0x80,al]; jmp outer
        let mut code = vec![0xbb, 0xe8, 0x03, 0x00, 0x00, 0x4b, 0x75, 0xfd];
        if natural_exit {
            code.extend_from_slice(&[0xe6, 0x80]);
        }
        let back = -(code.len() as i8 + 2);
        code.extend_from_slice(&[0xeb, back as u8]);
        let mut kernel = vec![0u8; 1024];
        kernel[0x1f1] = 1;
        kernel.extend(code);
        let spec = BootSpec {
            kernel: &kernel,
            initrd: &[],
            cmdline: String::new(),
            mem: INITRD_ADDR + 0x100000,
            rng_seed: [0; 32],
            mac: None,
            control_channel: false,
            disk: None,
            undo_margin: undo,
            barrier: false,
            pv_clock: false,
            page_timer: false,
        };
        let mut machine = Machine::new(kvm, "stop-probe", &spec).unwrap();
        let mut r = machine.vcpu.get_regs().unwrap();
        r.rflags = 0x2;
        machine.vcpu.set_regs(&r).unwrap();
        machine.pic.set_masks(0xffff);
        machine
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
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
            let mut kernel = vec![0u8; 1024];
            kernel[0x1f1] = 1;
            kernel.extend(&code);
            let spec = BootSpec {
                kernel: &kernel,
                initrd: &[],
                cmdline: String::new(),
                mem: INITRD_ADDR + 0x100000,
                rng_seed: [0; 32],
                mac: None,
                control_channel: false,
                disk: None,
                undo_margin: undo,
                barrier: false,
                pv_clock: false,
                page_timer: true,
            };
            let mut m = Machine::new(&kvm, "page-rearm", &spec).unwrap();
            let mut r = m.vcpu.get_regs().unwrap();
            r.rflags = 0x202;
            m.vcpu.set_regs(&r).unwrap();
            m.abi = ABI_PAGE_TIMER;
            m.enable_page_timer();
            m.tolerance = 200_000;
            m.pit.program_deadline(Some(100_000)); // D = 100k, hard = 300k, exit at ~280k
            m.pic.set_masks(0xffff);
            assert_eq!(
                m.run_until(&mut counter, 1_000_000, 0, &mut Vec::new())
                    .unwrap(),
                Stop::Halted
            );
            m.pic.set_masks(0);
            let got = (m.time, m.pic.pending(), m.pit.deadline, m.exit_witness());
            if let Some(want) = &want {
                assert_eq!(&got, want, "undo={undo}");
            } else {
                want = Some(got);
            }
        }
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn refined_counter_approaches_preserve_ram_and_exact_stops() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        let mut want = None;
        // A 4096 margin leaves more than 32 branches after a landing with any
        // plausible skid, so that run must refine at least once.
        for undo in [0, 16, 32, 64, 128, 256, 1024, 4096] {
            let mut m = loop_probe(&kvm, false, undo);
            // Increment a RAM word on every branch. A refinement that kept
            // registers but lost RAM would count some iterations twice.
            // outer: mov ebx,1000; inner: inc dword [0x80000]; dec ebx; jnz inner; jmp outer
            let code = [
                0xbb, 0xe8, 0x03, 0x00, 0x00, 0xff, 0x05, 0x00, 0x00, 0x08, 0x00, 0x4b, 0x75, 0xf7,
                0xeb, 0xf0,
            ];
            m.memory()[0x100000..0x100000 + code.len()].copy_from_slice(&code);
            let word = |m: &mut Machine| {
                u32::from_le_bytes(m.memory()[0x80000..0x80004].try_into().unwrap())
            };
            let start = word(&mut m);
            m.adaptive_margin = true;
            assert_eq!(
                m.run_until(&mut counter, 1_000_000, 0, &mut Vec::new())
                    .unwrap(),
                Stop::Reached
            );
            assert_eq!(m.time, 1_000_000);
            assert_eq!(m.branches, 1_000_000);
            if undo == 4096 {
                assert!(
                    m.undo.as_ref().unwrap().stats.checkpoints >= 2,
                    "undo={undo}: no refined approach"
                );
            }
            let got = (
                word(&mut m).wrapping_sub(start),
                m.exit_witness(),
                m.state_digest().unwrap().0,
            );
            if let Some(want) = &want {
                assert_eq!(&got, want, "undo={undo}");
            } else {
                want = Some(got);
            }
        }
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn stops_at_natural_exits_or_exact_hard_counts_without_yields() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        // (exits, limit, early): no natural exit, so an exact counted stop at
        // the hard target; an exit every 1000 branches with soft below hard,
        // so the first exit at or after soft; and soft == hard with an exit
        // exactly on the hard count, which must stop before that exit.
        let cases = [
            (false, 300_000, 150_000),
            (true, 300_000, 150_000),
            (true, 150_000, 0),
        ];
        for (exits, limit, early) in cases {
            let mut want = None;
            for undo in [0, 16, 256] {
                let mut m = loop_probe(&kvm, exits, undo);
                // The fixed margin can be shorter than a skid in this loop.
                m.adaptive_margin = true;
                let mut frames = Vec::new();
                assert_eq!(
                    m.run_until(&mut counter, limit, early, &mut frames)
                        .unwrap(),
                    Stop::Reached
                );
                let what = format!("exits={exits} limit={limit} early={early} undo={undo}");
                assert_eq!(m.page_get(2), 0, "{what}: a yield window was requested");
                assert_eq!(m.stats.yields, 0);
                match (exits, early) {
                    (false, _) => assert_eq!(m.time, limit, "{what}"),
                    (true, 0) => {
                        assert_eq!(m.time, limit, "{what}");
                        assert_eq!(
                            m.stats.pio,
                            limit / 1000 - 1,
                            "{what}: ran the exit at the hard count"
                        );
                    }
                    (true, _) => {
                        assert!(
                            m.time >= limit - early && m.time < limit - early + 1001,
                            "{what} time={}",
                            m.time
                        );
                        assert_eq!(
                            m.stats.precise_stops, 0,
                            "{what}: counted stop despite a natural exit"
                        );
                    }
                }
                let rip = m.vcpu.get_regs().unwrap().rip;
                let evidence = (m.time, rip, m.exit_witness(), m.state_digest().unwrap().0);
                if let Some(want) = &want {
                    assert_eq!(&evidence, want, "{what}");
                } else {
                    want = Some(evidence);
                }
            }
        }
    }

    fn halt_snapshot(m: &mut Machine, stage: &str) {
        let regs = m.vcpu.get_regs().unwrap();
        let sregs = m.vcpu.get_sregs().unwrap();
        let (sync, if_flag, exit_reason) = {
            let run = m.vcpu.get_kvm_run();
            (unsafe { run.s.regs.regs }, run.if_flag, run.exit_reason)
        };
        let marker = m.memory()[0x80000..0x80004].to_vec();
        let page: Vec<_> = (0..PAGE_FIELDS).map(|i| m.page_get(i)).collect();
        eprintln!(
            "{}",
            serde_json::json!({"stage":stage,"state":format!("{:?}",m.state),"guest_exit":m.guest_exit_code,"guest_init_error":m.guest_init_error,"arch":{"rip":regs.rip,"rflags":regs.rflags,"rax":regs.rax,"rdx":regs.rdx,"rsp":regs.rsp,"cr0":sregs.cr0,"cs_base":sregs.cs.base},"sync":{"rip":sync.rip,"rflags":sync.rflags,"rax":sync.rax,"rdx":sync.rdx},"run_if_flag":if_flag,"raw_run_exit_reason":exit_reason,"pending_io":m.pending_io.as_ref().map(|p|serde_json::json!({"remaining_ns":p.remaining_ns,"port":p.port,"write":p.write,"width":p.width,"count":p.count,"bytes":p.bytes})),"time_ns":m.time,"branches":m.branches,"page":page,"marker":marker,"exits":m.stats.exits,"halts":m.stats.halts,"irqs":m.stats.irqs,"kvm_exits":m.stats.kvm_n})
        );
    }

    #[test]
    #[ignore = "single manager-authorized diagnostic of unchanged halt case"]
    fn diagnose_first_halt_case() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        let mut m = probe(&kvm, 0x40, true, 0);
        let mut kernel = vec![0u8; 1024];
        kernel[0x1f1] = 1;
        kernel.extend_from_slice(&m.memory()[0x100000..0x100000 + 26]);
        eprintln!("fixture_kernel_sha256={:x}", Sha256::digest(&kernel));
        halt_snapshot(&mut m, "before_first_run");
        let mut frames = Vec::new();
        for (limit, stage) in [
            (500, "after_partial_500"),
            (750, "after_partial_750"),
            (1000, "after_completion_1000"),
        ] {
            let stop = m.run_until(&mut counter, limit, 0, &mut frames);
            eprintln!("limit={limit} stop={stop:?}");
            halt_snapshot(&mut m, stage);
            assert_eq!(stop.unwrap(), Stop::Reached);
        }
        let stop = m.run_until(&mut counter, 2000, 0, &mut frames);
        eprintln!("limit=2000 stop={stop:?}");
        halt_snapshot(&mut m, "after_final_resume");
        assert_eq!(stop.unwrap(), Stop::Halted);
    }

    /// Pause with 161 ns of I/O cost owed and a periodic timer armed: while
    /// paused nothing runs and nothing is paid. Resumed later, the missed
    /// ticks fire once, late, in phase, and the debt is paid from the resume.
    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn a_pause_keeps_owed_io_cost_and_resumes_in_timer_phase() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        let mut want = None;
        for undo in [0, 16, 256] {
            let mut m = probe(&kvm, 0x530, true, undo);
            let mut frames = Vec::new();
            assert_eq!(
                m.run_until(&mut counter, 839, 0, &mut frames).unwrap(),
                Stop::Reached
            );
            assert_eq!(m.pending_io.as_ref().unwrap().remaining_ns, 161);
            assert_eq!(m.pit.deadline, Some(1678));
            m.pause().unwrap();
            assert!(m.pause().is_err());
            let rip = m.vcpu.get_regs().unwrap().rip;
            m.resume(5000).unwrap();
            assert!(m.resume(5000).is_err());
            assert_eq!(m.time, 5000);
            assert_eq!(m.pit.deadline, Some(1678 + 4 * 839), "undo={undo}");
            assert_eq!(
                m.pending_io.as_ref().unwrap().remaining_ns,
                161,
                "debt kept while paused"
            );
            assert_eq!(
                (m.branches, m.vcpu.get_regs().unwrap().rip, m.stats.irqs),
                (0, rip, 0)
            );
            assert_eq!(
                m.run_until(&mut counter, 5100, 0, &mut frames).unwrap(),
                Stop::Reached
            );
            assert_eq!(m.pending_io.as_ref().unwrap().remaining_ns, 61);
            assert_eq!(
                m.run_until(&mut counter, 5161, 0, &mut frames).unwrap(),
                Stop::Reached
            );
            assert!(m.pending_io.is_none());
            assert_eq!(m.branches, 0);
            assert_eq!(
                m.run_until(&mut counter, 7000, 0, &mut frames).unwrap(),
                Stop::Halted
            );
            assert_eq!(m.branches, 1);
            assert_eq!(&m.memory()[0x80000..0x80004], &[0x78, 0x56, 0x34, 0x12]);
            let evidence = (m.time, m.exit_witness(), m.state_digest().unwrap().0);
            if let Some(want) = &want {
                assert_eq!(&evidence, want, "undo={undo}");
            } else {
                want = Some(evidence);
            }
        }
    }

    /// Pause once the debt is paid but before the guest runs on: resuming
    /// later runs exactly what an unpaused machine runs, just later.
    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn a_pause_with_no_debt_runs_on_unchanged_after_resume() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        for undo in [0, 256] {
            let mut m = probe(&kvm, 0x61, false, undo);
            let mut frames = Vec::new();
            assert_eq!(
                m.run_until(&mut counter, 1000, 0, &mut frames).unwrap(),
                Stop::Reached
            );
            assert!(m.pending_io.is_none());
            let rax = m.vcpu.get_regs().unwrap().rax;
            m.pause().unwrap();
            m.resume(3000).unwrap();
            assert_eq!(m.vcpu.get_regs().unwrap().rax, rax);
            assert_eq!(
                m.run_until(&mut counter, 5000, 0, &mut frames).unwrap(),
                Stop::Halted
            );
            assert_eq!(m.branches, 1);
            assert!(m.time >= 3000 && m.time < 5000);
            assert_eq!(&m.memory()[0x80000..0x80004], &[0x78, 0x56, 0x34, 0x12]);
        }
    }
}

#[cfg(test)]
mod fork_tests {
    use super::*;

    #[test]
    fn page_entropy_carries_all_32_bytes_in_order() {
        let words: [u32; ENTROPY_WORDS] = std::array::from_fn(|i| 0x1111_1111 * (i as u32 + 1));
        let bytes: Vec<u8> = page_entropy(&words)
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let want: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert_eq!(bytes, want);
    }

    /// The field indices here must match struct seedbox_time_page in
    /// kernel/seedbox.h, which the guest kernel is built from.
    #[test]
    fn time_page_fields_match_the_kernel_header() {
        let header = include_str!("../../../kernel/seedbox.h");
        let body = header
            .split("struct seedbox_time_page {")
            .nth(1)
            .unwrap()
            .split("};")
            .next()
            .unwrap();
        let mut index = 0;
        let mut fields = std::collections::BTreeMap::new();
        for line in body
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("u64 "))
        {
            let decl = line["u64 ".len()..].split(';').next().unwrap();
            let (name, count) = match decl.split_once('[') {
                Some((n, c)) => (n, c.trim_end_matches(']').parse::<usize>().unwrap()),
                None => (decl, 1),
            };
            fields.insert(name.to_string(), (index, count));
            index += count;
        }
        assert_eq!(fields["tsc"], (PAGE_TSC, 1));
        assert_eq!(fields["last"], (PAGE_LAST, 1));
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
        let abi: u64 = header
            .lines()
            .find_map(|l| l.strip_prefix("#define SEEDBOX_ABI\t"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(abi, ABI_PAGE_TIMER);
        for (name, port) in [
            ("TIME_LO", PORT_TIME_LO),
            ("TIME_HI", PORT_TIME_HI),
            ("TIME_READY", PORT_TIME_READY),
            ("YIELD", PORT_YIELD),
            ("TIMER", PORT_TIMER),
        ] {
            let define = format!("#define SEEDBOX_PORT_{name}\t{port:#x}\t");
            assert!(header.contains(&define), "{define}");
        }
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
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
                assert_eq!(
                    m.page_get(PAGE_CAPABILITIES),
                    if active { CAP_PAGE_TIMER } else { 0 }
                );
                assert_eq!(
                    m.page_get(PAGE_TIMER_DEADLINE),
                    if active { TIMER_DISARMED } else { 0 }
                );
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
        assert_eq!(
            m.next_deadline(),
            10000,
            "unadopted disarm cannot change idle wake time"
        );
        m.sync_page_timer().unwrap();
        assert_eq!(m.next_deadline(), TIMER_DISARMED);
        assert_eq!(m.pit.deadline, None);
    }

    // These tests create a vCPU but never enter the guest or open a perf event.
    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn abi_below_3_time_forks_do_not_write_extensions_or_add_reseed_witnesses() {
        let kvm = Kvm::new().unwrap();
        for abi in 0..ABI_PAGE_RESEED {
            let mut m = probe_machine(&kvm, 0, false);
            assert!(
                (6..14).all(|i| m.page_get(i) == 0),
                "counter boot, ABI {abi}"
            );
            for i in 6..14 {
                m.page_set(i, 100 + i as u64);
            }
            m.abi = abi;
            m.park().unwrap();
            let mut witness = m.witness.clone();
            witness.update([b'R']);
            witness.update(m.branches.to_le_bytes());
            witness.update(m.time.to_le_bytes());
            witness.update(m.time.to_le_bytes());
            // Pre-K2 exit_witness also covers pending I/O: none after park().
            witness.update(b"pending_io");
            witness.update([0]);
            assert_eq!(
                m.release(m.time, [7; ENTROPY_WORDS]).unwrap(),
                Reseed::Unsupported
            );
            assert!(
                (6..14).all(|i| m.page_get(i) == 100 + i as u64),
                "ABI {abi}"
            );
            assert_eq!(
                m.exit_witness(),
                format!("{:x}", witness.finalize()),
                "ABI {abi}"
            );
        }
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn a_time_fork_writes_only_entropy_and_a_barrier_writes_none() {
        let kvm = Kvm::new().unwrap();
        for barrier in [false, true] {
            let mut m = probe_machine(&kvm, 0, barrier);
            assert!((6..14).all(|i| m.page_get(i) == 0), "counter boot");
            m.abi = ABI_PAGE_RESEED;
            for i in 0..14 {
                m.page_set(i, 100 + i as u64);
            }
            let before: Vec<_> = (0..14).map(|i| m.page_get(i)).collect();
            // Both release paths require a parked world, before any page write.
            assert!(m.release(m.time, [7; ENTROPY_WORDS]).is_err());
            assert_eq!((0..14).map(|i| m.page_get(i)).collect::<Vec<_>>(), before);
            m.park().unwrap();
            let how = m.release(m.time, [7; ENTROPY_WORDS]).unwrap();
            assert_eq!(
                how,
                if barrier {
                    Reseed::Barrier
                } else {
                    Reseed::Page
                }
            );
            assert_eq!(
                (0..9).map(|i| m.page_get(i)).collect::<Vec<_>>(),
                before[..9]
            );
            if barrier {
                assert_eq!(
                    (9..14).map(|i| m.page_get(i)).collect::<Vec<_>>(),
                    before[9..14]
                );
            } else {
                assert_eq!(m.page_get(PAGE_ENTROPY_GEN), before[9] + 1);
                assert_eq!(
                    (10..14).map(|i| m.page_get(i)).collect::<Vec<_>>(),
                    page_entropy(&[7; ENTROPY_WORDS])
                );
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
        let mut code = vec![
            0xb8, 1, 0, 0, 0, 0xba, 0xf8, 0, 0, 0, 0xef, 0xed, 0xa3, 0x00, 0x00, 0x08, 0x00,
        ];
        code.extend_from_slice(&[0xb9, 0xe8, 0x03, 0, 0, 0x49, 0x75, 0xfd]);
        code.extend_from_slice(&[0xc7, 0x05, 0x00, 0x10, 0x08, 0x00, 0xad, 0xde, 0, 0, 0xf4]);
        let mut kernel = vec![0u8; 1024];
        kernel[0x1f1] = 1;
        kernel.extend(code);
        let spec = BootSpec {
            kernel: &kernel,
            initrd: &[],
            cmdline: String::new(),
            mem: INITRD_ADDR + 0x100000,
            rng_seed: [0; 32],
            mac: None,
            control_channel: false,
            disk: None,
            undo_margin: undo,
            barrier,
            pv_clock: false,
            page_timer: false,
        };
        Machine::new(kvm, "fork-probe", &spec).unwrap()
    }

    fn to_barrier(m: &mut Machine, counter: &mut Counter) {
        let mut frames = Vec::new();
        assert_eq!(
            m.run_until(counter, 1_000_000, 0, &mut frames).unwrap(),
            Stop::Barrier
        );
        assert!(m.parked);
    }

    fn released(
        m: &mut Machine,
        counter: &mut Counter,
        word: u32,
    ) -> (String, String, u64, u64, Vec<u8>) {
        let at = m.time + 10;
        m.release(at, [word; ENTROPY_WORDS]).unwrap();
        let mut frames = Vec::new();
        loop {
            match m.run_until(counter, 10_000_000, 0, &mut frames).unwrap() {
                Stop::Exited | Stop::Halted => break,
                Stop::Reached => {}
                other => panic!("unexpected stop {other:?}"),
            }
        }
        assert_eq!(&m.memory()[0x80000..0x80004], &word.to_le_bytes());
        assert_eq!(&m.memory()[0x81000..0x81004], &[0xad, 0xde, 0, 0]);
        (
            m.exit_witness(),
            m.state_digest().unwrap().0,
            m.time,
            m.branches,
            m.memory()[0x80000..0x80004].to_vec(),
        )
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn a_restored_fork_matches_a_cold_run_and_seeds_differ() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        for undo in [0, 256] {
            let mut cold = barrier_machine(&kvm, undo);
            to_barrier(&mut cold, &mut counter);
            let want = released(&mut cold, &mut counter, 7);
            assert!(want.3 >= 1000, "spin loop branches counted");

            let mut m = barrier_machine(&kvm, undo);
            to_barrier(&mut m, &mut counter);
            let snapshot = m.snapshot().unwrap();
            assert_eq!(
                released(&mut m, &mut counter, 7),
                want,
                "first seed after taking a snapshot, undo={undo}"
            );
            let pages = m.restore(&snapshot).unwrap();
            assert!(pages >= 2, "marker pages copied back, undo={undo}");
            assert!(m.parked);
            assert_eq!(&m.memory()[0x80000..0x80004], &[0, 0, 0, 0]);
            let other = released(&mut m, &mut counter, 9);
            // Both seeds take the same path, and the event witness records
            // port reads without their values, so only the state differs.
            assert_eq!(other.0, want.0);
            assert_ne!(other.1, want.1);
            assert_eq!(other.4, 9u32.to_le_bytes());
            m.restore(&snapshot).unwrap();
            assert_eq!(
                released(&mut m, &mut counter, 7),
                want,
                "seed restored from the fork, undo={undo}"
            );
        }
    }

    /// A machine released later than it parked (another machine parked
    /// later, or a release skew), with its timer due in between: the timer
    /// fires late at the release instead of leaving the next slice a hard
    /// stop behind its clock.
    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn a_release_past_an_armed_timer_fires_it_and_runs_on() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        for undo in [0, 256] {
            let mut m = barrier_machine(&kvm, undo);
            to_barrier(&mut m, &mut counter);
            let parked = m.time;
            // Periodic channel 0 armed at the barrier, as a guest's tick is.
            m.pit.write(0x43, 0x34, parked);
            m.pit.program(12, parked);
            let period = m.pit.deadline.unwrap() - parked;
            m.release(parked + 4 * period + period / 2, [7; ENTROPY_WORDS])
                .unwrap();
            // Four ticks fell due while parked; they fire as one late IRQ0
            // (masked in this probe) and the next is armed past the release.
            assert_eq!(m.pit.deadline, Some(parked + 5 * period), "undo={undo}");
            let mut frames = Vec::new();
            while !matches!(
                m.run_until(&mut counter, parked + 10_000_000, 0, &mut frames)
                    .unwrap(),
                Stop::Exited | Stop::Halted
            ) {}
            assert_eq!(&m.memory()[0x81000..0x81004], &[0xad, 0xde, 0, 0]);
        }
    }

    /// Fork at a simulated time inside the spin loop instead of at the guest's
    /// barrier (the port write and read are then ordinary unknown ports).
    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn a_fork_at_a_simulated_time_matches_a_cold_run() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        let finish = |m: &mut Machine, counter: &mut Counter| {
            let at = m.time + 10;
            m.release(at, [0; ENTROPY_WORDS]).unwrap();
            let mut frames = Vec::new();
            while !matches!(
                m.run_until(counter, 10_000_000, 0, &mut frames).unwrap(),
                Stop::Exited | Stop::Halted
            ) {}
            (
                m.exit_witness(),
                m.state_digest().unwrap().0,
                m.time,
                m.branches,
            )
        };
        for undo in [0, 256] {
            let mut cold = probe_machine(&kvm, undo, false);
            let mut frames = Vec::new();
            assert_eq!(
                cold.run_until(&mut counter, 500, 0, &mut frames).unwrap(),
                Stop::Reached
            );
            assert_eq!(cold.time, 500);
            cold.park().unwrap();
            let want = finish(&mut cold, &mut counter);

            let mut m = probe_machine(&kvm, undo, false);
            assert_eq!(
                m.run_until(&mut counter, 500, 0, &mut frames).unwrap(),
                Stop::Reached
            );
            m.park().unwrap();
            let snapshot = m.snapshot().unwrap();
            assert_eq!(
                finish(&mut m, &mut counter),
                want,
                "first run after a snapshot, undo={undo}"
            );
            m.restore(&snapshot).unwrap();
            assert_eq!(m.time, 500);
            assert_eq!(
                finish(&mut m, &mut counter),
                want,
                "restored fork, undo={undo}"
            );
        }
    }

    /// A seed that crashed a machine leaves a later incarnation in a new VM,
    /// so the next seed restores into a freshly built machine that holds no
    /// fork point: a full copy, after which it holds one and later restores
    /// take the cheap path again. Every restore must match the cold run.
    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn a_fork_restores_into_a_freshly_built_machine() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        for undo in [0, 256] {
            let mut cold = barrier_machine(&kvm, undo);
            to_barrier(&mut cold, &mut counter);
            let want = released(&mut cold, &mut counter, 7);

            let mut m = barrier_machine(&kvm, undo);
            to_barrier(&mut m, &mut counter);
            let snapshot = m.snapshot().unwrap();
            drop(m);
            let mut fresh = barrier_machine(&kvm, undo);
            if undo > 0 {
                assert!(!fresh.undo.as_ref().unwrap().holds_fork_point());
            }
            let pages = fresh.restore(&snapshot).unwrap();
            assert_eq!(
                pages,
                (fresh.mem.len() / 4096) as u64,
                "full copy, undo={undo}"
            );
            assert!(fresh.parked);
            assert_eq!(
                released(&mut fresh, &mut counter, 7),
                want,
                "restored into a fresh machine, undo={undo}"
            );
            if undo > 0 {
                assert!(fresh.undo.as_ref().unwrap().holds_fork_point());
            }
            let pages = fresh.restore(&snapshot).unwrap();
            if undo > 0 {
                assert!(
                    pages < (fresh.mem.len() / 4096) as u64,
                    "only written pages copied back"
                );
            }
            assert_eq!(
                released(&mut fresh, &mut counter, 7),
                want,
                "second restore, undo={undo}"
            );
        }
    }

    fn to_halt(m: &mut Machine, counter: &mut Counter, limit: u64) {
        let mut frames = Vec::new();
        while !matches!(
            m.run_until(counter, limit, 0, &mut frames).unwrap(),
            Stop::Exited | Stop::Halted
        ) {}
    }

    /// Crash a machine and boot its next incarnation from the same inputs:
    /// fresh RAM, the program runs again from its first instruction at the
    /// reboot time, and the logs and witness carry across.
    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn a_crashed_machine_reboots_fresh_from_its_boot_inputs() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        let mut want = None;
        for undo in [0, 256] {
            let mut old = probe_machine(&kvm, undo, false);
            to_halt(&mut old, &mut counter, 1_000_000);
            assert_eq!(&old.memory()[0x81000..0x81004], &[0xad, 0xde, 0, 0]);
            let first = (old.branches, old.exit_witness());
            old.com1.out.extend_from_slice(b"first boot\n");
            old.syscall_ns = 937;
            let crash = old.world.time + 100;
            let mut new = probe_machine(&kvm, undo, false);
            new.succeed(&mut old, crash, 1009, None).unwrap();
            assert!(new.down && !new.paused);
            assert_eq!(
                new.syscall_ns, 937,
                "the next incarnation keeps the PV syscall cost"
            );
            assert_eq!((new.incarnation, new.time), (1, crash));
            assert_eq!(&new.memory()[0x80000..0x80004], &[0, 0, 0, 0]);
            assert_eq!(&new.memory()[0x81000..0x81004], &[0, 0, 0, 0]);
            assert_eq!(new.com1.out, b"first boot\n");
            assert!(old.com1.out.is_empty());
            drop(old);
            assert!(new.boot_at(crash - 1).is_err());
            new.boot_at(crash + 1000).unwrap();
            assert!(new.boot_at(crash + 1000).is_err());
            let rip = new.vcpu.get_regs().unwrap().rip;
            assert_eq!(
                rip, 0x100000,
                "the new incarnation starts at the kernel entry"
            );
            to_halt(&mut new, &mut counter, crash + 1_000_000);
            assert_eq!(&new.memory()[0x81000..0x81004], &[0xad, 0xde, 0, 0]);
            assert_eq!(new.branches, first.0, "same program, counted from zero");
            assert!(new.time > crash + 1000);
            assert_ne!(
                new.exit_witness(),
                first.1,
                "the witness covers both incarnations"
            );
            let evidence = (
                first,
                new.time,
                new.exit_witness(),
                new.state_digest().unwrap().0,
            );
            if let Some(want) = &want {
                assert_eq!(&evidence, want, "undo={undo}");
            } else {
                want = Some(evidence);
            }
        }
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn a_reboot_applies_seeded_sector_survival_and_keeps_the_default_witness() {
        let kvm = Kvm::new().unwrap();
        for fault_model in [
            crate::disk::DiskFaultModel {
                sector_bytes: crate::disk::SECTOR,
                survival_probability: 0.0,
            },
            crate::disk::DiskFaultModel {
                sector_bytes: crate::disk::SECTOR,
                survival_probability: 0.5,
            },
        ] {
            let run = |seed| {
                let mut old = probe_machine(&kvm, 256, false);
                let mut disk = Disk::new(None, Some(32))
                    .unwrap()
                    .with_fault_model(fault_model);
                disk.write(0, &[b'A'; 32 * 512]).unwrap();
                disk.flush();
                disk.write(0, &[b'B'; 32 * 512]).unwrap();
                let expected = disk.crashed_seeded(seed, &old.world.name, old.world.incarnation);
                old.blk = Some(VirtioBlk::new(disk));
                let mut new = probe_machine(&kvm, 256, false);
                new.blk = Some(VirtioBlk::new(
                    Disk::new(None, Some(32))
                        .unwrap()
                        .with_fault_model(fault_model),
                ));
                // The pre-policy successor witness: old witness followed by C.
                let mut witness = new.witness.clone();
                witness.update(old.exit_witness().as_bytes());
                witness.update([b'C']);
                witness.update(new.branches.to_le_bytes());
                witness.update(1000u64.to_le_bytes());
                witness.update(1000u64.to_le_bytes());
                witness.update(b"pending_io");
                witness.update(new.pending_io_bytes());
                assert_eq!(
                    new.succeed(&mut old, 1000, seed, None).unwrap(),
                    Some(expected.1)
                );
                assert_eq!(new.exit_witness(), format!("{:x}", witness.finalize()));
                let disk = new.blk.as_ref().unwrap().disk();
                assert_eq!(disk.digest(), expected.0.digest());
                assert_eq!(disk.volatile_sectors(), 0);
                assert_eq!(new.blk.as_ref().unwrap().stats().writes, 0);
                (new.exit_witness(), disk.digest())
            };
            let a = run(1009);
            assert_eq!(a, run(1009));
            let b = run(1010);
            assert_eq!(a.0, b.0);
            if fault_model.survival_probability > 0.0 {
                assert_ne!(a.1, b.1);
            } else {
                assert_eq!(a.1, b.1);
            }
        }
    }

    /// A crash keeps exactly the flushed disk writes: the next incarnation
    /// boots with the stable layer, a fresh virtio-blk transport, and none of
    /// the volatile writes. A fork restore puts the whole disk back.
    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn a_reboot_keeps_flushed_disk_writes_only() {
        let kvm = Kvm::new().unwrap();
        let with_disk =
            |m: &mut Machine| m.blk = Some(VirtioBlk::new(Disk::new(None, Some(8)).unwrap()));
        let mut counter = Counter::new().unwrap();
        let mut old = probe_machine(&kvm, 256, false);
        with_disk(&mut old);
        to_halt(&mut old, &mut counter, 1_000_000);
        let snap = old.snapshot().unwrap();
        let disk = old.blk.as_mut().unwrap().disk_mut();
        disk.write(0, &[b'A'; 512]).unwrap();
        disk.flush();
        disk.write(0, &[b'B'; 512]).unwrap();
        disk.write(1, &[b'C'; 512]).unwrap();
        let mut new = probe_machine(&kvm, 256, false);
        with_disk(&mut new);
        assert_eq!(new.succeed(&mut old, 1000, 1009, None).unwrap(), Some(2));
        let disk = new.blk.as_ref().unwrap().disk();
        let mut buf = [0u8; 1024];
        disk.read(0, &mut buf).unwrap();
        assert_eq!((buf[0], buf[512]), (b'A', 0));
        assert_eq!((disk.stable_sectors(), disk.volatile_sectors()), (1, 0));
        assert_eq!(
            new.blk.as_ref().unwrap().stats().writes,
            0,
            "a fresh transport"
        );
        assert_eq!(
            old.blk.as_ref().unwrap().disk().volatile_sectors(),
            2,
            "the old incarnation is untouched"
        );
        old.restore(&snap).unwrap();
        assert_eq!(
            old.blk.as_ref().unwrap().disk().digest(),
            Disk::new(None, Some(8)).unwrap().digest()
        );
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
    fn runtime_checkpoint_restores_pv_fields_and_seeded_syscall_cost() {
        let kvm = Kvm::new().unwrap();
        let mut m = probe_machine(&kvm, 0, false);
        m.pv_clock = true;
        m.syscall_ns = 937;
        m.abi = ABI_PAGE_RESEED;
        for i in 6..14 {
            m.page_set(i, 100 + i as u64);
        }
        let saved = m.snapshot().unwrap();
        m.syscall_ns = 1000;
        for i in 6..14 {
            m.page_set(i, 0);
        }
        m.restore(&saved).unwrap();
        assert_eq!(m.syscall_ns, 937);
        assert_eq!(
            (6..14).map(|i| m.page_get(i)).collect::<Vec<_>>(),
            (6..14).map(|i| 100 + i as u64).collect::<Vec<_>>()
        );
        let pv_fields = (6..9).map(|i| m.page_get(i)).collect::<Vec<_>>();
        m.reseed_time_page([42; ENTROPY_WORDS]).unwrap();
        assert_eq!((6..9).map(|i| m.page_get(i)).collect::<Vec<_>>(), pv_fields);
        assert_eq!(m.page_get(9), 110);
        assert_eq!(
            (10..14).map(|i| m.page_get(i)).collect::<Vec<_>>(),
            page_entropy(&[42; ENTROPY_WORDS])
        );
    }

    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM")]
    fn runtime_reseed_publishes_page_without_advancing_or_unpausing() {
        let kvm = Kvm::new().unwrap();
        let mut m = probe_machine(&kvm, 0, false);
        m.abi = ABI_PAGE_RESEED;
        m.pause().unwrap();
        let saved = m.snapshot().unwrap();
        let before = m.published_entropy();
        m.reseed_time_page([42; ENTROPY_WORDS]).unwrap();
        assert!(m.paused);
        assert_eq!(m.time, 0);
        assert!(!m.parked);
        assert_eq!(m.page_get(PAGE_ENTROPY_GEN), 1);
        let page = page_entropy(&[42; ENTROPY_WORDS]);
        for (i, word) in page.into_iter().enumerate() {
            assert_eq!(m.page_get(PAGE_ENTROPY + i), word);
        }
        let branch = m.published_entropy();
        assert_ne!(branch, before);
        m.restore(&saved).unwrap();
        m.reseed_time_page([43; ENTROPY_WORDS]).unwrap();
        assert_ne!(m.published_entropy(), branch);
        m.restore(&saved).unwrap();
        m.reseed_time_page([42; ENTROPY_WORDS]).unwrap();
        assert_eq!(m.published_entropy(), branch);
        m.abi = 2;
        assert!(m
            .reseed_time_page([44; ENTROPY_WORDS])
            .unwrap_err()
            .contains("requires guest ABI 3"));
        m.abi = 0;
        m.reseed_time_page([45; ENTROPY_WORDS]).unwrap();
        assert_eq!(
            m.page_get(PAGE_ENTROPY_GEN),
            2,
            "pre-boot entropy is published too"
        );
    }
}

#[cfg(test)]
mod timed_log_tests {
    use super::*;

    fn output_machine(kvm: &Kvm, undo: u64) -> Machine {
        // Two console bytes and one management byte, with no intervening
        // conditional branch: all three must have the same committed time.
        let code = [
            0xb0, 0x00, 0xe6, 0xe9, 0xba, 0xf8, 0x02, 0, 0, 0xb0, 0xff, 0xee, 0xb0, b'A', 0xe6,
            0xe9, 0xbb, 0xe8, 0x03, 0, 0, 0x4b, 0x75, 0xfd, 0xb0, b'B', 0xe6, 0xe9, 0xb0, 0, 0xe6,
            0xf4,
        ];
        let mut kernel = vec![0; 1024];
        kernel[0x1f1] = 1;
        kernel.extend_from_slice(&code);
        let spec = BootSpec {
            kernel: &kernel,
            initrd: &[],
            cmdline: String::new(),
            mem: INITRD_ADDR + 0x100000,
            rng_seed: [0; 32],
            mac: None,
            control_channel: false,
            disk: None,
            undo_margin: undo,
            barrier: false,
            pv_clock: false,
            page_timer: false,
        };
        let mut m = Machine::new(kvm, "timed", &spec).unwrap();
        let mut r = m.vcpu.get_regs().unwrap();
        r.rflags = 2;
        m.vcpu.set_regs(&r).unwrap();
        m.pic.set_masks(0xffff);
        m
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn timed_logs_commit_order_and_observation_controls() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        let mut expected = None;
        for undo in [0, 16, 256] {
            for cap in [0, 2, 65_536] {
                let mut m = output_machine(&kvm, undo);
                m.com1.chunks.limit = Some(cap);
                m.com2.chunks.limit = Some(cap);
                let mut frames = Vec::new();
                // Force an exact counted boundary inside the branch loop.
                assert_eq!(
                    m.run_until(&mut counter, 500, 0, &mut frames).unwrap(),
                    Stop::Reached
                );
                assert_eq!(m.com1.out, b"\0A");
                assert_eq!(m.com2.out, b"\xff");
                if cap > 0 {
                    assert_eq!(
                        m.com1.chunks.records,
                        vec![[0, 0, 0, 0, 1], [0, 0, 2, 1, 1]]
                    );
                    assert_eq!(m.com2.chunks.records, vec![[0, 0, 1, 0, 1]]);
                }
                // Reading metadata and taking a full-state digest are observation only.
                let before = m.exit_witness();
                if cap > 0 {
                    let _ = m.state_digest().unwrap();
                }
                assert_eq!(before, m.exit_witness());
                assert_eq!(
                    m.run_until(&mut counter, 2000, 0, &mut frames).unwrap(),
                    Stop::Exited
                );
                assert_eq!(m.com1.out, b"\0AB");
                assert_eq!(m.com2.out, b"\xff");
                if cap > 2 {
                    assert_eq!(m.com1.chunks.records[2], [1000, 0, 3, 2, 1]);
                }
                let evidence = (
                    m.time,
                    m.exit_witness(),
                    m.state_digest().unwrap().0,
                    m.com1.out.clone(),
                    m.com2.out.clone(),
                );
                if let Some(want) = &expected {
                    assert_eq!(&evidence, want, "undo={undo} cap={cap}");
                } else {
                    expected = Some(evidence);
                }
            }
        }
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn timed_logs_survive_machine_incarnations() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        let mut old = output_machine(&kvm, 256);
        let mut frames = Vec::new();
        assert_eq!(
            old.run_until(&mut counter, 2000, 0, &mut frames).unwrap(),
            Stop::Exited
        );
        let mut new = output_machine(&kvm, 256);
        new.succeed(&mut old, 5000, 1009, None).unwrap();
        new.boot_at(5000).unwrap();
        assert_eq!(
            new.run_until(&mut counter, 7000, 0, &mut frames).unwrap(),
            Stop::Exited
        );
        assert_eq!(new.com1.out, b"\0AB\0AB");
        assert_eq!(new.com2.out, b"\xff\xff");
        assert_eq!(
            new.com1.chunks.records,
            vec![
                [0, 0, 0, 0, 1],
                [0, 0, 2, 1, 2],
                [5000, 1, 3, 3, 1],
                [5000, 1, 5, 4, 2]
            ]
        );
        assert_eq!(
            new.com2.chunks.records,
            vec![[0, 0, 1, 0, 1], [5000, 1, 4, 1, 1]]
        );
    }

    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn runtime_checkpoint_restores_timed_log_cursor_and_order() {
        let kvm = Kvm::new().unwrap();
        let mut counter = Counter::new().unwrap();
        let mut m = output_machine(&kvm, 256);
        let mut frames = Vec::new();
        m.run_until(&mut counter, 500, 0, &mut frames).unwrap();
        let saved = m.snapshot().unwrap();
        m.run_until(&mut counter, 2000, 0, &mut frames).unwrap();
        let want = (
            m.com1.out.clone(),
            m.com2.out.clone(),
            m.com1.chunks.records.clone(),
            m.com2.chunks.records.clone(),
            m.log_order,
        );
        for _ in 0..2 {
            let mut fresh = output_machine(&kvm, 256);
            fresh.restore(&saved).unwrap();
            assert_eq!(fresh.com1.chunks.seen, saved.com1.chunks.seen);
            assert_eq!(fresh.com2.chunks.seen, saved.com2.chunks.seen);
            fresh.run_until(&mut counter, 2000, 0, &mut frames).unwrap();
            assert_eq!(
                (
                    fresh.com1.out.clone(),
                    fresh.com2.out.clone(),
                    fresh.com1.chunks.records.clone(),
                    fresh.com2.chunks.records.clone(),
                    fresh.log_order
                ),
                want
            );
        }
    }

    // Inject a judge decision at a real KVM output exit to exercise the commit
    // gate deterministically. This is not evidence of actual PMU skid behavior.
    struct RejectOnce {
        counter: Counter,
        rollback: bool,
        first: bool,
    }
    impl Timebase for RejectOnce {
        const BRANCHES: bool = true;
        fn prepare(
            &mut self,
            m: &mut Machine,
            remaining: u64,
            soft: u64,
            legal: bool,
        ) -> Result<Prepared, String> {
            let prepared = self.counter.prepare(m, remaining, soft, legal)?;
            if self.first && self.rollback {
                m.checkpoint()?;
            }
            Ok(prepared)
        }
        fn start(&mut self, syscalls: u64) -> Result<u64, String> {
            self.counter.start(syscalls)
        }
        fn read(&mut self, _: u64) -> Result<u64, String> {
            if self.first && !self.rollback {
                Ok(self.counter.read()? + 1_000_000)
            } else {
                self.counter.read()
            }
        }
        fn inject_guard(&self) -> u64 {
            INJECT_GUARD
        }
        fn judge(&mut self, j: Judge<'_>, run: &Run) -> Result<Verdict, String> {
            if self.first {
                self.first = false;
                return Ok(if self.rollback {
                    assert!(j.undo_valid);
                    Verdict::Rollback
                } else {
                    Verdict::Overshoot
                });
            }
            self.counter.judge(j, run)
        }
    }
    #[test]
    #[cfg_attr(not(feature = "counter-tests"), ignore = "requires counter-tests")]
    fn timed_logs_commit_gate_rejection_and_rollback() {
        let kvm = Kvm::new().unwrap();
        for rollback in [false, true] {
            let mut m = output_machine(&kvm, 256);
            let mut tb = RejectOnce {
                counter: Counter::new().unwrap(),
                rollback,
                first: true,
            };
            let mut frames = Vec::new();
            let result = m.run_until(&mut tb, 2000, 0, &mut frames);
            if rollback {
                assert_eq!(result.unwrap(), Stop::Exited);
                assert!(m.undo.as_ref().unwrap().stats.rollbacks > 0);
                assert_eq!(m.com1.out, b"\0AB");
                assert_eq!(
                    m.com1.chunks.records,
                    vec![[0, 0, 0, 0, 1], [0, 0, 2, 1, 2]]
                );
                assert_eq!(m.com1.chunks.records[0], [0, 0, 0, 0, 1]);
            } else {
                assert!(result.unwrap_err().contains("overshoot"));
                assert!(m.com1.out.is_empty());
                assert!(m.com1.chunks.records.is_empty());
                assert_eq!(m.log_order, 0);
            }
        }
    }
}

#[cfg(test)]
mod committed_trigger_tests {
    use super::*;
    /// Force a real KVM UART candidate through the actual rollback branch.
    /// This is a commitment control, not a hardware counter qualification.
    struct DiscardFirst {
        prepared: bool,
        discarded: bool,
    }
    impl Timebase for DiscardFirst {
        const BRANCHES: bool = true;
        fn prepare(
            &mut self,
            m: &mut Machine,
            _: u64,
            _: u64,
            _: bool,
        ) -> Result<Prepared, String> {
            if !self.prepared {
                m.checkpoint()?;
                self.prepared = true;
            }
            Ok(Prepared::Run)
        }
        fn start(&mut self, _: u64) -> Result<u64, String> {
            Ok(0)
        }
        fn read(&mut self, _: u64) -> Result<u64, String> {
            Ok(0)
        }
        fn inject_guard(&self) -> u64 {
            INJECT_GUARD
        }
        fn judge(&mut self, _: Judge<'_>, _: &Run) -> Result<Verdict, String> {
            if !self.discarded {
                self.discarded = true;
                Ok(Verdict::Rollback)
            } else {
                Ok(Verdict::Advance { legal: true })
            }
        }
    }
    #[test]
    #[cfg_attr(not(feature = "kvm-tests"), ignore = "requires KVM")]
    fn committed_console_discards_rolled_back_candidate_before_matcher_feed() {
        let kvm = Kvm::new().unwrap();
        let mut kernel = vec![0; 1024];
        kernel[0x1f1] = 1;
        kernel.extend_from_slice(&[0xb0, b'A', 0xe6, 0xe9, 0xf4]);
        let spec = BootSpec {
            kernel: &kernel,
            initrd: &[],
            cmdline: String::new(),
            mem: INITRD_ADDR + 0x100000,
            rng_seed: [0; 32],
            mac: None,
            control_channel: false,
            disk: None,
            undo_margin: 256,
            barrier: false,
            pv_clock: false,
            page_timer: false,
        };
        let mut m = Machine::new(&kvm, "source", &spec).unwrap();
        m.observe_console = true;
        let mut tb = DiscardFirst {
            prepared: false,
            discarded: false,
        };
        let mut frames = Vec::new();
        assert_eq!(
            m.run_until(&mut tb, 10, 0, &mut frames).unwrap(),
            Stop::Control
        );
        assert!(tb.discarded);
        assert_eq!(m.undo.as_ref().unwrap().stats.rollbacks, 1);
        assert_eq!(m.com1.out, b"A");
        assert_eq!(m.committed_console.take(), Some((0, b'A')));
        // A trigger stop must retain exactly the accepted byte's timed chunk
        // and invalidate the checkpoint before returning control to the world.
        assert!(!m.undo.as_ref().unwrap().valid);
        assert_eq!(m.com1.chunks.records, vec![[0, 0, 0, 0, 1]]);
        assert_eq!(m.log_order, 1);
        assert_eq!(
            m.run_until(&mut tb, 10, 0, &mut frames).unwrap(),
            Stop::Exited
        );
        assert_eq!(m.committed_console, None);
        assert_eq!(m.com1.out, b"A");
        assert_eq!(m.com1.chunks.records, vec![[0, 0, 0, 0, 1]]);
        assert_eq!(m.log_order, 1);
    }
}

#[cfg(test)]
mod page_timer_tests {
    use super::*;

    #[test]
    fn page_deadline_is_picked_up_once_and_sentinel_disarms() {
        let mut page = PageTimer {
            enabled: true,
            seen: TIMER_DISARMED,
        };
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
        let mut page = PageTimer {
            enabled: true,
            seen: TIMER_DISARMED,
        };
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
            assert_eq!(
                pit.expire_through(now).unwrap(),
                port.expire_through(now).unwrap()
            );
            page.pickup(word, &mut pit, 0).unwrap();
            assert_eq!(pit.deadline, port.deadline);
        }
    }

    #[test]
    fn a_page_rearm_preserves_an_overdue_interrupt() {
        let mut page = PageTimer {
            enabled: true,
            seen: TIMER_DISARMED,
        };
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
